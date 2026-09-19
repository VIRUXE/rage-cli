use anyhow::{Context, Result};
use std::path::PathBuf;

use rage_formats::{encode_image, wheel_slot, DrawableEntry, ImageFormat, LodLevel};
use rage_render::{compose_sheet, render_parts, RenderOptions, RenderPart, SheetItem, SheetOptions,
                  TextureSet, View};

use crate::index::GameIndex;
use crate::resources::{embedded_textures_of, file_stem, load_renderables, load_texture_dictionary, sanitize,
                       Loaded};
use crate::rpf::{Archive, GtaKeys};

/// JPEG quality used for the rendered images (PNG/WebP ignore it).
const QUALITY: u8 = 90;

/// Largest grid cell, in pixels, regardless of how big the views are.
const MAX_GRID_CELL: u32 = 512;

#[derive(clap::Args)]
pub struct ScreenshotArgs {
    /// Path to the RPF archive
    pub archive: PathBuf,

    /// Name of a .ydr, .ydd or .yft inside the archive
    pub file: String,

    /// Output directory (default: current directory)
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,

    /// Texture dictionary to use for textures that are not embedded: a name inside the archive
    /// (".ytd" optional) or a path to a loose .ytd file. Repeatable; earlier ones win.
    #[arg(long, value_name = "NAME|PATH")]
    pub ytd: Vec<String>,

    /// Views to render: front, back, left, right, top, iso (comma separated)
    #[arg(long, value_delimiter = ',', default_value = "iso")]
    pub views: Vec<View>,

    /// Image size as WxH
    #[arg(long, default_value = "1024x1024", value_parser = parse_size)]
    pub size: (u32, u32),

    /// Also combine all views into one labelled grid image (always on a dark
    /// background so the labels stay legible, whatever --background is)
    #[arg(long)]
    pub grid: bool,

    /// LOD to render: high, medium, low, verylow
    #[arg(long, default_value = "high")]
    pub lod: LodLevel,

    /// Image format: png, jpg, webp
    #[arg(short, long, default_value = "png")]
    pub format: ImageFormat,

    /// Background: "grey" (default), "transparent", or #rrggbb
    #[arg(long, default_value = "grey", value_parser = parse_background)]
    pub background: [u8; 4],

    /// Cull back faces (off by default: many GTA surfaces are single-sided planes)
    #[arg(long)]
    pub cull: bool,

    /// Multiply vertex colours into the diffuse
    #[arg(long)]
    pub vertex_colors: bool,

    /// For .ydd/.yft: render only the entry with this name or 0x hash
    #[arg(long)]
    pub entry: Option<String>,

    /// Body colour (#rrggbb) for vehicle paint shaders, which otherwise render white
    #[arg(long, value_name = "#RRGGBB", value_parser = parse_paint)]
    pub paint: Option<[u8; 3]>,

    /// Skip the game-wide texture index: only --ytd and the same-stem guess
    /// are tried for textures the embedded dictionary doesn't have
    #[arg(long)]
    pub no_index: bool,

    /// Frame the camera on the whole model even when it is really several
    /// pieces scattered far apart, instead of the piece with the bulk of the
    /// geometry (the far pieces are always drawn either way)
    #[arg(long)]
    pub no_cluster_framing: bool,
}

/// Parses a `#rrggbb` paint colour.
fn parse_paint(value: &str) -> Result<[u8; 3], String> {
    let hex = value
        .trim()
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| format!("expected #rrggbb, got '{value}'"))?;

    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).unwrap_or(0);
    Ok([byte(0), byte(2), byte(4)])
}

/// Parses a `WxH` size such as `"1280x720"`.
fn parse_size(value: &str) -> Result<(u32, u32), String> {
    let (width, height) = value
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("expected WxH (e.g. 1280x720), got '{value}'"))?;

    let parse = |part: &str, what: &str| -> Result<u32, String> {
        let n: u32 = part
            .trim()
            .parse()
            .map_err(|_| format!("invalid {what} '{part}' in '{value}'"))?;
        if n == 0 {
            return Err(format!("{what} must be greater than zero in '{value}'"));
        }
        Ok(n)
    };

    Ok((parse(width, "width")?, parse(height, "height")?))
}

