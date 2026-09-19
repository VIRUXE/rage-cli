use anyhow::{Context, Result};
use std::path::PathBuf;

use rage_formats::{encode_image, fit_max_size, image, to_rgba_image, DrawableKind, ImageFormat,
                   YtdTexture};
use rage_render::{compose_sheet, SheetItem, SheetOptions};

use crate::resources::{embedded_textures, extension_of, file_stem, load_drawables, load_texture_dictionary,
                       sanitize};
use crate::rpf::{Archive, GtaKeys};

#[derive(clap::Args)]
pub struct TexturesArgs {
    /// Path to the RPF archive
    pub archive: PathBuf,

    /// Name of a .ytd, .ydr, .ydd or .yft inside the archive (e.g. "vehicles.ytd")
    pub file: String,

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

/// A texture together with the name it should be exported under.
struct Named<'a> {
    name: String,
    tex: &'a YtdTexture,
}

fn textures_from_file(archive: &Archive, args: &TexturesArgs, keys: Option<&GtaKeys>) -> Result<Vec<YtdTexture>> {
    let ext = extension_of(&args.file);

    if ext.eq_ignore_ascii_case("ytd") {
        return load_texture_dictionary(archive, &args.file, keys);
    }

    if let Some(kind) = DrawableKind::from_extension(ext) {
        let entries = load_drawables(archive, &args.file, kind, keys)?;
        return Ok(embedded_textures(&entries).into_iter().cloned().collect());
    }

    anyhow::bail!("'{}' is not a .ytd, .ydr, .ydd or .yft file", args.file);
}

fn texture_name(tex: &YtdTexture) -> String {
    if tex.name.is_empty() {
        format!("0x{:08X}", tex.name_hash)
    } else {
        tex.name.clone()
    }
}

pub fn run(args: &TexturesArgs, keys: Option<&GtaKeys>) -> Result<()> {
    let archive = Archive::open(&args.archive, keys)?;
    archive.require_keys(keys)?;

    let textures = textures_from_file(&archive, args, keys)?;

    if textures.is_empty() {
        println!("No textures found");
        return Ok(());
    }

    let stem = file_stem(&args.file);
    let out_dir = args.output.clone().unwrap_or_else(|| PathBuf::from(&stem));
    std::fs::create_dir_all(&out_dir)?;

    let named: Vec<Named<'_>> = textures.iter().map(|tex| Named { name: texture_name(tex), tex }).collect();

    let mut exported = 0usize;
    let mut failed = 0usize;
    let mut sheet_items: Vec<(String, image::RgbaImage, &YtdTexture)> = Vec::new();

    for Named { name, tex } in &named {
        println!(
            "  {} — {}x{}x{} {} {} mip(s) ({} bytes)",
            name,
            tex.width, tex.height, tex.depth,
            tex.format,
            tex.levels,
            tex.pixel_data.len(),
        );

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
