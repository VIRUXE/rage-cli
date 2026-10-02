//! `rage resource info` and `rage resource dump`: inspect a loose resource
//! or metadata file (or an entry inside an archive). `info` prints the
//! container header and a summary of what the file holds — textures,
//! drawables, a map's entities, a type file's archetypes, a manifest's
//! dependencies. `dump` writes any Meta or PSO file out whole, as XML in
//! CodeWalker's layout or as JSON. `rename` changes a name inside a file
//! — a map's own name, a manifest's imapName — without rewriting the rest.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use rage_formats::{
    dump_meta, dump_metadata, parse_drawables, parse_ymap, parse_ymf, parse_ytd, parse_ytyp, prepare_rsc7, rage_joaat, set_map_name,
    resource_size_from_flags, resource_version_from_flags, to_json, to_xml, DrawableEntry, DrawableKind, Manifest,
    MetaContainer, MetaDump, MetaStruct, MetaValue, NameTable, Vec3, Ymap, YmapEntity, YmapHeader, Ytyp, YtdTexture, RSC7_MAGIC, RSC8_MAGIC,
};

use crate::resources::load_resource_bytes;
use super::resource_drawable as drawable;
use crate::rpf::GtaKeys;
use crate::utils::json_string;

#[derive(clap::Args)]
pub struct ResourceArgs {
    #[command(subcommand)]
    pub command: ResourceCommand,
}

#[derive(clap::Subcommand)]
pub enum ResourceCommand {
    /// Print the header and a summary of a .ydr/.ydd/.yft/.ytd/.ybn/.ynd/.ymap/.ytyp/.ymf
    Info(InfoArgs),
    /// Write a Meta or PSO file (.ymap .ytyp .ymt .ymf .pso) out as XML or JSON; a .ydr, .ybn or .ynd as XML (textures as .dds)
    Dump(DumpArgs),
    /// Change a name inside a .ymap (its own name), a .ymf or any Meta/PSO/XML file, in place
    Rename(RenameArgs),
    /// Build a .ymap/.ytyp/.ymt, a _manifest.ymf/.pso, a .ydr, a .ybn or a .ynd from XML or JSON written by `dump`
    Build(BuildArgs),
    /// Fix the flags and extents of .ymap files in place (a file, or every .ymap in a folder)
    Recalc(RecalcArgs),
}

#[derive(clap::Args)]
pub struct RecalcArgs {
    /// .ymap files, or folders to search for them
    #[arg(required = true, value_name = "YMAP|FOLDER")]
    pub paths: Vec<PathBuf>,

    /// .ytyp files (or folders of them) declaring the maps' archetypes;
    /// default: every .ytyp in each map's resource folder, then the game's
    /// own through the index (--exe / GTAV_PATH)
    #[arg(long, value_name = "PATH")]
    pub ytyp: Vec<PathBuf>,

    /// Report what would change and write nothing
    #[arg(long)]
    pub dry_run: bool,

    /// Print one JSON object with each file's values before and after
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    /// The XML or JSON to build from (as `resource dump` writes it)
    pub file: PathBuf,

    /// The file to write; its extension picks the container unless --format says otherwise
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,

    /// meta (RSC7 .ymap/.ytyp/.ymt), pso (.ymf/.pso), ydr, ybn or ynd; default: by the output extension
    #[arg(long, value_name = "meta|pso|ydr|ybn|ynd")]
    pub format: Option<String>,

    /// Binary Meta/PSO files whose structure definitions take precedence
    /// over the built-in CodeWalker tables (the original file is a good one);
    /// an existing output file is used the same way
    #[arg(long, value_name = "FILE")]
    pub schema: Vec<PathBuf>,

    /// Treat any structure the writer could not fill, or any warning of a
    /// drawable build (a texture not embedded, a bone without a name), as an error
    #[arg(long)]
    pub strict: bool,

    /// Write a .ymap's flags, contentFlags and extents exactly as the input
    /// gives them, instead of working them out from what the map holds
    #[arg(long)]
    pub no_recalc: bool,

    /// .ytyp files (or folders of them) declaring the map's archetypes, for
    /// the extents; default: every .ytyp in the output's resource folder,
    /// then the game's own through the index (--exe / GTAV_PATH)
    #[arg(long, value_name = "PATH")]
    pub ytyp: Vec<PathBuf>,

    /// The folder a drawable's <FileName>.dds textures are read from;
    /// default: the XML's own folder
    #[arg(long, value_name = "DIR")]
    pub textures: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct RenameArgs {
    /// A loose .ymap, .ytyp, .ymt, .ymf, .pso or XML file on disk
    pub file: PathBuf,

    /// The new name (hashed lowercase, as the game hashes asset names)
    pub name: String,

    /// The name (or 0x hash) to replace, wherever a hash field holds it;
    /// without it a .ymap gets NAME as its own CMapData.name, and any
    /// other file needs it
    #[arg(long, value_name = "NAME")]
    pub from: Option<String>,

    /// Also set a .ymap's parent map (only without --from)
    #[arg(long, value_name = "NAME", conflicts_with = "from")]
    pub parent: Option<String>,

    /// Write here instead of over the input
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Report what would change and write nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::Args)]
pub struct InfoArgs {
    /// A loose resource file on disk, or (with --archive) a name inside the archive
    pub file: String,

    /// Look `FILE` up inside this RPF archive instead of on disk
    #[arg(short, long, value_name = "RPF")]
    pub archive: Option<PathBuf>,

    /// Print one JSON object instead of text
    #[arg(long)]
    pub json: bool,

    /// How many entities of a map to list (0 for all)
    #[arg(long, default_value = "50", value_name = "N")]
    pub limit: usize,

    /// Extra name lists (one name per line) for resolving hashes; repeatable
    #[arg(long, value_name = "FILE")]
    pub names: Vec<PathBuf>,
}

#[derive(clap::Args)]
pub struct DumpArgs {
    /// A loose file on disk, or (with --archive) a name inside the archive
    pub file: String,

    /// Look `FILE` up inside this RPF archive instead of on disk
    #[arg(short, long, value_name = "RPF")]
    pub archive: Option<PathBuf>,

    /// Write JSON instead of XML
    #[arg(long)]
    pub json: bool,

    /// Write here instead of stdout
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Extra name lists (one name per line) for resolving hashes; repeatable
    #[arg(long, value_name = "FILE")]
    pub names: Vec<PathBuf>,

    /// Do not save a drawable's embedded textures as .dds files
    /// (they go beside --output, or into the current folder)
    #[arg(long)]
    pub no_dds: bool,
}

/// "FXAP": the header Cfx.re asset escrow puts on encrypted stream files.
const FXAP_MAGIC: u32 = 0x5041_5846;

/// The 16-byte RSC7 header plus what could be learnt about the body.
#[derive(Debug, PartialEq)]
pub struct Rsc7Header {
    pub version: u32,
    pub system_flags: u32,
    pub graphics_flags: u32,
    pub system_size: usize,
    pub graphics_size: usize,
    /// Body is deflated (false when it is stored raw).
    pub compressed: bool,
    /// Size of the body as read, in bytes.
    pub body_len: usize,
}

/// What the first bytes say the file is.
#[derive(Debug, PartialEq)]
pub enum Container {
    Rsc7(Rsc7Header),
    /// PSO, RBF or XML metadata, with no RSC7 header to report.
    Meta(MetaContainer),
}

/// Recognises the container. An escrowed or Gen9 file is an error that
/// says which; so is anything that is none of RSC7, PSO, RBF or XML.
pub fn detect(data: &[u8]) -> Result<Container> {
    if data.len() >= 4 {
        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if magic == FXAP_MAGIC {
            bail!("FiveM escrow-encrypted asset (magic 'FXAP'); only the server that bought it can decrypt it");
        }
        if magic == RSC8_MAGIC {
            bail!("Gen9 RSC8 resources are not supported (magic 0x{magic:08X})");
        }
        if magic == RSC7_MAGIC {
            return Ok(Container::Rsc7(parse_header(data)?));
        }
        match MetaContainer::detect(data) {
            Some(MetaContainer::Meta) | None => {}
            Some(other) => return Ok(Container::Meta(other)),
        }
        bail!("not an RSC7 resource (magic 0x{magic:08X}, expected 0x{RSC7_MAGIC:08X}), nor a PSO, RBF or XML file");
    }
    bail!("file is {} bytes; an RSC7 header needs 16", data.len());
}

pub fn parse_header(data: &[u8]) -> Result<Rsc7Header> {
    if data.len() < 16 {
        bail!("file is {} bytes; an RSC7 header needs 16", data.len());
    }

    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if magic == FXAP_MAGIC {
        bail!("FiveM escrow-encrypted asset (magic 'FXAP'); only the server that bought it can decrypt it");
    }
    if magic == RSC8_MAGIC {
        bail!("Gen9 RSC8 resources are not supported (magic 0x{magic:08X})");
    }
    if magic != RSC7_MAGIC {
        bail!("not an RSC7 resource (magic 0x{magic:08X}, expected 0x{RSC7_MAGIC:08X})");
    }

    let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let system_flags = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let graphics_flags = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let system_size = resource_size_from_flags(system_flags);
    let graphics_size = resource_size_from_flags(graphics_flags);

    // prepare_rsc7 inflates when it can and otherwise hands the body back as
    // stored; the two cases are told apart by whether the system section it
    // returned is literally the start of the body.
    let body = &data[16..];
    let (system, _) = prepare_rsc7(data)?;
    let compressed = !(body.len() >= system_size + graphics_size && body[..system_size] == system[..]);

    Ok(Rsc7Header {
        version, system_flags, graphics_flags, system_size, graphics_size,
        compressed, body_len: body.len(),
    })
}