/// Parses a background colour: `grey`/`gray`, `transparent`, or `#rrggbb`.
fn parse_background(value: &str) -> Result<[u8; 4], String> {
    let trimmed = value.trim();

    match trimmed.to_ascii_lowercase().as_str() {
        "grey" | "gray" => return Ok([230, 230, 230, 255]),
        "transparent" => return Ok([0, 0, 0, 0]),
        _ => {}
    }

    let hex = trimmed
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| {
            format!("expected 'grey', 'transparent' or '#rrggbb', got '{value}'")
        })?;

    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).unwrap_or(0);
    Ok([byte(0), byte(2), byte(4), 255])
}

/// Strips RAGE's internal `.#dr`/`.#dd`/`.#ft` suffix (marking the resource
/// type of the entry the name was read from) so labels, file names and
/// `--entry` filters see the plain name underneath.
fn strip_rage_suffix(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    for suffix in [".#dr", ".#dd", ".#ft"] {
        if lower.ends_with(suffix) {
            return &name[..name.len() - suffix.len()];
        }
    }
    name
}

/// True when `filter` selects the entry: a case-insensitive name match
/// (ignoring RAGE's internal suffix on either side), or a `0x…` literal equal
/// to the entry's hash.
fn entry_matches(name: &str, hash: u32, filter: &str) -> bool {
    let filter = filter.trim();

    if let Some(hex) = filter.strip_prefix("0x").or_else(|| filter.strip_prefix("0X")) {
        if let Ok(wanted) = u32::from_str_radix(hex, 16) {
            return wanted == hash;
        }
    }

    strip_rage_suffix(name).eq_ignore_ascii_case(strip_rage_suffix(filter))
}

/// Builds an output file name: `<stem>[_<entry>][_<view>].<ext>`. `entry` is
/// given only when several entries are rendered, `view` only when several
/// views are (or for the grid, where it is `"grid"`).
fn image_file_name(stem: &str, entry: Option<&str>, view: Option<&str>, ext: &str) -> String {
    let mut name = stem.to_string();
    if let Some(entry) = entry {
        name.push('_');
        name.push_str(&sanitize(entry));
    }
    if let Some(view) = view {
        name.push('_');
        name.push_str(view);
    }
    format!("{name}.{ext}")
}

/// The views to render, each once, in the order first asked for; nothing
/// asked for means the default iso view.
fn unique_views(views: &[View]) -> Vec<View> {
    let mut unique: Vec<View> = Vec::with_capacity(views.len());
    for view in views {
        if !unique.contains(view) {
            unique.push(*view);
        }
    }
    if unique.is_empty() {
        unique.push(View::Iso);
    }
    unique
}

/// The one-line summary printed per rendered entry. Geometries whose shader
/// names no diffuse texture are called out separately from the untextured
/// total, since no `--ytd` can supply one; the clause is left out when
/// there are none. `parts` is the composite suffix, already comma-led.
fn summary_line(label: &str, report: &rage_render::RenderReport, parts: &str) -> String {
    let no_diffuse = if report.geometries_without_diffuse > 0 {
        format!(", {} with no diffuse", report.geometries_without_diffuse)
    } else {
        String::new()
    };
    let bounds = if report.framing_excluded_geometries > 0 {
        format!(
            "clustered (1 of {} islands, {} geometr{} excluded)",
            report.framing_islands,
            report.framing_excluded_geometries,
            if report.framing_excluded_geometries == 1 { "y" } else { "ies" },
        )
    } else if report.bounds_computed {
        "computed".to_string()
    } else {
        "from file".to_string()
    };
    format!(
        "{}: {} triangles, {} geometries ({} untextured{}), lod {}, bounds {}{}",
        label,
        report.triangles,
        report.geometries,
        report.untextured_geometries,
        no_diffuse,
        report.lod.map(|lod| lod.as_str()).unwrap_or("none"),
        bounds,
        parts,
    )
}

