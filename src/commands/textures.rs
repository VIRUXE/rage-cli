use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rage_formats::{auto_format, encode_image, encode_texture, fit_max_size, image, parse_dds, parse_drawables,
                   parse_ytd, serialize_ytd, to_rgba_image, DrawableKind, EncodeFormat, ImageFormat, YtdTexture};
use rage_render::{compose_sheet, SheetItem, SheetOptions};

use crate::resources::{embedded_textures, extension_of, file_stem, load_drawables, load_texture_dictionary,
                       sanitize};
use crate::rpf::{Archive, GtaKeys};

#[derive(clap::Args)]
#[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
pub struct TexturesArgs {
    #[command(subcommand)]
    pub command: Option<TexturesCommand>,

    /// Path to the RPF archive, or a loose .ytd/.ydr/.ydd/.yft file
    #[arg(required = true)]
    pub archive: Option<PathBuf>,

    /// Name of a .ytd, .ydr, .ydd or .yft inside the archive (e.g. "vehicles.ytd");
    /// not needed for a loose file
    pub file: Option<String>,

    /// Output directory (default: file stem)
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,

    /// Image format: png, jpg, webp
    #[arg(short, long, default_value = "png")]
    pub format: ImageFormat,

    /// Cap the longest edge of each exported image (pixels)
    #[arg(long, value_name = "PX")]
    pub max_size: Option<u32>,

    /// Also write one labelled contact sheet of every texture
    #[arg(long)]
    pub sheet: bool,

    /// Write raw DDS files instead of images (old `ytd` behaviour)
    #[arg(long)]
    pub dds: bool,

    /// JPEG quality 1-100 (ignored for png/webp)
    #[arg(long, default_value = "90", value_parser = clap::value_parser!(u8).range(1..=100))]
    pub quality: u8,

    /// Contact-sheet cell size in pixels
    #[arg(long, default_value = "256", value_parser = clap::value_parser!(u32).range(32..=2048))]
    pub cell: u32,
}

#[derive(clap::Subcommand)]
pub enum TexturesCommand {
    /// Encode PNG/TGA/JPG/WebP/BMP images to DDS (BC1/BC3/BC4/BC5/BC7) with a full mip chain
    Encode(EncodeArgs),

    /// Build a .ytd texture dictionary from DDS and image files
    Build(BuildArgs),
}

/// Mip levels wanted: `auto` (a full chain) or a count.
#[derive(Clone, Copy, Debug)]
pub struct Mips(pub Option<u8>);

impl std::str::FromStr for Mips {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("auto") || s.eq_ignore_ascii_case("full") {
            return Ok(Mips(None));
        }
        match s.parse::<u8>() {
            Ok(n) if n >= 1 => Ok(Mips(Some(n))),
            _ => Err(format!("'{s}' is not 'auto' or a mip count from 1 to 255")),
        }
    }
}

fn parse_format(s: &str) -> Result<EncodeFormat, String> {
    EncodeFormat::parse(s).ok_or_else(|| format!("'{s}' is not one of bc1, bc3, bc4, bc5, bc7, rgba8"))
}

#[derive(clap::Args)]
pub struct EncodeArgs {
    /// Image files, directories (every image inside) or globs
    #[arg(required = true, value_name = "INPUT")]
    pub inputs: Vec<String>,

    /// Output .dds file (one input) or directory (default: next to each input)
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// bc1, bc3, bc4, bc5, bc7 or rgba8 (default: bc5 for *_n normal maps,
    /// bc3 when the image has alpha, bc1 otherwise)
    #[arg(short, long, value_name = "FORMAT", value_parser = parse_format)]
    pub format: Option<EncodeFormat>,

    /// Mip levels: "auto" for the full chain (down to 4x4), or a count
    #[arg(long, default_value = "auto")]
    pub mips: Mips,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    /// DDS or image files, directories or globs; each texture is named after its file stem
    #[arg(required = true, value_name = "INPUT")]
    pub inputs: Vec<String>,

    /// The .ytd to write
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,