/// What the body was parsed as, keyed off the file extension and, failing
/// that, the container.
pub enum Contents {
    Textures(Vec<YtdTexture>),
    Drawables(Vec<DrawableEntry>, Option<drawable::DrawableExtras>),
    /// A `.ybn`: its root bound.
    Bounds(drawable::BoundInfo),
    /// A `.ynd`: the cell's nodes, links and junctions.
    Paths(rage_formats::Ynd),
    Map(Ymap),
    Types(Ytyp),
    Manifest(Manifest),
    /// Any other self-describing metadata: the generic tree.
    Meta(MetaDump),
    Other,
}

fn extension_of(name: &str) -> String {
    Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
}

fn parse_contents(name: &str, data: &[u8], container: &Container) -> Result<Contents> {
    let ext = extension_of(name);
    match container {
        Container::Rsc7(header) => {
            if ext == "ytd" {
                return Ok(Contents::Textures(parse_ytd(data).context("failed to parse texture dictionary")?));
            }
            if let Some(kind) = DrawableKind::from_extension(&ext) {
                let entries = parse_drawables(data, kind).context("failed to parse drawable")?;
                // The block reader adds skeleton, lights and bound; a file it rejects keeps the plain summary.
                let extras = if ext == "ydr" { drawable::DrawableExtras::read(data) } else { None };
                return Ok(Contents::Drawables(entries, extras));
            }
            // A bounds resource is version 43; any other body is not read as one.
            if ext == "ybn"
                && header.version == 43
                && let Some(info) = drawable::read_bound_info(data)
            {
                return Ok(Contents::Bounds(info));
            }
            match ext.as_str() {
                "ynd" => Ok(Contents::Paths(rage_formats::parse_ynd(data).context("failed to parse path nodes")?)),
                "ymap" => Ok(Contents::Map(parse_ymap(data).context("failed to parse map")?)),
                "ytyp" => Ok(Contents::Types(parse_ytyp(data).context("failed to parse type file")?)),
                "ymf" => parse_ymf(data).map(|(_, m)| Contents::Manifest(m)).context("failed to parse manifest"),
                // A Meta file of any other kind (a .ymt, say) still carries
                // its schema; anything else is a fixed-layout resource this
                // command has no summary for.
                _ => Ok(dump_meta(data).map_or(Contents::Other, Contents::Meta)),
            }
        }
        Container::Meta(kind) => {
            if ext == "ymf" {
                return parse_ymf(data).map(|(_, m)| Contents::Manifest(m)).context("failed to parse manifest");
            }
            let (found, dump) = dump_metadata(data)?;
            debug_assert_eq!(found, *kind);
            match Manifest::from_value(&dump.root) {
                Ok(m) => Ok(Contents::Manifest(m)),
                Err(_) => Ok(Contents::Meta(dump)),
            }
        }
    }
}

pub fn run(args: &ResourceArgs, keys: Option<&GtaKeys>, exe: Option<&Path>, verbose: bool) -> Result<()> {
    match &args.command {
        ResourceCommand::Info(info) => run_info(info, keys, verbose),
        ResourceCommand::Dump(dump) => run_dump(dump, keys),
        ResourceCommand::Rename(rename) => run_rename(rename),
        ResourceCommand::Build(build) => run_build(build, keys, exe),
        ResourceCommand::Recalc(recalc) => run_recalc(recalc, keys, exe),
    }
}

/// The hashes a `--from` argument stands for: a `0x` hash as given, or a
/// name in both the spelling given and lowercase (asset names are hashed
/// lowercase, but a file written by hand may carry either).
fn from_hashes(from: &str) -> Vec<u32> {
    if let Some(hex) = from.strip_prefix("0x").or_else(|| from.strip_prefix("0X"))
        && let Ok(h) = u32::from_str_radix(hex, 16)
    {
        return vec![h];
    }
    let mut v = vec![rage_joaat(from)];
    let lower = rage_joaat(&from.to_lowercase());
    if lower != v[0] {
        v.push(lower);
    }
    v
}

/// Replaces `old` wherever it is a whole element text or attribute value
/// (`>old<`, `"old"`), case-insensitively; returns the text and the count.
pub fn rename_in_xml(text: &str, old: &str, new: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let lower = text.to_lowercase();
    let needle = old.to_lowercase();
    let mut pos = 0;
    while let Some(i) = lower[pos..].find(&needle) {
        let start = pos + i;
        let end = start + needle.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        let whole = matches!(before, Some('>') | Some('"') | Some('\'')) && matches!(after, Some('<') | Some('"') | Some('\''));
        out.push_str(&text[pos..start]);
        if whole {
            out.push_str(new);
            count += 1;
        } else {
            out.push_str(&text[start..end]);
        }
        pos = end;
    }
    out.push_str(&text[pos..]);
    (out, count)
}