/// The label an entry is reported and named by.
fn entry_label(entry: &DrawableEntry) -> String {
    label_for(&entry.name, entry.hash)
}

fn label_for(name: &str, hash: u32) -> String {
    if name.is_empty() {
        format!("0x{hash:08X}")
    } else {
        strip_rage_suffix(name).to_string()
    }
}

/// One image set to render: a plain drawable, or a fragment's body with its
/// wheels and doors placed on it.
struct Renderable<'a> {
    label: String,
    hash: u32,
    parts: Vec<RenderPart<'a>>,
    /// Wheel slots drawn (including ones filled from another wheel's mesh).
    wheels: usize,
}

/// Lists what to render: each .ydr/.ydd entry on its own; a fragment as one
/// composite of body plus physics children, followed by its extra drawables.
fn renderables(loaded: &Loaded) -> Vec<Renderable<'_>> {
    match loaded {
        Loaded::Entries(entries) => entries
            .iter()
            .map(|entry| Renderable {
                label: entry_label(entry),
                hash: entry.hash,
                parts: vec![RenderPart::new(&entry.drawable)],
                wheels: 0,
            })
            .collect(),
        Loaded::Fragment(fragment) => {
            let mut out = Vec::new();
            if let Some(body) = &fragment.drawable {
                let name = if fragment.name.is_empty() { &body.name } else { &fragment.name };
                let fragment_parts = fragment.render_parts();
                let wheels = fragment_parts
                    .iter()
                    .filter(|part| part.bone_tag.is_some_and(|tag| wheel_slot(tag).is_some()))
                    .count();
                out.push(Renderable {
                    label: label_for(name, body.name_hash),
                    hash: body.name_hash,
                    parts: fragment_parts.into_iter().map(RenderPart::from).collect(),
                    wheels,
                });
            }
            for extra in &fragment.extra_drawables {
                out.push(Renderable {
                    label: entry_label(extra),
                    hash: extra.hash,
                    parts: vec![RenderPart::new(&extra.drawable)],
                    wheels: 0,
                });
            }
            out
        }
    }
}

/// Builds the texture set: embedded textures first, then one layer per
/// `--ytd`, then whatever the game-wide index resolves (or, with no index
/// available, the same-stem guess this always made).
fn build_texture_set(
    archive: &Archive,
    args: &ScreenshotArgs,
    loaded: &Loaded,
    keys: Option<&GtaKeys>,
    index: Option<&GameIndex>,
) -> TextureSet {
    let mut set = TextureSet::new();

    let embedded: Vec<_> = embedded_textures_of(loaded.drawables()).into_iter().cloned().collect();
    report_failed(&set.push_layer(&embedded), "embedded");

    for spec in &args.ytd {
        match load_texture_dictionary(archive, spec, keys) {
            Ok(textures) => {
                println!("Using texture dictionary {} ({} texture(s))", spec, textures.len());
                report_failed(&set.push_layer(&textures), spec);
            }
            Err(err) => eprintln!("warning: failed to load texture dictionary '{spec}': {err}"),
        }
    }

    let Some(index) = index else {
        // No index available (--no-index, or no --exe/--keys to build one
        // from): fall back to the one guess rpf-cli has always made — a
        // same-name .ytd in this same archive.
        let fallback = format!("{}.ytd", file_stem(&args.file));
        match load_texture_dictionary(archive, &fallback, keys) {
            Ok(textures) => {
                println!("Using texture dictionary {} ({} texture(s))", fallback, textures.len());
                report_failed(&set.push_layer(&textures), &fallback);
            }
            Err(err) => println!("No texture dictionary {} found: {}", fallback, err),
        }
        return set;
    };

    // The index-driven order (archetype's own texture dictionary, then the
    // same-stem guess, then any parent chain) is a strict superset of the
    // no-index fallback above, searched game-wide instead of one archive.
    let stem_hash = rage_formats::rage_joaat(&file_stem(&args.file).to_lowercase());
    for txd_hash in index.resolution_order(stem_hash) {
        let Some(loc) = index.ytd_by_name.get(&txd_hash) else {
            log::debug!("no .ytd for txd hash {txd_hash:08X} in the resolution order");
            continue;
        };
        match index.load_bytes(loc, keys).and_then(|data| rage_formats::parse_ytd(&data).map_err(Into::into)) {
            Ok(textures) => {
                println!("Using texture dictionary {} ({} texture(s), via index)", loc.inner_path, textures.len());
                report_failed(&set.push_layer(&textures), &loc.inner_path);
            }
            Err(err) => eprintln!("warning: failed to load '{}' from index: {}", loc.inner_path, err),
        }
    }

    set
}

