//! `catalog sheet`: blind, numbered contact sheets for a set of catalogue
//! items, and the `packet.json` a reviewer answers.
//!
//! Tiles carry a number and view labels only: no names, paths or hashes, so
//! a vision model describes what it sees rather than what a filename
//! suggests. The number-to-asset mapping stays in the catalogue
//! (`packet_tiles`) and every tile's PNG hash is recorded, so `annotate` can
//! refuse a description of an image that is not the one it was shown.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use rage_formats::texture_utils::{encode_image, fit_max_size, to_rgba_image, ImageFormat};
use rage_formats::{parse_drawables, parse_ytd, parse_yft, DrawableKind, YtdTexture};
use rage_render::image::{Rgba, RgbaImage};
use rage_render::{compose_sheet, draw_text, export_glb, glb_report, render_parts, text_width, GlbOptions, RenderOptions, RenderPart, RenderReport,
                  SheetItem, SheetOptions as GridOptions, TextureSet, View, GLYPH_H};

use super::loader::{fallback_for, resident_for, resolution_order, texture_set_for, DecodedChain, EntryLoader, TxdCache};
use super::scan::hex_digest;
use super::search::{load_item, ItemView};
use super::{now_secs, Catalog, Kind};
use crate::commands::screenshot::renderables;
use crate::resources::{embedded_textures_of, Loaded};
use crate::rpf::GtaKeys;

pub struct SheetOptions {
    pub views: Vec<View>,
    pub facing: rage_render::Facing,
    pub per_sheet: usize,
    pub cell: u32,
    pub out_dir: PathBuf,
    pub write_tiles: bool,
    pub seed: Option<u64>,
    pub query: Option<String>,
    pub filters: String,
    pub texture_budget: usize,
    pub quiet: bool,
}

pub struct PacketSummary {
    pub packet_id: String,
    pub packet_path: PathBuf,
    pub template_path: PathBuf,
    pub sheets: Vec<PathBuf>,
    pub tiles: usize,
    pub skipped: Vec<(String, String)>,
    pub missing_textures: usize,
}

const MAX_CHILDREN: usize = 16;
const SHEET_BG: [u8; 4] = [32, 32, 32, 255];
/// Ground for pale models, distinct from the sheet's own frame colour.
const DARK_BG: [u8; 4] = [64, 64, 64, 255];
/// Mean luminance gap between model and ground below which the model counts
/// as hard to see.
const MIN_CONTRAST: f32 = 56.0;
const LABEL: [u8; 4] = [255, 255, 255, 255];

/// A selection key (packet-issued key or catalogue key) paired with why it
/// was skipped.
pub type SkipList = Vec<(String, String)>;

/// Expands containers to what can be drawn: a drawable dictionary to its
/// entries, a texture dictionary to its textures. Order is kept, repeats dropped.
pub fn expand(cat: &Catalog, items: Vec<ItemView>) -> Result<(Vec<ItemView>, SkipList)> {
    let mut out: Vec<ItemView> = Vec::new();
    let mut skipped = Vec::new();
    let push = |i: ItemView, out: &mut Vec<ItemView>| {
        if !out.iter().any(|o| o.id == i.id) {
            out.push(i);
        }
    };
    for item in items {
        if let Some(e) = &item.parse_error {
            skipped.push((item.key.clone(), format!("rage cannot parse it: {e}")));
            continue;
        }
        match item.kind {
            Kind::Drawable | Kind::Fragment | Kind::DdEntry | Kind::Texture => push(item, &mut out),
            Kind::Dictionary | Kind::Txd => {
                let child = if item.kind == Kind::Dictionary { "dd_entry" } else { "texture" };
                let ids: Vec<i64> = cat
                    .conn
                    .prepare("SELECT id FROM items WHERE parent_id = ?1 AND kind = ?2 ORDER BY id LIMIT ?3")?
                    .query_map(params![item.id, child, (MAX_CHILDREN + 1) as i64], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                if ids.len() > MAX_CHILDREN {
                    eprintln!("warning: {} holds more than {MAX_CHILDREN} {child}s; sheeting the first {MAX_CHILDREN}", item.label());
                }
                for id in ids.into_iter().take(MAX_CHILDREN) {
                    push(load_item(cat, id)?, &mut out);
                }
            }
        }
    }
    Ok((out, skipped))
}

/// `pk_` and 16 hex digits, from what was asked for: the same items, views
/// and cell size on the same build always make the same packet.
pub fn packet_id(game_build: Option<&str>, keys: &[&str], views: &[View], cell: u32) -> String {
    let mut sorted: Vec<&str> = keys.to_vec();
    sorted.sort_unstable();
    let mut h = Sha256::new();
    h.update(game_build.unwrap_or("").as_bytes());
    for k in sorted {
        h.update([0]);
        h.update(k.as_bytes());
    }
    for v in views {
        h.update([1]);
        h.update(v.label().as_bytes());
    }
    h.update(cell.to_le_bytes());
    format!("pk_{}", &hex_digest(&h.finalize())[..16])
}

/// SplitMix64: a tiny, stable PRNG so a seed always gives the same layout.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A permutation of `0..n` from `seed` (Fisher-Yates).
pub fn shuffled(n: usize, seed: u64) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    let mut rng = SplitMix(seed);
    for i in (1..n).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
    v
}