    /// Start from an existing dictionary: inputs add to it, or replace entries of the same name
    #[arg(long, value_name = "YTD")]
    pub from: Option<PathBuf>,

    /// Format for images that need encoding (DDS inputs are stored as they are)
    #[arg(short, long, value_name = "FORMAT", value_parser = parse_format)]
    pub format: Option<EncodeFormat>,

    /// Mip levels for images that need encoding: "auto" or a count
    #[arg(long, default_value = "auto")]
    pub mips: Mips,
}

/// A texture together with the name it should be exported under.
struct Named<'a> {
    name: String,
    tex: &'a YtdTexture,
}

fn is_loose_resource(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    path.is_file() && (ext.eq_ignore_ascii_case("ytd") || DrawableKind::from_extension(ext).is_some())
}

fn textures_from_loose(path: &Path) -> Result<Vec<YtdTexture>> {
    let data = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("ytd") {
        return parse_ytd(&data).with_context(|| format!("failed to parse YTD {}", path.display()));
    }
    let kind = DrawableKind::from_extension(ext).expect("checked by is_loose_resource");
    let entries = parse_drawables(&data, kind).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(embedded_textures(&entries).into_iter().cloned().collect())
}

fn textures_from_archive(archive: &Archive, file: &str, keys: Option<&GtaKeys>) -> Result<Vec<YtdTexture>> {
    let ext = extension_of(file);

    if ext.eq_ignore_ascii_case("ytd") {
        return load_texture_dictionary(archive, file, keys);
    }

    if let Some(kind) = DrawableKind::from_extension(ext) {
        let entries = load_drawables(archive, file, kind, keys)?;
        return Ok(embedded_textures(&entries).into_iter().cloned().collect());
    }

    bail!("'{}' is not a .ytd, .ydr, .ydd or .yft file", file);
}

fn texture_name(tex: &YtdTexture) -> String {
    if tex.name.is_empty() {
        format!("0x{:08X}", tex.name_hash)
    } else {
        tex.name.clone()
    }
}

/// The per-texture line every texture command prints.
fn describe(name: &str, tex: &YtdTexture) -> String {
    format!(
        "  {} — {}x{}x{} {} {} mip(s) ({} bytes)",
        name, tex.width, tex.height, tex.depth, tex.format, tex.levels, tex.pixel_data.len(),
    )
}