/// Appends the resident dictionaries (`mapdetail`/`vehshare`) that actually
/// hold one of `missing` as new lowest-priority layers on `set`, skipping any
/// already in `already_loaded`. Returns true when a layer was added, i.e.
/// when re-rendering could change the result.
///
/// CodeWalker's `Renderer.cs TryGetRenderable` consults its by-texture-name
/// resident index only after the whole txd chain; that index is populated
/// only from `mapdetail`/`vehshare` (`GameFileCache.InitGtxds`) rather than
/// from every streamed ytd. Loading both dictionaries unconditionally on
/// every render would be needlessly expensive — `mapdetail.ytd` is large and
/// `TextureSet::push_layer` fully decodes every texture to RGBA — so this is
/// only called once rendering has already reported a name still missing.
fn push_resident_fallback(
    set: &mut TextureSet,
    missing: &[String],
    index: &GameIndex,
    keys: Option<&GtaKeys>,
    already_loaded: &mut Vec<u32>,
) -> bool {
    let mut wanted: Vec<u32> = Vec::new();
    for name in missing {
        let Some(dict) = index.resident_dict_for_texture(rage_formats::rage_joaat(&name.to_lowercase())) else {
            continue;
        };
        if !already_loaded.contains(&dict) && !wanted.contains(&dict) {
            wanted.push(dict);
        }
    }
    if wanted.is_empty() {
        return false;
    }

    let mut added = false;
    for dict in wanted {
        already_loaded.push(dict);
        let Some(loc) = index.ytd_by_name.get(&dict) else {
            log::debug!("resident dictionary {dict:08X} has no .ytd in the index");
            continue;
        };
        match index.load_bytes(loc, keys).and_then(|d| rage_formats::parse_ytd(&d).map_err(Into::into)) {
            Ok(textures) => {
                println!("Using resident texture dictionary {} ({} texture(s), via index)", loc.inner_path, textures.len());
                report_failed(&set.push_layer(&textures), &loc.inner_path);
                added = true;
            }
            Err(err) => eprintln!("warning: failed to load resident dictionary '{}': {}", loc.inner_path, err),
        }
    }
    added
}

fn report_failed(failed: &[String], source: &str) {
    if !failed.is_empty() {
        eprintln!("warning: {} texture(s) in {} failed to decode: {}",
                  failed.len(), source, failed.join(", "));
    }
}