struct Tile {
    item: ItemView,
    png: Vec<u8>,
    sha: String,
    views: Vec<String>,
    missing: Vec<String>,
    lod: String,
}

pub fn make_packet(cat: &mut Catalog, keys: Option<&GtaKeys>, items: Vec<ItemView>, opts: &SheetOptions) -> Result<PacketSummary> {
    if opts.views.is_empty() {
        bail!("no views to render");
    }
    let (mut items, mut skipped) = expand(cat, items)?;
    if items.is_empty() {
        bail!("nothing to sheet: no drawable, fragment, dictionary entry or texture in the selection");
    }
    std::fs::create_dir_all(&opts.out_dir).with_context(|| format!("failed to create {}", opts.out_dir.display()))?;
    let game_build = cat.meta("game_build")?;

    // Render in texture-chain order so neighbours share decoded dictionaries.
    let mut orders: Vec<(ItemView, Vec<u32>)> = Vec::with_capacity(items.len());
    for item in items.drain(..) {
        let order = if item.kind == Kind::Texture { Vec::new() } else { resolution_order(cat, &item)? };
        orders.push((item, order));
    }
    orders.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.id.cmp(&b.0.id)));

    let mut loader = EntryLoader::new(keys);
    let mut txds = TxdCache::new(opts.texture_budget);
    let mut shared: Option<DecodedChain> = None;
    let mut tiles: Vec<Tile> = Vec::new();
    let total = orders.len();
    for (n, (item, order)) in orders.into_iter().enumerate() {
        if !opts.quiet {
            use std::io::Write;
            eprint!("\r[{:>3}/{total}] rendering tiles", n + 1);
            let _ = std::io::stderr().flush();
        }
        let rendered = if item.kind == Kind::Texture {
            texture_tile(cat, &mut loader, &item, opts)
        } else {
            model_tile(cat, &mut loader, &mut txds, &mut shared, &item, &order, opts)
        };
        match rendered {
            Ok(mut t) => {
                let png = encode_image(&t.0, ImageFormat::Png, 90)?;
                t.1.sha = hex_digest(&Sha256::digest(&png));
                t.1.png = png;
                tiles.push(t.1);
            }
            Err(e) => skipped.push((item.key.clone(), format!("{e:#}"))),
        }
    }
    if !opts.quiet {
        eprint!("\r{:<40}\r", "");
    }
    if tiles.is_empty() {
        let reasons: Vec<String> = skipped.iter().take(5).map(|(k, r)| format!("{k}: {r}")).collect();
        bail!("no tile could be rendered:\n  {}", reasons.join("\n  "));
    }

    // Blind numbering: a seeded shuffle of the rendered tiles.
    let keys_list: Vec<&str> = tiles.iter().map(|t| t.item.key.as_str()).collect();
    let pid = packet_id(game_build.as_deref(), &keys_list, &opts.views, opts.cell);
    let seed = opts.seed.unwrap_or_else(|| u64::from_str_radix(&pid[3..], 16).unwrap_or(0));
    let perm = shuffled(tiles.len(), seed);
    let mut numbered: Vec<Option<Tile>> = tiles.into_iter().map(Some).collect();
    let ordered: Vec<Tile> = perm.iter().map(|&i| numbered[i].take().unwrap()).collect();

    // Compose sheets.
    let per_sheet = opts.per_sheet.max(1);
    let columns = (per_sheet as f64).sqrt().ceil().max(1.0) as usize;
    let mut sheets: Vec<PathBuf> = Vec::new();
    // Each sheet's file name, its own PNG hash, and (tile, col, row) for every cell on it.
    type SheetRow = (String, String, Vec<(usize, usize, usize)>);
    let mut sheet_rows: Vec<SheetRow> = Vec::new();
    if opts.write_tiles {
        std::fs::create_dir_all(opts.out_dir.join("tiles"))?;
    }
    for (s, chunk) in ordered.chunks(per_sheet).enumerate() {
        let images: Vec<RgbaImage> = chunk
            .iter()
            .map(|t| rage_render::image::load_from_memory(&t.png).map(|i| i.to_rgba8()))
            .collect::<std::result::Result<_, _>>()?;
        let first_tile = s * per_sheet + 1;
        let (sheet, cells) = compose_numbered(&images, first_tile, columns);
        let png = encode_image(&sheet, ImageFormat::Png, 90)?;
        let sha = hex_digest(&Sha256::digest(&png));
        let file = format!("sheet-{:03}.png", s + 1);
        let path = opts.out_dir.join(&file);
        std::fs::write(&path, &png).with_context(|| format!("failed to write {}", path.display()))?;
        if opts.write_tiles {
            for (i, t) in chunk.iter().enumerate() {
                std::fs::write(opts.out_dir.join("tiles").join(format!("tile-{:03}.png", first_tile + i)), &t.png)?;
            }
        }
        sheets.push(path);
        sheet_rows.push((file, sha, cells.into_iter().enumerate().map(|(i, (c, r))| (first_tile + i, c, r)).collect()));
    }

    // Record the packet: the mapping stays here, never in packet.json.
    let views_csv: String = opts.views.iter().map(|v| v.label()).collect::<Vec<_>>().join(",");
    let created = now_secs();
    let missing_total: usize = ordered.iter().map(|t| t.missing.len()).sum();
    {
        let tx = cat.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM packets WHERE packet_id = ?1", [&pid])?;
        tx.execute(
            "INSERT INTO packets(packet_id, created, game_build, query, filters, views, cell, out_dir, sheets, tiles)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                pid, created, game_build.as_deref().and_then(|b| b.parse::<i64>().ok()), opts.query, opts.filters,
                views_csv, opts.cell as i64, opts.out_dir.to_string_lossy(), sheets.len() as i64, ordered.len() as i64,
            ],
        )?;
        let mut insert = tx.prepare(
            "INSERT INTO packet_tiles(packet_id, tile, item_id, asset_key, sheet_file, col, row, tile_sha256, sheet_sha256, render_report)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        for (file, sha, cells) in &sheet_rows {
            for (tile, col, row) in cells {
                let t = &ordered[tile - 1];
                let report = json::object! { missing_textures: t.missing.clone(), lod: t.lod.clone() }.dump();
                insert.execute(params![pid, *tile as i64, t.item.id, t.item.asset_key, file, *col as i64, *row as i64, t.sha, sha, report])?;
            }
        }
        drop(insert);
        tx.commit()?;
    }

    // packet.json: what the reviewer sees and must answer.
    let mut sheets_json = json::JsonValue::new_array();
    for (file, sha, cells) in &sheet_rows {
        let mut tiles_json = json::JsonValue::new_array();
        for (tile, col, row) in cells {
            let t = &ordered[tile - 1];
            let mut o = json::object! {
                tile: *tile, col: *col, row: *row, kind: if t.item.kind == Kind::Texture { "texture" } else { "model" },
                views: t.views.clone(), sha256: t.sha.clone(),
                render: json::object! { missing_textures: t.missing.len(), lod: t.lod.clone() },
            };
            if let Some(s) = t.item.size {
                o["size_m"] = json::array![round2(s[0]), round2(s[1]), round2(s[2])];
            }
            if let (Some(w), Some(h)) = (t.item.width, t.item.height) {
                o["size_px"] = json::array![w, h];
            }
            let _ = tiles_json.push(o);
        }
        let columns_here = cells.iter().map(|c| c.1).max().unwrap_or(0) + 1;
        let rows_here = cells.iter().map(|c| c.2).max().unwrap_or(0) + 1;
        let _ = sheets_json.push(json::object! { file: file.clone(), sha256: sha.clone(), columns: columns_here, rows: rows_here, tiles: tiles_json });
    }
    let packet = json::object! {
        schema: "rage-catalog-packet/1",
        packet_id: pid.clone(),
        created: created,
        game_build: game_build.clone(),
        db: cat.path.to_string_lossy().to_string(),
        views: opts.views.iter().map(|v| v.label().into_owned()).collect::<Vec<_>>(),
        cell: opts.cell,
        sheets: sheets_json,
        instructions: INSTRUCTIONS,
        response_schema: response_schema(&pid),
    };
    let packet_path = opts.out_dir.join("packet.json");
    std::fs::write(&packet_path, packet.pretty(2))?;

    // A pre-filled responses file: the reviewer only writes descriptions.
    let mut tpl_tiles = json::JsonValue::new_array();
    for (t, tile) in ordered.iter().enumerate() {
        let _ = tpl_tiles.push(json::object! {
            tile: t + 1, sha256: tile.sha.clone(), description: "", shape: "", material: "", condition: "",
            likely_use: "", tags: json::array![], confidence: json::Null, orientation_doubt: false,
            missing_views: json::array![], limitations: "",
        });
    }
    let template = json::object! {
        schema: "rage-catalog-responses/1", packet_id: pid.clone(), reviewer: "agent", reviewer_name: "", tiles: tpl_tiles,
    };
    let template_path = opts.out_dir.join("responses.template.json");
    std::fs::write(&template_path, template.pretty(2))?;

    Ok(PacketSummary { packet_id: pid, packet_path, template_path, sheets, tiles: ordered.len(), skipped, missing_textures: missing_total })
}