pub fn run(args: &TexturesArgs, keys: Option<&GtaKeys>) -> Result<()> {
    match &args.command {
        Some(TexturesCommand::Encode(encode)) => return run_encode(encode),
        Some(TexturesCommand::Build(build)) => return run_build(build),
        None => {}
    }
    let archive_path = args.archive.as_deref().expect("required by clap");

    let (textures, stem) = match &args.file {
        None if is_loose_resource(archive_path) => {
            let stem = archive_path.file_stem().and_then(|s| s.to_str()).unwrap_or("textures").to_string();
            (textures_from_loose(archive_path)?, stem)
        }
        None => bail!(
            "{} is not a loose .ytd/.ydr/.ydd/.yft; to read inside an archive give the file name too",
            archive_path.display()
        ),
        Some(file) => {
            let archive = Archive::open(archive_path, keys)?;
            archive.require_keys(keys)?;
            (textures_from_archive(&archive, file, keys)?, file_stem(file))
        }
    };

    if textures.is_empty() {
        println!("No textures found");
        return Ok(());
    }

    let out_dir = args.output.clone().unwrap_or_else(|| PathBuf::from(&stem));
    std::fs::create_dir_all(&out_dir)?;

    let named: Vec<Named<'_>> = textures.iter().map(|tex| Named { name: texture_name(tex), tex }).collect();

    let mut exported = 0usize;
    let mut failed = 0usize;
    let mut sheet_items: Vec<(String, image::RgbaImage, &YtdTexture)> = Vec::new();

    for Named { name, tex } in &named {
        println!("{}", describe(name, tex));

        if args.dds {
            let dds_path = out_dir.join(format!("{}.dds", sanitize(name)));
            match std::fs::write(&dds_path, tex.to_dds()) {
                Ok(()) => exported += 1,
                Err(err) => {
                    eprintln!("warning: failed to write {}: {}", dds_path.display(), err);
                    failed += 1;
                }
            }
            continue;
        }

        let result: Result<()> = (|| {
            let mut img = to_rgba_image(tex)?;
            if let Some(max_size) = args.max_size {
                img = fit_max_size(img, max_size);
            }

            if args.sheet {
                sheet_items.push((name.clone(), img.clone(), *tex));
            }

            let encoded = encode_image(&img, args.format, args.quality)?;
            let out_path = out_dir.join(format!("{}.{}", sanitize(name), args.format.extension()));
            std::fs::write(&out_path, encoded)
                .with_context(|| format!("failed to write {}", out_path.display()))?;
            Ok(())
        })();

        match result {
            Ok(()) => exported += 1,
            Err(err) => {
                eprintln!("warning: failed to export '{}': {}", name, err);
                failed += 1;
            }
        }
    }

    if exported == 0 && failed > 0 {
        anyhow::bail!("failed to export any of the {} texture(s)", named.len());
    }

    if args.sheet {
        if args.dds {
            println!("note: --sheet is ignored with --dds");
        } else if !sheet_items.is_empty() {
            let items: Vec<SheetItem<'_>> = sheet_items
                .iter()
                .map(|(name, img, tex)| SheetItem {
                    label: format!("{} {}x{} {}", name, img.width(), img.height(), tex.format),
                    image: img,
                })
                .collect();

            let options = SheetOptions { cell: args.cell, ..Default::default() };
            let sheet = compose_sheet(&items, &options);
            let encoded = encode_image(&sheet, args.format, args.quality)?;
            let sheet_path = out_dir.join(format!("{}_sheet.{}", stem, args.format.extension()));
            std::fs::write(&sheet_path, encoded)
                .with_context(|| format!("failed to write {}", sheet_path.display()))?;
        }
    }

    println!("Exported {} texture(s) to {}", exported, out_dir.display());
    Ok(())
}

// ─── encode / build ───────────────────────────────────────────────────────────

const IMAGE_EXTENSIONS: &[&str] = &["png", "tga", "jpg", "jpeg", "webp", "bmp"];

fn has_extension(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// Expands files, directories and globs into the files with one of `exts`,
/// in a stable order without duplicates. A file named outright is taken
/// whatever its extension, so the caller can say what is wrong with it.
fn collect_inputs(inputs: &[String], exts: &[&str]) -> Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = Vec::new();
    let push = |p: PathBuf, found: &mut Vec<PathBuf>| {
        if !found.contains(&p) {
            found.push(p);
        }
    };
    for input in inputs {
        let path = Path::new(input);
        if path.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
                .with_context(|| format!("failed to list {}", path.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && has_extension(p, exts))
                .collect();
            entries.sort();
            if entries.is_empty() {
                eprintln!("warning: no {} files in {}", exts.join("/"), path.display());
            }
            for p in entries {
                push(p, &mut found);
            }
        } else if path.is_file() {
            push(path.to_path_buf(), &mut found);
        } else if input.contains(['*', '?', '[']) {
            let mut matched: Vec<PathBuf> = glob::glob(input)
                .with_context(|| format!("bad pattern '{input}'"))?
                .filter_map(|m| m.ok())
                .filter(|p| p.is_file())
                .collect();
            matched.sort();
            if matched.is_empty() {
                eprintln!("warning: '{input}' matched nothing");
            }
            for p in matched {
                push(p, &mut found);
            }
        } else {
            bail!("{input}: no such file or directory");
        }
    }
    if found.is_empty() {
        bail!("no input files");
    }
    Ok(found)
}

fn stem_of(path: &Path) -> String {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("texture").to_string()
}

/// Loads an image file and encodes it as a texture named after its stem.
fn encode_file(path: &Path, format: Option<EncodeFormat>, mips: Mips) -> Result<YtdTexture> {
    let img = image::open(path).with_context(|| format!("failed to open {}", path.display()))?.to_rgba8();
    let (w, h) = img.dimensions();
    let name = stem_of(path);
    let format = format.unwrap_or_else(|| auto_format(&name, img.as_raw()));
    encode_texture(&name, img.as_raw(), w, h, format, mips.0).with_context(|| format!("{}", path.display()))
}