/// Loads the cached game-wide texture index, building and caching it if
/// there's none yet. `None` (with a warning, not an error — the same-stem
/// guess still applies) when there's no `--exe`/`GTAV_PATH` to find the game
/// directory from, or the build itself fails.
fn load_or_build_index(exe: Option<&std::path::Path>, keys: Option<&GtaKeys>) -> Option<GameIndex> {
    let exe = exe?;
    let exe_path = match crate::keys::resolve_exe(exe) {
        Ok(p) => p,
        Err(err) => { eprintln!("warning: couldn't resolve --exe for the texture index: {err}"); return None; }
    };
    let game_root = exe_path.parent()?.to_path_buf();

    let cache_path = GameIndex::cache_path(&exe_path);

    if let Some(path) = &cache_path
        && path.is_file()
    {
        match GameIndex::load_cached(path) {
            Ok(index) => return Some(index),
            Err(err) => eprintln!("texture index cache at {} is invalid ({err}); rebuilding", path.display()),
        }
    }

    println!("Building texture index for {} (one-off; cached for next time)...", game_root.display());
    let index = match GameIndex::build(&game_root, keys) {
        Ok(index) => index,
        Err(err) => { eprintln!("warning: failed to build texture index: {err}"); return None; }
    };
    println!("Texture index: {}", index.summary());

    if let Some(path) = &cache_path
        && let Err(err) = index.save_cached(path)
    {
        eprintln!("warning: failed to cache texture index: {err}");
    }

    Some(index)
}