const INSTRUCTIONS: &str = "Describe each numbered tile from its image alone: shape, material, condition, likely use, and tags a person would search for. Names are withheld on purpose; do not guess them. Say when a view is missing or the orientation is unclear, give a confidence from 0 to 1, and leave a tile out rather than invent a description. Copy each tile's sha256 into your answer unchanged.";

fn response_schema(pid: &str) -> json::JsonValue {
    json::object! {
        schema: "rage-catalog-responses/1",
        packet_id: pid,
        reviewer: "agent | human",
        reviewer_name: "string, optional",
        tiles: json::array![json::object! {
            tile: "integer, from this packet",
            sha256: "string, copied from this packet",
            description: "string, required",
            shape: "string", material: "string", condition: "string", likely_use: "string",
            tags: "array of strings",
            confidence: "number 0..1",
            orientation_doubt: "boolean",
            missing_views: "array of view names",
            limitations: "string",
        }],
    }
}

fn luma(p: [u8; 4]) -> f32 {
    0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32
}

/// True when the model's pixels, across every view, sit on average within
/// `MIN_CONTRAST` luminance of the ground. Empty renders are not low contrast.
fn low_contrast(rendered: &[(View, RgbaImage, rage_render::RenderReport)], background: [u8; 4]) -> bool {
    let ground = luma(background);
    let (mut sum, mut count) = (0.0f64, 0u64);
    for (_, img, _) in rendered {
        for p in img.pixels().filter(|p| p.0 != background) {
            sum += (luma(p.0) - ground).abs() as f64;
            count += 1;
        }
    }
    count > 0 && ((sum / count as f64) as f32) < MIN_CONTRAST
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Loads an item's model and the texture set it draws with. `probe` reports
/// the textures still missing; while some are, the resident dictionaries
/// holding those names and then the shadowed base copies of patched
/// dictionaries are added and `probe` runs again. `finish` then runs once
/// on the final set, with the last probe's result.
#[allow(clippy::too_many_arguments)]
fn with_model<T, U>(
    cat: &Catalog,
    loader: &mut EntryLoader,
    txds: &mut TxdCache,
    shared: &mut Option<DecodedChain>,
    item: &ItemView,
    order: &[u32],
    mut probe: impl FnMut(&[RenderPart<'_>], &TextureSet) -> Result<(T, Vec<String>)>,
    finish: impl FnOnce(&[RenderPart<'_>], &TextureSet, T) -> Result<U>,
) -> Result<(U, Vec<String>)> {
    let bytes = loader.item_bytes(item)?;
    record_source_hash(cat, item.id, &bytes)?;
    let ext = item.entry_name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let loaded = match ext {
        "yft" => Loaded::Fragment(parse_yft(&bytes)?),
        "ydd" => Loaded::Entries(parse_drawables(&bytes, DrawableKind::Ydd)?),
        _ => Loaded::Entries(parse_drawables(&bytes, DrawableKind::Ydr)?),
    };
    let all = renderables(&loaded);
    let chosen = match item.kind {
        Kind::DdEntry => all.iter().find(|r| r.hash == item.hash),
        _ => all.first(),
    }
    .with_context(|| format!("nothing drawable for {}", item.label()))?;

    let embedded: Vec<YtdTexture> = embedded_textures_of(loaded.drawables()).into_iter().cloned().collect();
    let mut set = texture_set_for(cat, loader, txds, shared, order, &embedded)?;
    let (mut probed, mut missing) = probe(&chosen.parts, &set)?;
    if !missing.is_empty() {
        let resident = resident_for(cat, &missing)?;
        if !resident.is_empty() {
            for h in resident {
                if let Some(t) = txds.get(cat, loader, h)? {
                    set.to_mut().push_layer(&t);
                }
            }
            (probed, missing) = probe(&chosen.parts, &set)?;
        }
        // Still missing: shadowed base copies of patched dictionaries, then
        // any dictionary holding a texture of that exact name.
        let fallback = if missing.is_empty() { Vec::new() } else { fallback_for(cat, order, &missing)? };
        if !fallback.is_empty() {
            for id in fallback {
                if let Some(t) = txds.get_item(cat, loader, id)? {
                    set.to_mut().push_layer(&t);
                }
            }
            (probed, missing) = probe(&chosen.parts, &set)?;
        }
    }
    Ok((finish(&chosen.parts, &set, probed)?, missing))
}

fn model_tile(
    cat: &Catalog,
    loader: &mut EntryLoader,
    txds: &mut TxdCache,
    shared: &mut Option<DecodedChain>,
    item: &ItemView,
    order: &[u32],
    opts: &SheetOptions,
) -> Result<(RgbaImage, Tile)> {
    let options = RenderOptions { width: opts.cell, height: opts.cell, view: opts.views[0], cluster_framing: true, facing: opts.facing, ..Default::default() };
    let probe = |parts: &[RenderPart<'_>], set: &TextureSet| {
        let rendered = render_parts(parts, set, &options, &opts.views)?;
        let missing = rendered.first().map(|r| r.2.missing_textures.clone()).unwrap_or_default();
        Ok((rendered, missing))
    };
    // Pale props (white-textured markers, plaster, paper) all but vanish on
    // the light default ground; draw those on a dark one instead.
    let finish = |parts: &[RenderPart<'_>], set: &TextureSet, rendered: Vec<(View, RgbaImage, RenderReport)>| {
        if !low_contrast(&rendered, options.background) {
            return Ok(rendered);
        }
        let dark = RenderOptions { background: DARK_BG, ..options };
        render_parts(parts, set, &dark, &opts.views)
    };
    let (rendered, missing) = with_model(cat, loader, txds, shared, item, order, probe, finish)?;
    let lod = rendered.first().map(|r| format!("{:?}", r.2.lod)).unwrap_or_default();
    let items: Vec<SheetItem<'_>> = rendered.iter().map(|(v, img, _)| SheetItem { label: v.label().to_string(), image: img }).collect();
    let image = compose_sheet(
        &items,
        &GridOptions { cell: opts.cell, columns: Some(items.len() as u32), padding: 4, label_scale: 1, background: SHEET_BG },
    );
    let views = opts.views.iter().map(|v| v.label().to_string()).collect();
    Ok((image, Tile { item: item.clone(), png: Vec::new(), sha: String::new(), views, missing, lod: lod.to_lowercase() }))
}

/// What a `catalog model --thumb` preview is drawn on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThumbBackground {
    /// A vertical sky gradient, deep blue at the top to pale at the bottom.
    Sky,
    /// One colour, with pale models moved to a dark ground as on the review sheets.
    Solid([u8; 4]),
}

const SKY_ZENITH: [f32; 3] = [58.0, 112.0, 184.0];
const SKY_HORIZON: [f32; 3] = [198.0, 220.0, 238.0];

/// Composites a render drawn on a transparent ground over the sky gradient.
fn over_sky(image: &RgbaImage) -> RgbaImage {
    let span = image.height().saturating_sub(1).max(1) as f32;
    let mut out = image.clone();
    for (_, y, pixel) in out.enumerate_pixels_mut() {
        let t = y as f32 / span;
        let alpha = pixel.0[3] as f32 / 255.0;
        for c in 0..3 {
            let sky = SKY_ZENITH[c] + (SKY_HORIZON[c] - SKY_ZENITH[c]) * t;
            pixel.0[c] = (pixel.0[c] as f32 * alpha + sky * (1.0 - alpha)).round() as u8;
        }
        pixel.0[3] = 255;
    }
    out
}

/// What `catalog model` wrote for one item.
pub struct ModelExport {
    pub item: ItemView,
    pub path: PathBuf,
    pub bytes: usize,
    pub report: RenderReport,
    /// The iso preview written next to the model, with `--thumb`.
    pub thumb: Option<PathBuf>,
}

/// Writes each model item as `<id>.glb` in `out_dir`, its textures resolved
/// through the catalogue exactly as `catalog sheet` resolves them, and with
/// `thumb` an iso preview `<id>.jpg` of that size drawn from the same model
/// and textures. Items that fail are returned with the reason instead of
/// stopping the run.
pub fn export_models(cat: &Catalog, keys: Option<&GtaKeys>, items: Vec<ItemView>, out_dir: &Path, glb: &GlbOptions, thumb: Option<(u32, ThumbBackground)>, texture_budget: usize) -> Result<(Vec<ModelExport>, SkipList)> {
    let (items, mut skipped) = expand(cat, items)?;
    std::fs::create_dir_all(out_dir).with_context(|| format!("failed to create {}", out_dir.display()))?;
    let mut orders: Vec<(ItemView, Vec<u32>)> = Vec::with_capacity(items.len());
    for item in items {
        if item.kind == Kind::Texture {
            skipped.push((item.key.clone(), "a texture, not a model".to_string()));
            continue;
        }
        let order = resolution_order(cat, &item)?;
        orders.push((item, order));
    }
    // Texture-chain order, so neighbours share decoded dictionaries.
    orders.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.id.cmp(&b.0.id)));

    let mut loader = EntryLoader::new(keys);
    let mut txds = TxdCache::new(texture_budget);
    let mut shared: Option<DecodedChain> = None;
    let mut written = Vec::new();
    let probe = |parts: &[RenderPart<'_>], set: &TextureSet| {
        let report = glb_report(parts, set, glb);
        let missing = report.missing_textures.clone();
        Ok(((), missing))
    };
    for (item, order) in orders {
        let finish = |parts: &[RenderPart<'_>], set: &TextureSet, _: ()| {
            let (bytes, report) = export_glb(parts, set, glb)?;
            let Some((px, ground)) = thumb else { return Ok((bytes, report, None)) };
            let background = match ground {
                ThumbBackground::Sky => [0, 0, 0, 0],
                ThumbBackground::Solid(colour) => colour,
            };
            let options = RenderOptions { width: px, height: px, view: View::Iso, background, cluster_framing: true, facing: glb.facing, lod: glb.lod, ..Default::default() };
            let mut rendered = render_parts(parts, set, &options, &[View::Iso])?;
            // On a solid ground, pale models move to a dark one, as on the review sheets.
            if ground != ThumbBackground::Sky && low_contrast(&rendered, options.background) {
                let dark = RenderOptions { background: DARK_BG, ..options };
                rendered = render_parts(parts, set, &dark, &[View::Iso])?;
            }
            let image = match ground {
                ThumbBackground::Sky => over_sky(&rendered[0].1),
                ThumbBackground::Solid(_) => rendered.swap_remove(0).1,
            };
            let jpeg = encode_image(&image, ImageFormat::Jpeg, 85)?;
            Ok((bytes, report, Some(jpeg)))
        };
        match with_model(cat, &mut loader, &mut txds, &mut shared, &item, &order, probe, finish) {
            Ok(((bytes, report, jpeg), _)) => {
                let path = out_dir.join(format!("{}.glb", item.id));
                std::fs::write(&path, &bytes).with_context(|| format!("failed to write {}", path.display()))?;
                let thumb = match jpeg {
                    Some(jpeg) => {
                        let thumb_path = out_dir.join(format!("{}.jpg", item.id));
                        std::fs::write(&thumb_path, jpeg).with_context(|| format!("failed to write {}", thumb_path.display()))?;
                        Some(thumb_path)
                    }
                    None => None,
                };
                written.push(ModelExport { item, path, bytes: bytes.len(), report, thumb });
            }
            Err(e) => skipped.push((item.key.clone(), format!("{e:#}"))),
        }
    }
    Ok((written, skipped))
}

fn texture_tile(cat: &Catalog, loader: &mut EntryLoader, item: &ItemView, opts: &SheetOptions) -> Result<(RgbaImage, Tile)> {
    let bytes = loader.item_bytes(item)?;
    record_source_hash(cat, item.id, &bytes)?;
    let wanted = item.name.clone().unwrap_or_default();
    let textures: Vec<YtdTexture> = if item.embedded {
        let ext = item.entry_name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        let loaded = match ext {
            "yft" => Loaded::Fragment(parse_yft(&bytes)?),
            "ydd" => Loaded::Entries(parse_drawables(&bytes, DrawableKind::Ydd)?),
            _ => Loaded::Entries(parse_drawables(&bytes, DrawableKind::Ydr)?),
        };
        embedded_textures_of(loaded.drawables()).into_iter().cloned().collect()
    } else {
        parse_ytd(&bytes)?
    };
    let tex = textures
        .iter()
        .find(|t| t.name.to_lowercase() == wanted)
        .or_else(|| textures.iter().find(|t| t.name_hash == item.hash))
        .with_context(|| format!("texture {} not found in its dictionary", item.label()))?;
    let image = fit_max_size(to_rgba_image(tex)?, opts.cell);
    let items = [SheetItem { label: "texture".into(), image: &image }];
    let tile = compose_sheet(&items, &GridOptions { cell: opts.cell, columns: Some(1), padding: 4, label_scale: 1, background: SHEET_BG });
    Ok((tile, Tile { item: item.clone(), png: Vec::new(), sha: String::new(), views: vec!["texture".into()], missing: Vec::new(), lod: String::new() }))
}

/// Fills in the container's `source_sha256` the first time its bytes are read.
fn record_source_hash(cat: &Catalog, item_id: i64, bytes: &[u8]) -> Result<()> {
    let root: Option<i64> = cat
        .conn
        .query_row("SELECT COALESCE(root_id, id) FROM items WHERE id = ?1 AND (SELECT source_sha256 FROM items r WHERE r.id = COALESCE(items.root_id, items.id)) IS NULL", [item_id], |r| r.get(0))
        .optional()?;
    if let Some(root) = root {
        let sha = hex_digest(&Sha256::digest(bytes));
        cat.conn.execute("UPDATE items SET source_sha256 = ?2 WHERE id = ?1", params![root, sha])?;
    }
    Ok(())
}

/// Lays tiles out in a grid, each under a `#N` label strip. Cells are as big
/// as the largest tile so nothing is rescaled. Returns each tile's (col, row).
fn compose_numbered(tiles: &[RgbaImage], first: usize, columns: usize) -> (RgbaImage, Vec<(usize, usize)>) {
    let scale = 2;
    let strip = GLYPH_H * scale + 8;
    let pad = 8u32;
    let cw = tiles.iter().map(|t| t.width()).max().unwrap_or(1);
    let ch = tiles.iter().map(|t| t.height()).max().unwrap_or(1) + strip;
    let cols = columns.min(tiles.len()).max(1) as u32;
    let rows = (tiles.len() as u32).div_ceil(cols);
    let mut sheet = RgbaImage::from_pixel(pad + cols * (cw + pad), pad + rows * (ch + pad), Rgba(SHEET_BG));
    let mut cells = Vec::with_capacity(tiles.len());
    for (i, tile) in tiles.iter().enumerate() {
        let (c, r) = (i as u32 % cols, i as u32 / cols);
        let x = pad + c * (cw + pad);
        let y = pad + r * (ch + pad);
        let label = format!("#{}", first + i);
        let lx = x as i32 + ((cw.saturating_sub(text_width(&label, scale))) / 2) as i32;
        draw_text(&mut sheet, lx, y as i32 + 4, &label, scale, LABEL);
        rage_render::image::imageops::overlay(&mut sheet, tile, (x + (cw - tile.width()) / 2) as i64, (y + strip) as i64);
        cells.push((c as usize, r as usize));
    }
    (sheet, cells)
}

/// One revealed tile: its number, catalogue key (or issued asset key when the
/// item is gone), resolved name (if any), and the sheet file it is on.
pub type RevealedTile = (i64, String, Option<String>, String);

/// The tile-to-asset mapping of a packet, for people once reviewing is done.
pub fn reveal(cat: &Catalog, packet_id: &str) -> Result<Vec<RevealedTile>> {
    let exists: Option<i64> = cat.conn.query_row("SELECT 1 FROM packets WHERE packet_id = ?1", [packet_id], |r| r.get(0)).optional()?;
    if exists.is_none() {
        bail!("packet {packet_id} is not in this catalogue");
    }
    let mut stmt = cat.conn.prepare(
        "SELECT t.tile, COALESCE(i.key, t.asset_key), i.name, t.sheet_file FROM packet_tiles t LEFT JOIN items i ON i.id = t.item_id
         WHERE t.packet_id = ?1 ORDER BY t.tile",
    )?;
    let rows = stmt.query_map([packet_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Reads catalogue ids or keys from a file: one per line, or the JSON
/// `catalog search --json` writes, or a JSON array of ids/keys.
pub fn read_selection(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let v = json::parse(&text).with_context(|| format!("{} is not valid JSON", path.display()))?;
        let list = if v.is_object() { &v["results"] } else { &v };
        let mut out = Vec::new();
        for m in list.members() {
            if let Some(k) = m["key"].as_str() {
                out.push(k.to_string());
            } else if let Some(s) = m.as_str() {
                out.push(s.to_string());
            } else if let Some(n) = m.as_i64() {
                out.push(n.to_string());
            } else if let Some(n) = m["id"].as_i64() {
                out.push(n.to_string());
            }
        }
        return Ok(out);
    }
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_ids_ignore_selection_order() {
        let a = packet_id(Some("3889"), &["x", "y"], &[View::Iso], 256);
        let b = packet_id(Some("3889"), &["y", "x"], &[View::Iso], 256);
        assert_eq!(a, b);
        assert!(a.starts_with("pk_") && a.len() == 19);
        assert_ne!(a, packet_id(Some("3889"), &["x", "y"], &[View::Top], 256));
        assert_ne!(a, packet_id(Some("3890"), &["x", "y"], &[View::Iso], 256));
    }

    #[test]
    fn shuffles_are_seeded_permutations() {
        let a = shuffled(20, 7);
        assert_eq!(a, shuffled(20, 7));
        assert_ne!(a, shuffled(20, 8));
        let mut sorted = a.clone();
        sorted.sort();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn sky_shows_through_where_nothing_was_drawn() {
        let mut image = RgbaImage::from_pixel(1, 3, Rgba([0, 0, 0, 0]));
        image.put_pixel(0, 1, Rgba([255, 0, 0, 255]));
        let out = over_sky(&image);
        assert_eq!(out.get_pixel(0, 0).0, [58, 112, 184, 255], "zenith at the top");
        assert_eq!(out.get_pixel(0, 1).0, [255, 0, 0, 255], "the model is untouched");
        assert_eq!(out.get_pixel(0, 2).0, [198, 220, 238, 255], "horizon at the bottom");
    }

    #[test]
    fn numbered_sheets_lay_tiles_out_in_rows() {
        let tiles: Vec<RgbaImage> = (0..5).map(|_| RgbaImage::new(30, 20)).collect();
        let (sheet, cells) = compose_numbered(&tiles, 1, 2);
        assert_eq!(cells, vec![(0, 0), (1, 0), (0, 1), (1, 1), (0, 2)]);
        assert!(sheet.width() >= 60 && sheet.height() >= 60);
    }
}