fn run_encode(args: &EncodeArgs) -> Result<()> {
    let inputs = collect_inputs(&args.inputs, IMAGE_EXTENSIONS)?;

    let single_file_output = args.output.as_ref().is_some_and(|o| has_extension(o, &["dds"]));
    if single_file_output && inputs.len() > 1 {
        bail!("--output names a .dds file but there are {} inputs; give a directory instead", inputs.len());
    }
    if let Some(dir) = args.output.as_ref().filter(|_| !single_file_output) {
        std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }

    let mut written = 0usize;
    let mut failed = 0usize;
    for path in &inputs {
        if has_extension(path, &["dds"]) {
            println!("  {} — already DDS, skipped", path.display());
            continue;
        }
        let out_path = match &args.output {
            Some(o) if single_file_output => o.clone(),
            Some(dir) => dir.join(format!("{}.dds", stem_of(path))),
            None => path.with_extension("dds"),
        };
        match encode_file(path, args.format, args.mips) {
            Ok(tex) => {
                std::fs::write(&out_path, tex.to_dds()).with_context(|| format!("failed to write {}", out_path.display()))?;
                println!("{} -> {}", describe(&tex.name, &tex), out_path.display());
                written += 1;
            }
            Err(err) => {
                eprintln!("warning: {err:#}");
                failed += 1;
            }
        }
    }
    if written == 0 {
        bail!("encoded none of the {} input(s)", inputs.len());
    }
    if failed > 0 {
        println!("Encoded {written} image(s), {failed} failed");
    } else {
        println!("Encoded {written} image(s)");
    }
    Ok(())
}

fn run_build(args: &BuildArgs) -> Result<()> {
    let mut exts: Vec<&str> = vec!["dds"];
    exts.extend(IMAGE_EXTENSIONS);
    let inputs = collect_inputs(&args.inputs, &exts)?;

    // Lowercase name -> texture; the dictionary looks entries up by the
    // hash of the lowercased name, so that is the identity here.
    let mut entries: BTreeMap<String, YtdTexture> = BTreeMap::new();
    let mut kept: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(from) = &args.from {
        let data = std::fs::read(from).with_context(|| format!("failed to read {}", from.display()))?;
        let existing = parse_ytd(&data).with_context(|| format!("failed to parse YTD {}", from.display()))?;
        println!("Starting from {} ({} texture(s))", from.display(), existing.len());
        for tex in existing {
            kept.insert(tex.name.to_lowercase());
            entries.insert(tex.name.to_lowercase(), tex);
        }
    }

    let mut added = 0usize;
    let mut replaced = 0usize;
    for path in &inputs {
        let tex = if has_extension(path, &["dds"]) {
            let data = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
            let mut tex = parse_dds(&data).with_context(|| format!("{}", path.display()))?;
            tex.name = stem_of(path);
            tex.name_hash = rage_formats::rage_joaat(&tex.name.to_lowercase());
            tex
        } else if has_extension(path, IMAGE_EXTENSIONS) {
            encode_file(path, args.format, args.mips)?
        } else {
            bail!("{}: not a DDS or image file", path.display());
        };
        let key = tex.name.to_lowercase();
        kept.remove(&key);
        let verb = if entries.insert(key, tex.clone()).is_some() {
            replaced += 1;
            "replaces"
        } else {
            added += 1;
            "adds"
        };
        println!("{}  [{} {}]", describe(&tex.name, &tex), path.display(), verb);
    }

    let textures: Vec<YtdTexture> = entries.into_values().collect();
    let bytes = serialize_ytd(&textures)?;
    if let Some(parent) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&args.output, &bytes).with_context(|| format!("failed to write {}", args.output.display()))?;

    let kept = kept.len();
    println!(
        "Wrote {} with {} texture(s) ({} bytes): {} added, {} replaced, {} kept",
        args.output.display(), textures.len(), bytes.len(), added, replaced, kept
    );
    Ok(())
}