pub fn run(args: &ScreenshotArgs, keys: Option<&GtaKeys>, exe: Option<&std::path::Path>) -> Result<()> {
    let archive = Archive::open(&args.archive, keys)?;
    archive.require_keys(keys)?;

    let loaded = load_renderables(&archive, &args.file, keys)?;
    let mut entries = renderables(&loaded);

    if let Some(filter) = &args.entry {
        entries.retain(|entry| entry_matches(&entry.label, entry.hash, filter));
        if entries.is_empty() {
            anyhow::bail!("no entry matching '{}' in '{}'", filter, args.file);
        }
    }

    if entries.is_empty() {
        anyhow::bail!("'{}' holds no drawables", args.file);
    }

    let index = if args.no_index { None } else { load_or_build_index(exe, keys) };
    let mut textures = build_texture_set(&archive, args, &loaded, keys, index.as_ref());
    let mut resident_loaded: Vec<u32> = Vec::new();

    let stem = file_stem(&args.file);
    let out_dir = args.output.clone().unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;

    let views = unique_views(&args.views);
    let (width, height) = args.size;
    let ext = args.format.extension();
    let many_entries = entries.len() > 1;
    let many_views = views.len() > 1;

    let options = RenderOptions {
        width,
        height,
        view: views[0],
        background: args.background,
        lod: args.lod,
        backface_cull: args.cull,
        vertex_colors: args.vertex_colors,
        paint: args.paint,
        cluster_framing: !args.no_cluster_framing,
        ..Default::default()
    };

    let mut written = 0usize;

    for entry in &entries {
        let label = &entry.label;
        let mut rendered = render_parts(&entry.parts, &textures, &options, &views)
            .with_context(|| format!("failed to render '{}'", label))?;

        let mut report = rendered
            .first()
            .map(|(_, _, report)| report.clone())
            .unwrap_or_default();

        // A texture still missing after the whole index-driven chain gets
        // one more chance: the resident dictionaries (mapdetail/vehshare),
        // by texture name rather than dictionary hash. Only re-render when
        // something was actually missing and a layer was genuinely added —
        // the overwhelmingly common case is nothing missing at all.
        if !report.missing_textures.is_empty()
            && let Some(index) = index.as_ref()
            && push_resident_fallback(&mut textures, &report.missing_textures, index, keys, &mut resident_loaded)
        {
            rendered = render_parts(&entry.parts, &textures, &options, &views)
                .with_context(|| format!("failed to re-render '{}'", label))?;
            report = rendered
                .first()
                .map(|(_, _, report)| report.clone())
                .unwrap_or_default();
        }

        let parts = if entry.parts.len() > 1 {
            format!(", {} parts ({} wheels)", entry.parts.len(), entry.wheels)
        } else {
            String::new()
        };
        println!("{}", summary_line(label, &report, &parts));

        if !report.missing_textures.is_empty() {
            println!("Missing textures:");
            for name in &report.missing_textures {
                println!("  {name}");
            }
        }

        let entry_part = many_entries.then(|| label.as_str());

        for (view, image, _) in &rendered {
            let view_part = many_views.then(|| view.label());
            let file_name = image_file_name(&stem, entry_part, view_part, ext);
            let path = out_dir.join(&file_name);
            let encoded = encode_image(image, args.format, QUALITY)?;
            std::fs::write(&path, encoded)
                .with_context(|| format!("failed to write {}", path.display()))?;
            written += 1;
        }

        if args.grid {
            let items: Vec<SheetItem<'_>> = rendered
                .iter()
                .map(|(view, image, _)| SheetItem { label: view.label().to_string(), image })
                .collect();

            // The sheet keeps its own dark background so the view labels stay
            // legible whatever the render background is.
            let sheet_options = SheetOptions {
                cell: width.min(height).min(MAX_GRID_CELL).max(1),
                ..Default::default()
            };

            let sheet = compose_sheet(&items, &sheet_options);
            let encoded = encode_image(&sheet, args.format, QUALITY)?;
            let file_name = image_file_name(&stem, entry_part, Some("grid"), ext);
            let path = out_dir.join(&file_name);
            std::fs::write(&path, encoded)
                .with_context(|| format!("failed to write {}", path.display()))?;
            written += 1;
        }
    }

    println!("Wrote {} image(s) to {}", written, out_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_accepts_wxh() {
        assert_eq!(parse_size("1280x720"), Ok((1280, 720)));
        assert_eq!(parse_size("1024X1024"), Ok((1024, 1024)));
        assert_eq!(parse_size("1x1"), Ok((1, 1)));
    }

    #[test]
    fn parse_size_rejects_junk() {
        assert!(parse_size("1280").is_err());
        assert!(parse_size("axb").is_err());
        assert!(parse_size("1280x").is_err());
        assert!(parse_size("0x512").is_err());
        assert!(parse_size("-4x4").is_err());
    }

    #[test]
    fn parse_background_named_colours() {
        assert_eq!(parse_background("grey"), Ok([230, 230, 230, 255]));
        assert_eq!(parse_background("GRAY"), Ok([230, 230, 230, 255]));
        assert_eq!(parse_background("transparent"), Ok([0, 0, 0, 0]));
    }

    #[test]
    fn parse_background_hex() {
        assert_eq!(parse_background("#ff8800"), Ok([255, 136, 0, 255]));
        assert_eq!(parse_background("#FF8800"), Ok([255, 136, 0, 255]));
        assert_eq!(parse_background("#000000"), Ok([0, 0, 0, 255]));
    }

    #[test]
    fn parse_paint_hex_only() {
        assert_eq!(parse_paint("#c81e1e"), Ok([200, 30, 30]));
        assert_eq!(parse_paint("#FFFFFF"), Ok([255, 255, 255]));
        assert!(parse_paint("red").is_err());
        assert!(parse_paint("#fff").is_err());
        assert!(parse_paint("transparent").is_err());
    }

    #[test]
    fn parse_background_rejects_junk() {
        assert!(parse_background("#ff88").is_err());
        assert!(parse_background("ff8800").is_err());
        assert!(parse_background("#gggggg").is_err());
        assert!(parse_background("puce").is_err());
    }

    #[test]
    fn entry_filter_matches_name_case_insensitively() {
        assert!(entry_matches("prop_barrel_01a", 0x1234_5678, "PROP_Barrel_01A"));
        assert!(!entry_matches("prop_barrel_01a", 0x1234_5678, "prop_barrel_01b"));
    }

    #[test]
    fn entry_filter_matches_hash() {
        assert!(entry_matches("prop_barrel_01a", 0x1234_5678, "0x12345678"));
        assert!(entry_matches("prop_barrel_01a", 0x1234_5678, "0X12345678"));
        assert!(!entry_matches("prop_barrel_01a", 0x1234_5678, "0xdeadbeef"));
        // A malformed 0x literal falls back to a name comparison.
        assert!(!entry_matches("prop_barrel_01a", 0x1234_5678, "0xzz"));
    }

    #[test]
    fn strip_rage_suffix_removes_known_suffixes() {
        assert_eq!(strip_rage_suffix("prop_x.#dr"), "prop_x");
        assert_eq!(strip_rage_suffix("prop_x.#DD"), "prop_x");
        assert_eq!(strip_rage_suffix("prop_x.#ft"), "prop_x");
        assert_eq!(strip_rage_suffix("prop_x"), "prop_x");
    }

    #[test]
    fn entry_filter_ignores_rage_suffix_on_either_side() {
        assert!(entry_matches("prop_x.#dr", 0x1234_5678, "prop_x"));
        assert!(entry_matches("prop_x", 0x1234_5678, "prop_x.#dr"));
        assert!(entry_matches("prop_x.#dr", 0x1234_5678, "prop_x.#dr"));
    }

    #[test]
    fn file_names_omit_parts_that_are_not_needed() {
        assert_eq!(image_file_name("prop_x", None, None, "png"), "prop_x.png");
        assert_eq!(image_file_name("prop_x", None, Some("iso"), "png"), "prop_x_iso.png");
        assert_eq!(image_file_name("dict", Some("part_a"), None, "jpg"), "dict_part_a.jpg");
        assert_eq!(
            image_file_name("dict", Some("part_a"), Some("front"), "webp"),
            "dict_part_a_front.webp"
        );
        assert_eq!(image_file_name("prop_x", None, Some("grid"), "png"), "prop_x_grid.png");
    }

    fn stub_drawable(name: &str, models: usize) -> rage_formats::Drawable {
        use rage_formats::{Drawable, DrawableBounds, DrawableLod, DrawableModel, Vec3};
        Drawable {
            name: name.to_string(),
            name_hash: rage_formats::rage_joaat(name),
            bounds: DrawableBounds { center: Vec3::ZERO, sphere_radius: 0.0, box_min: Vec3::ZERO, box_max: Vec3::ZERO },
            lod_distances: [0.0; 4],
            render_masks: [0; 4],
            shader_group: None,
            lods: vec![DrawableLod {
                level: LodLevel::High,
                models: (0..models)
                    .map(|_| DrawableModel { skeleton_binding: 0, render_mask_flags: 0, shader_mapping: Vec::new(), geometries: Vec::new() })
                    .collect(),
            }],
        }
    }

    /// A fragment renders as one entry made of its body and physics children
    /// (empty wheel slots filled in), followed by its extra drawables as
    /// entries of their own.
    #[test]
    fn fragment_becomes_one_composite_entry_plus_its_extras() {
        use rage_formats::{Fragment, FragmentChild, Mat4, Vec3};

        let child = |bone_tag: u16, drawable: Option<rage_formats::Drawable>| FragmentChild {
            group_index: 0,
            bone_tag,
            drawable,
            transform: Mat4::identity(),
        };
        let fragment = Fragment {
            name: "adder.#ft".to_string(),
            bound_center: Vec3::ZERO,
            bound_radius: 1.0,
            drawable: Some(stub_drawable("adder", 1)),
            extra_drawables: vec![DrawableEntry {
                hash: rage_formats::rage_joaat("adder_hi"),
                name: "adder_hi".to_string(),
                drawable: stub_drawable("adder_hi", 1),
            }],
            bone_transforms: Vec::new(),
            children: vec![
                child(27922, Some(stub_drawable("wheel_lf", 1))),
                child(26418, None),
                child(4321, Some(stub_drawable("door_dside_f", 1))),
                child(8765, None),
            ],
        };

        let loaded = Loaded::Fragment(fragment);
        let renderables = renderables(&loaded);

        assert_eq!(renderables.len(), 2);
        assert_eq!(renderables[0].label, "adder");
        assert_eq!(renderables[0].hash, rage_formats::rage_joaat("adder"));
        assert_eq!(renderables[0].parts.len(), 4, "body, two wheels and a door");
        assert_eq!(renderables[0].wheels, 2);
        assert_eq!(renderables[1].label, "adder_hi");
        assert_eq!(renderables[1].parts.len(), 1);
        assert_eq!(renderables[1].wheels, 0);
    }

    #[test]
    fn plain_entries_become_single_part_renderables() {
        let loaded = Loaded::Entries(vec![DrawableEntry {
            hash: 0x1234_5678,
            name: "prop_x.#dr".to_string(),
            drawable: stub_drawable("prop_x", 1),
        }]);
        let renderables = renderables(&loaded);
        assert_eq!(renderables.len(), 1);
        assert_eq!(renderables[0].label, "prop_x");
        assert_eq!(renderables[0].hash, 0x1234_5678);
        assert_eq!(renderables[0].parts.len(), 1);
    }

    /// `--views iso,front,iso` renders iso once; order of first appearance
    /// is kept and an empty list means the default iso.
    #[test]
    fn views_are_rendered_once_each_in_first_seen_order() {
        assert_eq!(unique_views(&[View::Iso, View::Front, View::Iso]), vec![View::Iso, View::Front]);
        assert_eq!(unique_views(&[View::Front, View::Front]), vec![View::Front]);
        assert_eq!(unique_views(&[]), vec![View::Iso]);
    }

    /// Geometries whose shader names no diffuse at all are called out, since
    /// no `--ytd` can fix them; when there are none the line is unchanged.
    #[test]
    fn summary_line_mentions_geometries_with_no_diffuse_only_when_present() {
        let mut report = rage_render::RenderReport::default();
        report.triangles = 10;
        report.geometries = 4;
        report.untextured_geometries = 2;
        report.lod = Some(LodLevel::High);

        assert_eq!(
            summary_line("prop", &report, ""),
            "prop: 10 triangles, 4 geometries (2 untextured), lod high, bounds from file"
        );

        report.geometries_without_diffuse = 1;
        assert_eq!(
            summary_line("prop", &report, ", 5 parts (4 wheels)"),
            "prop: 10 triangles, 4 geometries (2 untextured, 1 with no diffuse), lod high, bounds from file, 5 parts (4 wheels)"
        );
    }

    /// The "clustered" clause only replaces the bounds term when framing
    /// actually excluded something; a normal report (no exclusion) prints
    /// the old strings verbatim, keeping past batch runs comparable.
    #[test]
    fn summary_line_reports_clustered_framing() {
        let mut report = rage_render::RenderReport::default();
        report.triangles = 36;
        report.geometries = 2;
        report.lod = Some(LodLevel::High);
        report.framing_islands = 2;
        report.framing_excluded_geometries = 1;

        assert_eq!(
            summary_line("hei_bank_heist_card", &report, ""),
            "hei_bank_heist_card: 36 triangles, 2 geometries (0 untextured), lod high, bounds clustered (1 of 2 islands, 1 geometry excluded)"
        );

        report.framing_islands = 3;
        report.framing_excluded_geometries = 2;
        assert_eq!(
            summary_line("prop", &report, ""),
            "prop: 36 triangles, 2 geometries (0 untextured), lod high, bounds clustered (1 of 3 islands, 2 geometries excluded)"
        );

        // With nothing excluded, the framing_islands count alone changes nothing.
        report.framing_excluded_geometries = 0;
        assert_eq!(
            summary_line("prop", &report, ""),
            "prop: 36 triangles, 2 geometries (0 untextured), lod high, bounds from file"
        );
    }

    #[test]
    fn file_names_sanitize_entry_names() {
        assert_eq!(image_file_name("d", Some("a b/c.d"), None, "png"), "d_a_b_c_d.png");
        assert_eq!(image_file_name("d", Some(""), None, "png"), "d_entry.png");
    }
}