fn run_rename(args: &RenameArgs) -> Result<()> {
    let path = &args.file;
    let data = std::fs::read(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let container = detect(&data).with_context(|| format!("'{}'", path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();

    let new_hash = rage_joaat(&args.name.to_lowercase());

    // Without --from, a map's own name (and parent) by the fixed layout;
    // with it, every hash field equal to the old name, by the schema.
    let (from_label, out, changed) = match (&args.from, &container) {
        (None, Container::Rsc7(_)) if ext == "ymap" => {
            let old = rage_formats::parse_ymap_header(&data).context("reading the map's name")?.name_hash;
            let parent = args.parent.as_deref().map(|p| rage_joaat(&p.to_lowercase()));
            let (out, n) = set_map_name(&data, new_hash, parent)?;
            (format!("0x{old:08X}"), out, n)
        }
        (None, _) => bail!("--from is required: only a .ymap has one name of its own to set"),
        (Some(from), Container::Rsc7(_)) => {
            let (out, n) = rage_formats::meta_schema::replace_hashes(&data, &from_hashes(from), new_hash)
                .with_context(|| format!("'{}' is an RSC7 resource but not a Meta file (only .ymap/.ytyp/.ymt carry names to rename)", path.display()))?;
            (from.clone(), out, n)
        }
        (Some(from), Container::Meta(MetaContainer::Pso)) => {
            let (out, n) = rage_formats::pso::replace_hashes(&data, &from_hashes(from), new_hash)?;
            (from.clone(), out, n)
        }
        (Some(from), Container::Meta(MetaContainer::Xml)) => {
            let text = String::from_utf8(data).context("the XML file is not UTF-8")?;
            let (text, n) = rename_in_xml(&text, from, &args.name);
            (from.clone(), text.into_bytes(), n)
        }
        (Some(_), Container::Meta(other)) => bail!("renaming inside a {other} file is not supported"),
    };

    if changed == 0 {
        bail!("no name field in '{}' holds {from_label} (or it already is {}); nothing written", path.display(), args.name);
    }
    let dest = args.output.as_deref().unwrap_or(path);
    if args.dry_run {
        println!("Would rename {changed} field(s) {from_label} -> {} (0x{new_hash:08X}) in {}", args.name, dest.display());
        return Ok(());
    }
    std::fs::write(dest, &out).with_context(|| format!("writing {}", dest.display()))?;
    println!("Renamed {changed} field(s) {from_label} -> {} (0x{new_hash:08X}) in {}", args.name, dest.display());
    if ext == "ymap" {
        let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !stem.eq_ignore_ascii_case(&args.name) {
            eprintln!("note: the game registers this map as {stem} (its file name); rename the file to {}.ymap for the two to agree", args.name);
        }
        eprintln!("note: a _manifest.ymf declaring the old name needs the same change: rage resource rename _manifest.ymf {} --from {from_label}", args.name);
    }
    Ok(())
}

/// The file on disk whose siblings name the hashes, when the input is one.
fn run_build(args: &BuildArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    use rage_formats::{build_meta, build_pso, dump_meta, dump_pso, from_json, from_xml, Schema};

    let text = std::fs::read_to_string(&args.file).with_context(|| format!("failed to read '{}'", args.file.display()))?;
    let out_ext = args.output.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    // Drawables and bounds are built from their own XML, not the generic Meta tree.
    let drawable_kind = match args.format.as_deref().map(str::to_lowercase).as_deref() {
        Some("ydr") => Some("ydr"),
        Some("ybn") => Some("ybn"),
        Some("ynd") => Some("ynd"),
        Some(_) => None,
        None if matches!(out_ext.as_str(), "ydr" | "ybn" | "ynd") => drawable::kind_of_name(&args.output.to_string_lossy()),
        None if matches!(out_ext.as_str(), "ymf" | "pso" | "ymap" | "ytyp" | "ymt") => None,
        None => drawable::sniff_xml(&text),
    };
    if let Some(kind) = drawable_kind {
        return drawable::build(kind, &text, args);
    }
    let mut value = if text.trim_start().starts_with('{') {
        from_json(&text).with_context(|| format!("'{}'", args.file.display()))?
    } else {
        from_xml(&text).with_context(|| format!("'{}'", args.file.display()))?
    };

    let format = match args.format.as_deref().map(str::to_lowercase).as_deref() {
        Some("meta") => "meta",
        Some("pso") => "pso",
        Some(other) => bail!("--format {other}: expected meta or pso (or ydr, ybn, ynd for a drawable, bound or path nodes)"),
        None if matches!(out_ext.as_str(), "ymf" | "pso") => "pso",
        None if matches!(out_ext.as_str(), "ymap" | "ytyp" | "ymt") => "meta",
        None => bail!("cannot tell the container from '.{out_ext}'; write a .ymap/.ytyp/.ymt or .ymf/.pso, or pass --format"),
    };

    let mut schema = Schema::builtin().clone();
    let mut sources: Vec<&Path> = args.schema.iter().map(PathBuf::as_path).collect();
    if args.output.is_file() && !sources.contains(&args.output.as_path()) {
        sources.push(&args.output);
    }
    for path in sources {
        let data = std::fs::read(path).with_context(|| format!("failed to read '{}'", path.display()))?;
        let own = Schema::from_file(&data).with_context(|| format!("'{}' as a schema source", path.display()))?;
        eprintln!("Using the structure definitions of {} ({} structures)", path.display(), own.meta_structs.len() + own.pso_structs.len());
        schema.merge(own);
    }

    if !args.no_recalc
        && let MetaValue::Struct(map) = &mut value
        && map.type_hash == rage_joaat("CMapData")
    {
        recalc_map(map, &args.output, &args.ytyp, keys, exe)?;
    }

    let written = if format == "pso" { build_pso(&value, &schema)? } else { build_meta(&value, &schema)? };
    for warning in &written.warnings {
        eprintln!("warning: {warning}");
    }
    if args.strict && !written.warnings.is_empty() {
        bail!("{} member(s) could not be written (--strict)", written.warnings.len());
    }

    // Read it back the way `dump` would, so a file the tool cannot read is
    // never handed over silently.
    let check = if format == "pso" { dump_pso(&written.bytes) } else { dump_meta(&written.bytes) };
    let check = check.context("the written file does not read back")?;
    for warning in &check.warnings {
        eprintln!("warning: reading the result back: {warning}");
    }
    if let Some(parent) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&args.output, &written.bytes).with_context(|| format!("writing {}", args.output.display()))?;
    let root = value.as_struct().map_or(0, |s| s.type_hash);
    let names = crate::names::load(&[], Some(&args.output))?;
    eprintln!(
        "Wrote {} ({} bytes, {} from {}, root {})",
        args.output.display(),
        written.bytes.len(),
        if format == "pso" { "PSO" } else { "RSC7 Meta" },
        args.file.display(),
        names.resolve(root),
    );
    Ok(())
}

/// Works a map's flags and extents out from its contents (see
/// `crate::extents`), with archetype bounds from `--ytyp`, the resource
/// folder's own .ytyp files, and the game index for whatever is left.
pub fn recalc_map(map: &mut MetaStruct, output: &Path, ytyp: &[PathBuf], keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    let roots: Vec<PathBuf> = if ytyp.is_empty() { resource_root(output).into_iter().collect() } else { ytyp.to_vec() };
    let files = ytyp_files(&roots)?;
    let lookup = crate::extents::Lookup::new(&files, exe, keys);
    let r = MapRecalc::run(map, &lookup, files.len());
    if r.changes.is_empty() {
        eprintln!("Map flags and extents already match its contents{}", r.sources());
    } else {
        eprintln!("Recalculated {}{}", r.changes.join(", "), r.sources());
    }
    if let Some(why) = r.kept_because {
        eprintln!("note: the extents are kept as given: {why}");
    }
    if !r.unbound().is_empty() {
        let names = crate::names::load(&[], Some(output))?;
        eprintln!("warning: {}", r.unbound_warning(&names, exe));
    }
    Ok(())
}

/// Every .ytyp in the files and folders given.
fn ytyp_files(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for root in roots {
        if root.is_dir() {
            files.extend(crate::utils::walkdir(root)?.into_iter().filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ytyp"))));
        } else if root.is_file() {
            files.push(root.clone());
        } else {
            bail!("--ytyp {}: no such file or folder", root.display());
        }
    }
    Ok(files)
}

/// What recalculating one map tree changed and could not work out.
struct MapRecalc {
    changes: Vec<String>,
    from_ytyp: usize,
    from_game: usize,
    kept_because: Option<&'static str>,
    /// Placed archetypes with no bounds (their entities count as points).
    unbound_points: Vec<u32>,
    /// Interior room entities' archetypes with no bounds.
    unbound_rooms: Vec<u32>,
}

impl MapRecalc {
    /// Recalculates `map` in place; `ytyps` is how many files `lookup` read.
    fn run(map: &mut MetaStruct, lookup: &crate::extents::Lookup, ytyps: usize) -> Self {
        lookup.reset();
        let result = crate::extents::calc(map, &|a| lookup.get(a));
        let changes = crate::extents::apply(map, &result);
        MapRecalc {
            changes,
            from_ytyp: ytyps,
            from_game: lookup.from_game.borrow().len(),
            kept_because: result.kept_because,
            unbound_points: result.unbound,
            unbound_rooms: lookup.room_unbound.take(),
        }
    }

    fn sources(&self) -> String {
        match (self.from_ytyp, self.from_game) {
            (0, 0) => String::new(),
            (n, 0) => format!(" (archetypes from {n} .ytyp)"),
            (0, g) => format!(" ({g} archetypes from the game)"),
            (n, g) => format!(" (archetypes from {n} .ytyp, {g} from the game)"),
        }
    }

    fn unbound(&self) -> Vec<u32> {
        let mut all = self.unbound_points.clone();
        all.extend(self.unbound_rooms.iter().filter(|h| !self.unbound_points.contains(h)));
        all
    }

    fn unbound_warning(&self, names: &NameTable, exe: Option<&Path>) -> String {
        let unbound = self.unbound();
        let listed: Vec<String> = unbound.iter().take(8).map(|h| names.resolve(*h).into_owned()).collect();
        let more = if unbound.len() > 8 { format!(" and {} more", unbound.len() - 8) } else { String::new() };
        let rooms = if self.unbound_rooms.is_empty() { String::new() } else { format!(", including {} inside interiors", self.unbound_rooms.len()) };
        let effect = match (self.unbound_points.is_empty(), self.unbound_rooms.is_empty()) {
            (false, true) => "their entities count as points",
            (true, false) => "their entities are left out of the interior boxes",
            _ => "their entities count as points, or are left out of the interior boxes",
        };
        let hint = if exe.is_none() { "; pass --ytyp, or --exe / GTAV_PATH for vanilla archetypes" } else { "; pass --ytyp with the files that declare them" };
        format!("no bounds for {} archetype(s){rooms}: {}{more}; {effect}{hint}", unbound.len(), listed.join(", "))
    }
}

/// What `recalc` did to one file.
struct RecalcRow {
    file: PathBuf,
    before: crate::extents::Header,
    after: crate::extents::Header,
    report: MapRecalc,
    written: bool,
}

fn run_recalc(args: &RecalcArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    use std::collections::HashMap;

    let mut maps = Vec::new();
    for path in &args.paths {
        if path.is_dir() {
            let found: Vec<PathBuf> = crate::utils::walkdir(path)?.into_iter().filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ymap"))).collect();
            if found.is_empty() {
                eprintln!("warning: no .ymap files under {}", path.display());
            }
            maps.extend(found);
        } else if path.is_file() {
            maps.push(path.clone());
        } else {
            bail!("{}: no such file or folder", path.display());
        }
    }
    maps.sort();
    maps.dedup();
    if maps.is_empty() {
        bail!("no .ymap files to recalculate");
    }

    // One lookup per resource (or one for all with --ytyp), so each
    // resource's .ytyp files are read once however many maps it holds.
    let given = if args.ytyp.is_empty() { None } else { Some(ytyp_files(&args.ytyp)?) };
    let mut lookups: HashMap<Option<PathBuf>, (usize, crate::extents::Lookup)> = HashMap::new();
    let mut rows = Vec::new();
    let mut errors: Vec<(PathBuf, String)> = Vec::new();
    for path in &maps {
        let key = if given.is_some() { None } else { resource_root(path) };
        if !lookups.contains_key(&key) {
            let files = match &given {
                Some(files) => files.clone(),
                None => ytyp_files(key.as_slice())?,
            };
            lookups.insert(key.clone(), (files.len(), crate::extents::Lookup::new(&files, exe, keys)));
        }
        let (ytyps, lookup) = &lookups[&key];
        match recalc_file(path, lookup, *ytyps, args.dry_run) {
            Ok(row) => rows.push(row),
            Err(e) => {
                if !args.json {
                    eprintln!("error: {}: {e:#}", path.display());
                }
                errors.push((path.clone(), format!("{e:#}")));
            }
        }
    }

    let changed = rows.iter().filter(|r| !r.report.changes.is_empty()).count();
    let names = crate::names::load(&[], maps.first().map(PathBuf::as_path))?;
    if args.json {
        let vec = |v: Option<Vec3>| v.map_or(json::JsonValue::Null, json_vec);
        let header = |h: &crate::extents::Header| {
            json::object! {
                flags: h.flags,
                content_flags: h.content_flags,
                entities_extents: json::array![vec(h.entities_extents.0), vec(h.entities_extents.1)],
                streaming_extents: json::array![vec(h.streaming_extents.0), vec(h.streaming_extents.1)],
            }
        };
        let mut files: Vec<json::JsonValue> = rows
            .iter()
            .map(|r| {
                json::object! {
                    file: r.file.display().to_string(),
                    changed: !r.report.changes.is_empty(),
                    written: r.written,
                    changes: r.report.changes.clone(),
                    before: header(&r.before),
                    after: header(&r.after),
                    extents_kept_because: r.report.kept_because,
                    unbound_archetypes: r.report.unbound().iter().map(|h| json_name(*h, &names)).collect::<Vec<_>>(),
                    archetypes_from_ytyp_files: r.report.from_ytyp,
                    archetypes_from_game: r.report.from_game,
                }
            })
            .collect();
        files.extend(errors.iter().map(|(file, error)| json::object! { file: file.display().to_string(), error: error.clone() }));
        let out = json::object! { dry_run: args.dry_run, maps: maps.len(), changed: changed, failed: errors.len(), files: files };
        println!("{}", out.dump());
    } else {
        for r in &rows {
            let file = r.file.display();
            if !r.report.changes.is_empty() {
                let verb = if args.dry_run { "would change" } else { "recalculated" };
                println!("{file}: {verb} {}{}", r.report.changes.join(", "), r.report.sources());
            }
            if let Some(why) = r.report.kept_because {
                eprintln!("note: {file}: the extents are kept as given: {why}");
            }
            if !r.report.unbound().is_empty() {
                eprintln!("warning: {file}: {}", r.report.unbound_warning(&names, exe));
            }
        }
        let verb = if args.dry_run { "would change" } else { "recalculated" };
        let failed = if errors.is_empty() { String::new() } else { format!(", {} failed", errors.len()) };
        println!("{} of {} map(s) {verb}, {} already matched{failed}", changed, maps.len(), rows.len() - changed);
    }
    if !errors.is_empty() {
        bail!("{} map(s) could not be recalculated", errors.len());
    }
    Ok(())
}

/// Recalculates one .ymap on disk, rewriting it when anything changed
/// (and `dry_run` is off). A file whose round trip through the Meta
/// writer would lose anything is left alone, as an error.
fn recalc_file(path: &Path, lookup: &crate::extents::Lookup, ytyps: usize, dry_run: bool) -> Result<RecalcRow> {
    use rage_formats::{build_meta, Schema};

    let data = std::fs::read(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    if !matches!(detect(&data)?, Container::Rsc7(_)) {
        bail!("not an RSC7 .ymap (an XML or JSON map is fixed with `resource build`)");
    }
    let dump = dump_meta(&data).context("not a Meta file")?;
    if let Some(w) = dump.warnings.first() {
        bail!("not rewritten: the map does not read cleanly ({w}{})", if dump.warnings.len() > 1 { format!(", and {} more", dump.warnings.len() - 1) } else { String::new() });
    }
    let MetaValue::Struct(mut map) = dump.root else { bail!("the file's root is not a structure") };
    if map.type_hash != rage_joaat("CMapData") {
        bail!("not a map (the root is 0x{:08X}, not CMapData)", map.type_hash);
    }

    let before = crate::extents::header(&map);
    let report = MapRecalc::run(&mut map, lookup, ytyps);
    let after = crate::extents::header(&map);
    let mut written = false;
    if !report.changes.is_empty() && !dry_run {
        let mut schema = Schema::builtin().clone();
        schema.merge(Schema::from_file(&data).context("reading the map's own structure definitions")?);
        let out = build_meta(&MetaValue::Struct(map), &schema)?;
        if let Some(w) = out.warnings.first() {
            bail!("not rewritten: {} member(s) could not be written back ({w})", out.warnings.len());
        }
        let check = dump_meta(&out.bytes).context("not rewritten: the new file does not read back")?;
        if let Some(w) = check.warnings.first() {
            bail!("not rewritten: the new file reads back with warnings ({w})");
        }
        std::fs::write(path, &out.bytes).with_context(|| format!("writing {}", path.display()))?;
        written = true;
    }
    Ok(RecalcRow { file: path.to_path_buf(), before, after, report, written })
}

/// The FiveM resource an output file lands in (the nearest folder up with
/// an `fxmanifest.lua` or `__resource.lua`), else just its own folder.
fn resource_root(output: &Path) -> Option<PathBuf> {
    let dir = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let dir = std::fs::canonicalize(dir).ok()?;
    dir.ancestors()
        .find(|d| d.join("fxmanifest.lua").is_file() || d.join("__resource.lua").is_file())
        .map(Path::to_path_buf)
        .or(Some(dir))
}

fn local_path(file: &str, archive: Option<&Path>) -> Option<PathBuf> {
    archive.is_none().then(|| PathBuf::from(file))
}

/// The text `resource info` prints: JSON when `args.json`, else the summary.
pub fn info_text(args: &InfoArgs, keys: Option<&GtaKeys>, verbose: bool) -> Result<String> {
    let data = load_resource_bytes(&args.file, args.archive.as_deref(), keys)?;
    let container = detect(&data).with_context(|| format!("'{}'", args.file))?;
    let contents = parse_contents(&args.file, &data, &container)?;
    let local = local_path(&args.file, args.archive.as_deref());
    let names = crate::names::load(&args.names, local.as_deref())?;
    let checks = local.as_deref().map(|p| Checks::of(p, &contents, &args.names)).unwrap_or_default();

    let mut out = String::new();
    if args.json {
        write_json(&mut out, args, &container, &contents, &names, &checks, verbose);
    } else {
        write_text(&mut out, args, &container, &contents, &names, &checks, verbose);
        if let Some(note) = unresolved_note(&out) {
            out.push_str(&note);
        }
    }
    Ok(out)
}

fn run_info(args: &InfoArgs, keys: Option<&GtaKeys>, verbose: bool) -> Result<()> {
    print!("{}", info_text(args, keys, verbose)?);
    Ok(())
}

/// A closing line when the text left `hash_XXXXXXXX` placeholders behind:
/// how many, what the active list covers, and that an absent name may be
/// newer than the list rather than missing from the game.
pub fn unresolved_note(text: &str) -> Option<String> {
    let count = text.match_indices("hash_").filter(|(i, _)| text[i + 5..].chars().take(8).all(|c| c.is_ascii_hexdigit()) && text[i + 5..].len() >= 8).count();
    if count == 0 {
        return None;
    }
    let list = match crate::names::harvested_header() {
        Some(h) => format!("the name list {}", h.coverage()),
        None => "there is no name list yet (`rage names fetch` needs no game install; `rage names harvest --exe` scans one)".to_owned(),
    };
    Some(format!(
        "Unresolved: {count} hash(es) with no known name; {list}. A name absent from a list older than the file is not proof the asset does not exist.\n"
    ))
}

/// What a loose file on disk says about itself versus its surroundings.
/// Neither check applies to an entry read out of an archive with
/// `--archive`, where the lookup name and the file name are one thing.
#[derive(Debug, Default, PartialEq)]
pub struct Checks {
    /// The `.ymap`'s internal `CMapData.name` is not the file's stem, so
    /// parent links and manifest `imapName` entries that use the internal
    /// name will never bind to the map the game registers under the file
    /// name. Holds the stem the game will use.
    pub map_name_mismatch: Option<String>,
    /// Manifest `imapName` entries with no `.ymap` of that name next to the
    /// manifest: either vanilla maps (the common case for a resource that
    /// edits base-game maps) or leftovers from a map since renamed or
    /// removed. Each is paired with whether any known name list (built-in,
    /// harvested, `--names`; not the siblings) has the name — when none
    /// does, it is most likely a leftover.
    pub orphan_imaps: Vec<(String, bool)>,
}

impl Checks {
    pub fn of(path: &Path, contents: &Contents, extra: &[PathBuf]) -> Self {
        let mut checks = Self::default();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        match contents {
            Contents::Map(ymap) => {
                let h = ymap.header.name_hash;
                if !stem.is_empty() && h != rage_joaat(stem) && h != rage_joaat(&stem.to_lowercase()) {
                    checks.map_name_mismatch = Some(stem.to_owned());
                }
            }
            Contents::Manifest(m) => {
                let sibling_maps: std::collections::HashSet<u32> = path
                    .parent()
                    .and_then(|dir| crate::utils::walkdir(dir).ok())
                    .unwrap_or_default()
                    .iter()
                    .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ymap")))
                    .filter_map(|p| p.file_stem().and_then(|s| s.to_str()))
                    .flat_map(|s| [rage_joaat(s), rage_joaat(&s.to_lowercase())])
                    .collect();
                let known = crate::names::load(extra, None).unwrap_or_else(|_| NameTable::core());
                for dep in &m.imap_dependencies_2 {
                    let hash = match &dep.name.name {
                        Some(n) => rage_joaat(n),
                        None => dep.name.hash,
                    };
                    if sibling_maps.contains(&hash) {
                        continue;
                    }
                    let label = dep.name.name.clone().unwrap_or_else(|| known.resolve(hash).into_owned());
                    checks.orphan_imaps.push((label, known.get(hash).is_some()));
                }
            }
            _ => {}
        }
        checks
    }

    /// The manifest entries that no list names at all.
    fn unknown_imaps(&self) -> Vec<&str> {
        self.orphan_imaps.iter().filter(|(_, known)| !known).map(|(n, _)| n.as_str()).collect()
    }
}

fn run_dump(args: &DumpArgs, keys: Option<&GtaKeys>) -> Result<()> {
    let data = load_resource_bytes(&args.file, args.archive.as_deref(), keys)?;
    let container = detect(&data).with_context(|| format!("'{}'", args.file))?;
    if let (Container::Rsc7(_), Some(kind)) = (&container, drawable::kind_of_name(&args.file)) {
        return run_dump_drawable(kind, &data, args);
    }
    let dump = match container {
        Container::Rsc7(_) => dump_meta(&data).with_context(|| {
            format!("'{}' is an RSC7 resource but not a Meta file (only .ymap/.ytyp/.ymt carry a schema to dump)", args.file)
        })?,
        Container::Meta(_) => dump_metadata(&data)?.1,
    };
    for warning in &dump.warnings {
        eprintln!("warning: {warning}");
    }
    let names = crate::names::load(&args.names, local_path(&args.file, args.archive.as_deref()).as_deref())?;
    let text = if args.json { to_json(&dump.root, &names).pretty(2) + "\n" } else { to_xml(&dump.root, &names) };
    match &args.output {
        Some(path) => {
            std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
            eprintln!("Wrote {}", path.display());
        }
        None => print!("{text}"),
    }
    if let Some(note) = unresolved_note(&text) {
        eprint!("{note}");
    }
    Ok(())
}

/// `dump` of a `.ydr` or `.ybn`: XML only, textures as `.dds` beside the output.
fn run_dump_drawable(kind: &str, data: &[u8], args: &DumpArgs) -> Result<()> {
    let names = crate::names::load(&args.names, local_path(&args.file, args.archive.as_deref()).as_deref())?;
    let text = drawable::dump(kind, data, &names, args.output.as_deref(), args.no_dds, args.json).with_context(|| format!("'{}'", args.file))?;
    match &args.output {
        Some(path) => {
            // as `resource build` does, and as a `.ydr` dump already did for its textures
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
            }
            std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
            eprintln!("Wrote {}", path.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

// ─── shared ──────────────────────────────────────────────────────────────────

/// The heading (yaw about z, degrees) an entity's stored rotation gives it.
/// Map entities store the inverse quaternion, so the conjugate is measured.
pub fn entity_yaw_degrees(e: &YmapEntity) -> f32 {
    let [x, y, z, w] = e.rotation;
    let (x, y, z) = (-x, -y, -z);
    (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z)).to_degrees()
}

/// The names of a `CEntityDef.flags` word's set bits, as CodeWalker labels them.
fn entity_flag_names(flags: u32) -> Vec<&'static str> {
    const NAMES: [&str; 32] = [
        "allow full rotation", "stream low priority", "disable embedded collision", "lod in parent map", "lod adopt me",
        "static entity", "interior lod", "lod use alt fade", "under water", "doesn't touch water", "doesn't spawn peds",
        "cast static shadows", "cast dynamic shadows", "ignore time in child rendering", "don't render in shadows",
        "only render in shadows", "don't render in reflections", "only render in reflections", "don't render in water reflections",
        "only render in water reflections", "don't render in mirror reflections", "only render in mirror reflections",
        "don't render in stream", "streaming ignore", "don't cast shadows", "unk25", "unk26", "unk27", "unk28", "unk29", "unk30", "unk31",
    ];
    NAMES.iter().enumerate().filter(|(i, _)| flags & (1 << i) != 0).map(|(_, n)| *n).collect()
}

fn header_flag_names(flags: u32) -> Vec<&'static str> {
    let mut names = Vec::new();
    if flags & YmapHeader::FLAG_SCRIPTED != 0 {
        names.push("SCRIPTED");
    }
    if flags & YmapHeader::FLAG_LOD != 0 {
        names.push("LOD");
    }
    names
}

fn fmt_vec3(v: Vec3) -> String {
    format!("({:.2}, {:.2}, {:.2})", v.x, v.y, v.z)
}

/// An empty map stores its boxes inverted (min at +MAX, max at -MAX); such
/// a box is not worth printing.
fn is_finite_box(lo: Vec3, hi: Vec3) -> bool {
    [lo.x, lo.y, lo.z, hi.x, hi.y, hi.z].iter().all(|v| v.is_finite() && v.abs() < 1.0e9) && lo.x <= hi.x && lo.y <= hi.y && lo.z <= hi.z
}

// ─── text ────────────────────────────────────────────────────────────────────

fn write_text(out: &mut String, args: &InfoArgs, container: &Container, contents: &Contents, names: &NameTable, checks: &Checks, verbose: bool) {
    use std::fmt::Write;

    match &args.archive {
        Some(archive) => writeln!(out, "File:      {} : {}", archive.display(), args.file).unwrap(),
        None => writeln!(out, "File:      {}", args.file).unwrap(),
    }
    match container {
        Container::Rsc7(h) => {
            writeln!(out, "Format:    RSC7  version {}", h.version).unwrap();
            let from_flags = resource_version_from_flags(h.system_flags, h.graphics_flags);
            if from_flags != h.version {
                writeln!(out, "           (warning: flags encode version {from_flags})").unwrap();
            }
            writeln!(out, "System:    0x{:08X} flags -> {} bytes", h.system_flags, h.system_size).unwrap();
            writeln!(out, "Graphics:  0x{:08X} flags -> {} bytes", h.graphics_flags, h.graphics_size).unwrap();
            writeln!(out, "Body:      {} bytes ({})", h.body_len, if h.compressed { "deflated" } else { "stored" }).unwrap();
        }
        Container::Meta(kind) => writeln!(out, "Format:    {kind}").unwrap(),
    }

    match contents {
        Contents::Other => writeln!(out, "Summary:   not a drawable, texture dictionary, map, type file or manifest").unwrap(),
        Contents::Textures(textures) => {
            writeln!(out, "Textures:  {}", textures.len()).unwrap();
            for tex in textures {
                write_texture_line(out, tex);
            }
        }
        Contents::Drawables(entries, extras) => {
            writeln!(out, "Drawables: {}", entries.len()).unwrap();
            for entry in entries {
                write_drawable(out, entry, verbose);
                if let Some(extras) = extras {
                    extras.write_text(out);
                }
            }
        }
        Contents::Bounds(info) => writeln!(out, "Bounds:    {}", info.label()).unwrap(),
        Contents::Paths(ynd) => out.push_str(&crate::commands::paths::describe(ynd, rage_formats::ynd_area_id_from_file_name(&args.file), names)),
        Contents::Map(ymap) => write_map(out, ymap, names, checks, args.limit),
        Contents::Types(ytyp) => write_types(out, ytyp, names, args.limit),
        Contents::Manifest(manifest) => write_manifest(out, manifest, names, checks),
        Contents::Meta(dump) => {
            let root = dump.root.as_struct().map(|s| names.resolve(s.type_hash).into_owned()).unwrap_or_else(|| "?".into());
            let fields = dump.root.as_struct().map_or(0, |s| s.fields.len());
            writeln!(out, "Summary:   {root} with {fields} members; `rage resource dump` prints it in full").unwrap();
            for w in &dump.warnings {
                writeln!(out, "           warning: {w}").unwrap();
            }
        }
    }
}

fn write_map(out: &mut String, ymap: &Ymap, names: &NameTable, checks: &Checks, limit: usize) {
    use std::fmt::Write;
    let h = &ymap.header;
    let parent = if h.parent_hash == 0 { "-".to_string() } else { names.resolve(h.parent_hash).into_owned() };
    writeln!(out, "Map:       {}  parent {parent}", names.resolve(h.name_hash)).unwrap();
    if let Some(stem) = &checks.map_name_mismatch {
        let internal = names.resolve(h.name_hash);
        writeln!(out, "           warning: the file is called {stem} but the map calls itself {internal} (0x{:08X}); the game registers it as {stem},", h.name_hash).unwrap();
        writeln!(out, "           so parent links and _manifest.ymf imapName entries that say {internal} will not bind (renamed outside CodeWalker?)").unwrap();
    }
    writeln!(out, "Flags:     0x{:X} {}  content 0x{:X} {}", h.flags, header_flag_names(h.flags).join("|"), h.content_flags, h.content_flag_names().join("|")).unwrap();
    if is_finite_box(h.streaming_extents_min, h.streaming_extents_max) {
        writeln!(out, "Streaming: {}..{}", fmt_vec3(h.streaming_extents_min), fmt_vec3(h.streaming_extents_max)).unwrap();
    }
    if is_finite_box(h.entities_extents_min, h.entities_extents_max) && !ymap.entities.is_empty() {
        writeln!(out, "Extents:   {}..{}", fmt_vec3(h.entities_extents_min), fmt_vec3(h.entities_extents_max)).unwrap();
    }
    let (outside, unstreamed) = crate::extents::strays(ymap);
    if outside + unstreamed > 0 {
        // The entities box spans the models, not their origins, so an origin
        // outside it is only a sign of a stale box; outside the streaming
        // box (the models grown by their lodDist) it is almost surely one.
        match (outside, unstreamed) {
            (_, 0) => writeln!(
                out,
                "           note: {outside} entities stand outside the entities extents; `rage resource recalc` fixes a stale box (if it changes nothing, their models just sit away from their origins)"
            ),
            (0, _) => writeln!(out, "           warning: {unstreamed} entities stand outside the streaming extents, so the game never loads the map where they are; `rage resource recalc` fixes both"),
            _ => writeln!(
                out,
                "           warning: {outside} entities stand outside the entities extents, {unstreamed} outside the streaming extents (the game never loads the map where those are); `rage resource recalc` fixes both"
            ),
        }
        .unwrap();
    }
    writeln!(out, "Entities:  {} ({} MLO instances)", ymap.entities.len(), ymap.mlo_instances.len()).unwrap();
    if ymap.entities.is_empty() {
        return;
    }
    let shown = if limit == 0 { ymap.entities.len() } else { limit.min(ymap.entities.len()) };
    writeln!(out, "  {:>4}  {:<32} {:>10} {:>10} {:>8} {:>7} {:>5} {:>6}  flags", "#", "archetype", "x", "y", "z", "yaw", "scale", "lod").unwrap();
    for (i, e) in ymap.entities.iter().take(shown).enumerate() {
        let name = names.resolve(e.archetype_hash);
        let kind = if e.is_mlo_instance { " (MLO)" } else { "" };
        writeln!(
            out,
            "  {i:>4}  {:<32} {:>10.3} {:>10.3} {:>8.3} {:>6.1}° {:>5.2} {:>6.0}  0x{:X}{kind}",
            format!("{name}"), e.position.x, e.position.y, e.position.z, entity_yaw_degrees(e), e.scale_xy, e.lod_dist, e.flags
        )
        .unwrap();
    }
    if shown < ymap.entities.len() {
        writeln!(out, "  ... {} more (--limit 0 for all)", ymap.entities.len() - shown).unwrap();
    }
    let mut set = std::collections::BTreeSet::new();
    for e in &ymap.entities {
        for name in entity_flag_names(e.flags) {
            set.insert(name);
        }
    }
    if !set.is_empty() {
        writeln!(out, "Entity flags seen: {}", set.into_iter().collect::<Vec<_>>().join(", ")).unwrap();
    }
}

fn write_types(out: &mut String, ytyp: &Ytyp, names: &NameTable, limit: usize) {
    use std::fmt::Write;
    writeln!(out, "Archetypes: {} ({} MLO)", ytyp.archetypes.len(), ytyp.mlos.len()).unwrap();
    let shown = if limit == 0 { ytyp.archetypes.len() } else { limit.min(ytyp.archetypes.len()) };
    writeln!(out, "  {:<32} {:<24} {:>7}  bounds", "name", "texture dictionary", "lod").unwrap();
    for a in ytyp.archetypes.iter().take(shown) {
        let txd = if a.texture_dict_hash == 0 { "-".to_string() } else { names.resolve(a.texture_dict_hash).into_owned() };
        writeln!(out, "  {:<32} {:<24} {:>7.0}  {}..{}{}", names.resolve(a.name_hash), txd, a.lod_dist, fmt_vec3(a.bb_min), fmt_vec3(a.bb_max), if a.is_mlo { " (MLO)" } else { "" }).unwrap();
    }
    if shown < ytyp.archetypes.len() {
        writeln!(out, "  ... {} more (--limit 0 for all)", ytyp.archetypes.len() - shown).unwrap();
    }
    for mlo in &ytyp.mlos {
        writeln!(
            out,
            "MLO {}: {} rooms, {} portals, {} entities, {} entity sets",
            names.resolve(mlo.name_hash), mlo.rooms.len(), mlo.portals.len(), mlo.entities.len(), mlo.entity_sets.len()
        )
        .unwrap();
        for (i, room) in mlo.rooms.iter().enumerate() {
            writeln!(out, "  room {i:>2} {:<24} {} props", room.name, room.attached_objects.len()).unwrap();
        }
    }
}

fn write_manifest(out: &mut String, m: &Manifest, names: &NameTable, checks: &Checks) {
    use std::fmt::Write;
    let name = |h: &rage_formats::HashName| match &h.name {
        Some(n) => n.clone(),
        None => names.resolve(h.hash).into_owned(),
    };
    let list = |v: &[rage_formats::HashName]| v.iter().map(name).collect::<Vec<_>>().join(", ");
    if m.is_empty() {
        writeln!(out, "Manifest:  empty").unwrap();
        return;
    }
    if !m.imap_dependencies_2.is_empty() {
        writeln!(out, "Map dependencies ({}):", m.imap_dependencies_2.len()).unwrap();
        for d in &m.imap_dependencies_2 {
            let flags = if d.manifest_flags & 1 != 0 { " [INTERIOR_DATA]" } else { "" };
            let n = name(&d.name);
            let note = match checks.orphan_imaps.iter().find(|(o, _)| *o == n) {
                Some((_, true)) => "  (no such .ymap here; a vanilla map?)",
                Some((_, false)) => "  (no such .ymap here, and no list knows the name)",
                None => "",
            };
            writeln!(out, "  {n}{flags} -> {}{note}", if d.ityp_deps.is_empty() { "-".to_string() } else { list(&d.ityp_deps) }).unwrap();
        }
        let unknown = checks.unknown_imaps();
        if !unknown.is_empty() {
            writeln!(
                out,
                "           warning: {} declared map(s) exist neither next to this manifest nor in any name list ({}); a leftover from a map since renamed or removed?",
                unknown.len(),
                unknown.join(", ")
            )
            .unwrap();
        }
    }
    if !m.ityp_dependencies_2.is_empty() {
        writeln!(out, "Type dependencies ({}):", m.ityp_dependencies_2.len()).unwrap();
        for d in &m.ityp_dependencies_2 {
            let flags = if d.manifest_flags & 1 != 0 { " [INTERIOR_DATA]" } else { "" };
            writeln!(out, "  {}{flags} -> {}", name(&d.name), if d.ityp_deps.is_empty() { "-".to_string() } else { list(&d.ityp_deps) }).unwrap();
        }
    }
    if !m.imap_dependencies.is_empty() {
        writeln!(out, "Map dependencies, old form ({}):", m.imap_dependencies.len()).unwrap();
        for d in &m.imap_dependencies {
            writeln!(out, "  {} -> {} (pack {})", name(&d.imap), name(&d.ityp), name(&d.pack_file)).unwrap();
        }
    }
    if !m.interiors.is_empty() {
        writeln!(out, "Interiors ({}):", m.interiors.len()).unwrap();
        for i in &m.interiors {
            writeln!(out, "  {} -> {}", name(&i.name), list(&i.bounds)).unwrap();
        }
    }
    if !m.hd_txd_bindings.is_empty() {
        writeln!(out, "HD texture bindings ({}):", m.hd_txd_bindings.len()).unwrap();
        for b in &m.hd_txd_bindings {
            writeln!(out, "  {} {} -> {}", name(&b.asset_type), b.target_asset, b.hd_txd).unwrap();
        }
    }
    if !m.map_data_groups.is_empty() {
        writeln!(out, "Map data groups ({}):", m.map_data_groups.len()).unwrap();
        for g in &m.map_data_groups {
            writeln!(out, "  {} flags 0x{:X} hours 0x{:X} bounds [{}] weather [{}]", name(&g.name), g.flags, g.hours_on_off, list(&g.bounds), list(&g.weather_types)).unwrap();
        }
    }
}

/// Same line format as `rpf textures`, so the two commands agree.
fn write_texture_line(out: &mut String, tex: &YtdTexture) {
    use std::fmt::Write;
    let name = if tex.name.is_empty() { format!("0x{:08X}", tex.name_hash) } else { tex.name.clone() };
    writeln!(
        out, "  {} — {}x{}x{} {} {} mip(s) ({} bytes)",
        name, tex.width, tex.height, tex.depth, tex.format, tex.levels, tex.pixel_data.len(),
    ).unwrap();
}

fn write_drawable(out: &mut String, entry: &DrawableEntry, verbose: bool) {
    use std::fmt::Write;
    let d = &entry.drawable;

    writeln!(out).unwrap();
    writeln!(out, "  {} (0x{:08X})", if d.name.is_empty() { &entry.name } else { &d.name }, entry.hash).unwrap();

    let (bounds, computed) = match d.best_lod() {
        Some(lod) => d.bounds_or_computed(lod),
        None => (d.bounds.clone(), false),
    };
    let c = bounds.center;
    writeln!(out, "    bounds:   center ({:.3}, {:.3}, {:.3}) radius {:.3}{}",
             c.x, c.y, c.z, bounds.sphere_radius, if computed { " (computed)" } else { "" }).unwrap();
    writeln!(out, "              min ({:.3}, {:.3}, {:.3}) max ({:.3}, {:.3}, {:.3})",
             bounds.box_min.x, bounds.box_min.y, bounds.box_min.z,
             bounds.box_max.x, bounds.box_max.y, bounds.box_max.z).unwrap();
    writeln!(out, "    lod dist: {:.1} / {:.1} / {:.1} / {:.1}",
             d.lod_distances[0], d.lod_distances[1], d.lod_distances[2], d.lod_distances[3]).unwrap();

    if verbose {
        if let Some(lod) = d.best_lod() {
            let geoms = d.geometry_bounds(lod);
            if !geoms.is_empty() {
                writeln!(out, "    geometry bounds:").unwrap();
                for g in &geoms {
                    writeln!(out, "      #{} model {} shader {}   {} tris, {} verts",
                             g.geometry, g.model, g.shader_id, g.triangles, g.vertices).unwrap();
                    writeln!(out, "           min ({:.3}, {:.3}, {:.3}) max ({:.3}, {:.3}, {:.3}) centroid ({:.3}, {:.3}, {:.3})",
                             g.min.x, g.min.y, g.min.z, g.max.x, g.max.y, g.max.z,
                             g.centroid.x, g.centroid.y, g.centroid.z).unwrap();
                }
            }
        }
    }

    for lod in &d.lods {
        let geometries: usize = lod.models.iter().map(|m| m.geometries.len()).sum();
        writeln!(out, "    {:<8} {} models, {} geometries, {} triangles",
                 format!("{}:", lod.level), lod.models.len(), geometries, d.triangle_count(lod)).unwrap();
    }

    if let Some(group) = &d.shader_group {
        writeln!(out, "    shaders:  {}", group.shaders.len()).unwrap();
        for (id, shader) in group.shaders.iter().enumerate() {
            let diffuse = d.diffuse_texture_name(id as u16).unwrap_or("-");
            writeln!(out, "      #{:<3} name 0x{:08X}  file 0x{:08X}  bucket {}  diffuse {}",
                     id, shader.name_hash, shader.file_name_hash, shader.render_bucket, diffuse).unwrap();
        }
        writeln!(out, "    embedded textures: {}", group.textures.len()).unwrap();
        for tex in &group.textures {
            out.push_str("  ");
            write_texture_line(out, tex);
        }
    } else {
        writeln!(out, "    shaders:  none").unwrap();
    }
}

// ─── json ────────────────────────────────────────────────────────────────────

fn write_json(out: &mut String, args: &InfoArgs, container: &Container, contents: &Contents, names: &NameTable, checks: &Checks, verbose: bool) {
    use std::fmt::Write;

    let (kind, body) = match contents {
        Contents::Other => ("other", String::new()),
        Contents::Textures(textures) => ("textures", format!(",\"textures\":{}", json_textures(textures))),
        Contents::Bounds(info) => {
            let children = info.children.map_or(String::new(), |n| format!(",\"children\":{n}"));
            ("bounds", format!(",\"bound\":{{\"kind\":\"{}\"{children}}}", info.kind.name()))
        }
        Contents::Drawables(entries, extras) => {
            let items: Vec<String> = entries.iter().map(|e| json_drawable(e, extras.as_ref(), verbose)).collect();
            ("drawables", format!(",\"drawables\":[{}]", items.join(",")))
        }
        Contents::Paths(ynd) => ("paths", format!(",\"paths\":{}", crate::commands::paths::json_summary(ynd, rage_formats::ynd_area_id_from_file_name(&args.file)).dump())),
        Contents::Map(ymap) => ("map", format!(",\"map\":{}", json_map(ymap, names, checks).dump())),
        Contents::Types(ytyp) => ("types", format!(",\"types\":{}", json_types(ytyp, names).dump())),
        Contents::Manifest(m) => ("manifest", format!(",\"manifest\":{}", json_manifest(m, names, checks).dump())),
        Contents::Meta(dump) => ("meta", format!(",\"meta\":{}", to_json(&dump.root, names).dump())),
    };

    let header = match container {
        Container::Rsc7(h) => format!(
            "\"format\":\"RSC7\",\"version\":{},\"system_flags\":\"0x{:08X}\",\"system_size\":{},\"graphics_flags\":\"0x{:08X}\",\"graphics_size\":{},\"body_bytes\":{},\"compressed\":{}",
            h.version, h.system_flags, h.system_size, h.graphics_flags, h.graphics_size, h.body_len, h.compressed
        ),
        Container::Meta(kind) => format!("\"format\":{}", json_string(&kind.to_string())),
    };
    write!(
        out,
        "{{\"file\":{},\"archive\":{},{header},\"kind\":\"{kind}\"{body}}}\n",
        json_string(&args.file),
        args.archive.as_ref().map_or("null".to_string(), |a| json_string(&a.to_string_lossy())),
    )
    .unwrap();
}

fn json_vec(v: Vec3) -> json::JsonValue {
    json::array![v.x, v.y, v.z]
}

fn json_name(hash: u32, names: &NameTable) -> json::JsonValue {
    if hash == 0 {
        json::JsonValue::Null
    } else {
        json::JsonValue::from(names.resolve(hash).as_ref())
    }
}

fn json_map(ymap: &Ymap, names: &NameTable, checks: &Checks) -> json::JsonValue {
    let h = &ymap.header;
    let (outside, unstreamed) = crate::extents::strays(ymap);
    let entities: Vec<json::JsonValue> = ymap
        .entities
        .iter()
        .map(|e| {
            json::object! {
                archetype: json_name(e.archetype_hash, names),
                archetype_hash: format!("0x{:08X}", e.archetype_hash),
                position: json_vec(e.position),
                rotation: json::array![e.rotation[0], e.rotation[1], e.rotation[2], e.rotation[3]],
                yaw_degrees: entity_yaw_degrees(e),
                scale_xy: e.scale_xy,
                scale_z: e.scale_z,
                lod_dist: e.lod_dist,
                child_lod_dist: e.child_lod_dist,
                lod_level: e.lod_level,
                num_children: e.num_children,
                parent_index: e.parent_index,
                flags: e.flags,
                flag_names: entity_flag_names(e.flags),
                guid: e.guid,
                mlo_instance: e.is_mlo_instance,
            }
        })
        .collect();
    let instances: Vec<json::JsonValue> = ymap
        .mlo_instances
        .iter()
        .map(|i| {
            json::object! {
                archetype: json_name(i.entity.archetype_hash, names),
                position: json_vec(i.entity.position),
                group_id: i.group_id,
                floor_id: i.floor_id,
                default_entity_sets: i.default_entity_sets.iter().map(|h| json_name(*h, names)).collect::<Vec<_>>(),
                num_exit_portals: i.num_exit_portals,
            }
        })
        .collect();
    json::object! {
        name: json_name(h.name_hash, names),
        name_hash: format!("0x{:08X}", h.name_hash),
        file_name_mismatch: checks.map_name_mismatch.as_deref().map_or(json::JsonValue::Null, json::JsonValue::from),
        parent: json_name(h.parent_hash, names),
        flags: h.flags,
        flag_names: header_flag_names(h.flags),
        content_flags: h.content_flags,
        content_flag_names: h.content_flag_names(),
        streaming_extents: json::array![json_vec(h.streaming_extents_min), json_vec(h.streaming_extents_max)],
        entities_extents: json::array![json_vec(h.entities_extents_min), json_vec(h.entities_extents_max)],
        entities_outside_extents: outside,
        entities_outside_streaming_extents: unstreamed,
        entities: entities,
        mlo_instances: instances,
    }
}

fn json_types(ytyp: &Ytyp, names: &NameTable) -> json::JsonValue {
    let archetypes: Vec<json::JsonValue> = ytyp
        .archetypes
        .iter()
        .map(|a| {
            json::object! {
                name: json_name(a.name_hash, names),
                name_hash: format!("0x{:08X}", a.name_hash),
                texture_dictionary: json_name(a.texture_dict_hash, names),
                lod_dist: a.lod_dist,
                bb_min: json_vec(a.bb_min),
                bb_max: json_vec(a.bb_max),
                mlo: a.is_mlo,
            }
        })
        .collect();
    let mlos: Vec<json::JsonValue> = ytyp
        .mlos
        .iter()
        .map(|m| {
            json::object! {
                name: json_name(m.name_hash, names),
                rooms: m.rooms.iter().map(|r| json::object! { name: r.name.as_str(), props: r.attached_objects.len(), bb_min: json_vec(r.bb_min), bb_max: json_vec(r.bb_max) }).collect::<Vec<_>>(),
                portals: m.portals.len(),
                entities: m.entities.len(),
                entity_sets: m.entity_sets.iter().map(|s| json_name(s.name_hash, names)).collect::<Vec<_>>(),
            }
        })
        .collect();
    json::object! { archetypes: archetypes, mlos: mlos }
}

fn json_manifest(m: &Manifest, names: &NameTable, checks: &Checks) -> json::JsonValue {
    let name = |h: &rage_formats::HashName| match &h.name {
        Some(n) => json::JsonValue::from(n.as_str()),
        None => json_name(h.hash, names),
    };
    let list = |v: &[rage_formats::HashName]| v.iter().map(name).collect::<Vec<_>>();
    let deps = |v: &[rage_formats::Dependencies]| {
        v.iter().map(|d| json::object! { name: name(&d.name), manifest_flags: d.manifest_flags, ityp_deps: list(&d.ityp_deps) }).collect::<Vec<_>>()
    };
    let orphan = |(n, known): &(String, bool)| json::object! { name: n.as_str(), known_name: *known };
    json::object! {
        map_data_groups: m.map_data_groups.iter().map(|g| json::object! { name: name(&g.name), flags: g.flags, hours_on_off: g.hours_on_off, bounds: list(&g.bounds), weather_types: list(&g.weather_types) }).collect::<Vec<_>>(),
        imap_dependencies: m.imap_dependencies.iter().map(|d| json::object! { imap: name(&d.imap), ityp: name(&d.ityp), pack_file: name(&d.pack_file) }).collect::<Vec<_>>(),
        imap_dependencies_2: deps(&m.imap_dependencies_2),
        imaps_not_here: checks.orphan_imaps.iter().map(orphan).collect::<Vec<_>>(),
        ityp_dependencies_2: deps(&m.ityp_dependencies_2),
        hd_txd_bindings: m.hd_txd_bindings.iter().map(|b| json::object! { asset_type: name(&b.asset_type), target_asset: b.target_asset.as_str(), hd_txd: b.hd_txd.as_str() }).collect::<Vec<_>>(),
        interiors: m.interiors.iter().map(|i| json::object! { name: name(&i.name), bounds: list(&i.bounds) }).collect::<Vec<_>>(),
    }
}

fn json_textures(textures: &[YtdTexture]) -> String {
    let items: Vec<String> = textures.iter().map(|tex| format!(
        "{{\"name\":{},\"hash\":\"0x{:08X}\",\"width\":{},\"height\":{},\"depth\":{},\"format\":\"{}\",\"mips\":{},\"bytes\":{}}}",
        json_string(&tex.name), tex.name_hash, tex.width, tex.height, tex.depth,
        tex.format, tex.levels, tex.pixel_data.len(),
    )).collect();
    format!("[{}]", items.join(","))
}

fn json_vec3(v: &rage_formats::Vec3) -> String {
    format!("[{},{},{}]", v.x, v.y, v.z)
}

fn json_geometry_bounds(geoms: &[rage_formats::GeometryBounds]) -> String {
    let items: Vec<String> = geoms.iter().map(|g| format!(
        "{{\"model\":{},\"geometry\":{},\"shader\":{},\"vertices\":{},\"triangles\":{},\"min\":{},\"max\":{},\"centroid\":{}}}",
        g.model, g.geometry, g.shader_id, g.vertices, g.triangles,
        json_vec3(&g.min), json_vec3(&g.max), json_vec3(&g.centroid),
    )).collect();
    format!("[{}]", items.join(","))
}

fn json_drawable(entry: &DrawableEntry, extras: Option<&drawable::DrawableExtras>, verbose: bool) -> String {
    let d = &entry.drawable;
    let (bounds, computed) = match d.best_lod() {
        Some(lod) => d.bounds_or_computed(lod),
        None => (d.bounds.clone(), false),
    };

    let geometry_bounds = if verbose {
        d.best_lod().map(|lod| format!(",\"geometry_bounds\":{}", json_geometry_bounds(&d.geometry_bounds(lod))))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let lods: Vec<String> = d.lods.iter().map(|lod| {
        let geometries: usize = lod.models.iter().map(|m| m.geometries.len()).sum();
        format!("{{\"level\":\"{}\",\"models\":{},\"geometries\":{},\"triangles\":{}}}",
                lod.level, lod.models.len(), geometries, d.triangle_count(lod))
    }).collect();

    let (shaders, textures) = match &d.shader_group {
        Some(group) => {
            let shaders: Vec<String> = group.shaders.iter().enumerate().map(|(id, s)| format!(
                "{{\"id\":{},\"name_hash\":\"0x{:08X}\",\"file_name_hash\":\"0x{:08X}\",\"render_bucket\":{},\"diffuse\":{}}}",
                id, s.name_hash, s.file_name_hash, s.render_bucket,
                d.diffuse_texture_name(id as u16).map_or("null".to_string(), json_string),
            )).collect();
            (format!("[{}]", shaders.join(",")), json_textures(&group.textures))
        }
        None => ("[]".to_string(), "[]".to_string()),
    };

    format!(
        "{{\"name\":{},\"hash\":\"0x{:08X}\",\"bounds\":{{\"center\":{},\"radius\":{},\"min\":{},\"max\":{},\"computed\":{}}}{},\"lod_distances\":[{},{},{},{}],\"lods\":[{}],\"shaders\":{},\"textures\":{}{}}}",
        json_string(if d.name.is_empty() { &entry.name } else { &d.name }), entry.hash,
        json_vec3(&bounds.center), bounds.sphere_radius, json_vec3(&bounds.box_min), json_vec3(&bounds.box_max), computed,
        geometry_bounds,
        d.lod_distances[0], d.lod_distances[1], d.lod_distances[2], d.lod_distances[3],
        lods.join(","), shaders, textures, extras.map(drawable::DrawableExtras::json_members).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(magic: &[u8; 4], version: u32, sys: u32, gfx: u32, body: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(magic);
        data.extend_from_slice(&version.to_le_bytes());
        data.extend_from_slice(&sys.to_le_bytes());
        data.extend_from_slice(&gfx.to_le_bytes());
        data.extend_from_slice(body);
        data
    }

    #[test]
    fn parses_a_stored_header() {
        let data = header(b"RSC7", 165, 0xA800_0004, 0x5000_0000, &[0u8; 8192]);
        let h = parse_header(&data).unwrap();
        assert_eq!(h, Rsc7Header {
            version: 165, system_flags: 0xA800_0004, graphics_flags: 0x5000_0000,
            system_size: 8192, graphics_size: 0, compressed: false, body_len: 8192,
        });
    }

    #[test]
    fn rejects_bad_magic() {
        let err = parse_header(&header(b"XXXX", 0, 0, 0, &[])).unwrap_err().to_string();
        assert!(err.contains("0x58585858"), "{err}");
        let err = detect(&header(b"XXXX", 0, 0, 0, &[])).unwrap_err().to_string();
        assert!(err.contains("0x58585858"), "{err}");
    }

    #[test]
    fn names_fivem_escrow_files() {
        let err = parse_header(&header(b"FXAP", 0, 0, 0, &[0u8; 32])).unwrap_err().to_string();
        assert!(err.contains("FiveM escrow"), "{err}");
    }

    #[test]
    fn rejects_rsc8() {
        let err = parse_header(&header(b"RSC8", 0, 0, 0, &[])).unwrap_err().to_string();
        assert!(err.contains("RSC8"), "{err}");
    }

    #[test]
    fn rejects_short_input() {
        assert!(parse_header(b"RSC7").is_err());
        assert!(detect(b"RS").is_err());
    }

    #[test]
    fn detects_the_metadata_containers() {
        assert_eq!(detect(b"PSIN\0\0\0\x10\0\0\0\0").unwrap(), Container::Meta(MetaContainer::Pso));
        assert_eq!(detect(b"RBF0\0\0\0\0").unwrap(), Container::Meta(MetaContainer::Rbf));
        assert_eq!(detect(b"<?xml version=\"1.0\"?><CPackFileMetaData/>").unwrap(), Container::Meta(MetaContainer::Xml));
        assert!(matches!(detect(&header(b"RSC7", 2, 0x0800_0004, 0, &[0u8; 8192])).unwrap(), Container::Rsc7(_)));
    }

    #[test]
    fn yaw_is_measured_from_the_conjugate() {
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let e = |rotation: [f32; 4]| YmapEntity {
            archetype_hash: 0, flags: 0, guid: 0, position: Vec3::new(0.0, 0.0, 0.0), rotation,
            scale_xy: 1.0, scale_z: 1.0, parent_index: -1, lod_dist: 0.0,
            child_lod_dist: -1.0, lod_level: 0, num_children: 0, is_mlo_instance: false,
        };
        assert!((entity_yaw_degrees(&e([0.0, 0.0, 0.0, 1.0]))).abs() < 1e-4);
        // A stored +90° about z is a -90° heading in the world.
        assert!((entity_yaw_degrees(&e([0.0, 0.0, h, h])) + 90.0).abs() < 1e-3);
        assert!((entity_yaw_degrees(&e([0.0, 0.0, -0.17364818, 0.9848077])) - 20.0).abs() < 1e-3);
    }

    #[test]
    fn xml_rename_touches_whole_values_only() {
        let (out, n) = rename_in_xml("<a><imapName>map1</imapName><x>map10</x><y name=\"MAP1\"/>map1 </a>", "map1", "beach");
        assert_eq!(n, 2);
        assert_eq!(out, "<a><imapName>beach</imapName><x>map10</x><y name=\"beach\"/>map1 </a>");
        assert_eq!(rename_in_xml("<a/>", "map1", "b").1, 0);
    }

    #[test]
    fn from_hashes_take_a_hash_or_both_spellings() {
        assert_eq!(from_hashes("0xAEC13995"), vec![0xAEC13995]);
        assert_eq!(from_hashes("map1"), vec![rage_joaat("map1")]);
        assert_eq!(from_hashes("bombaPALETO"), vec![rage_joaat("bombaPALETO"), rage_joaat("bombapaleto")]);
    }

    #[test]
    fn unresolved_note_counts_placeholders_only() {
        assert!(unresolved_note("Map: paleto_props\n").is_none());
        let note = unresolved_note("Map: hash_AEC13995\n  hash_4FD621BC x\n  hash_not_one\n").unwrap();
        assert!(note.starts_with("Unresolved: 2 hash(es)"), "{note}");
    }

    #[test]
    fn entity_flags_are_named_by_bit() {
        assert_eq!(entity_flag_names(0x20), ["static entity"]);
        assert_eq!(entity_flag_names(0), Vec::<&str>::new());
        assert_eq!(entity_flag_names(1 | 1 << 5), ["allow full rotation", "static entity"]);
    }
}
