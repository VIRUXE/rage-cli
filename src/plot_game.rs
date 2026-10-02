//! What `rage plot --game` adds to a page: the vanilla map's visible
//! entities over the region (see `region`), the interiors they place, the
//! water quads and heightmap contours, and the navmesh, path and collision
//! chunks covering the box, all read through the game index.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rage_formats::{
    parse_heightmap, parse_water_xml, parse_ybn, parse_ynd, parse_ynv, parse_ytyp, rage_joaat, Vec3, Ybn, Ynd, Ynv, WorldHeightmap,
};
use rage_render::{HeightField, Layer, WaterQuadShape};

use crate::index::{EntryLoc, GameIndex, Parts};
use crate::region::{self, Region, RegionEntity, RegionOptions};
use crate::rpf::GtaKeys;

/// The layers `--game` draws when `--layers` is not given.
pub const DEFAULT_LAYERS: [Layer; 5] = [Layer::Entities, Layer::Water, Layer::Terrain, Layer::Navmesh, Layer::Paths];

/// An interior an MLO instance in the region places: its entities in
/// world space.
#[derive(Debug, Clone)]
pub struct GameInterior {
    /// `(archetype, world position)` of each entity the instance shows:
    /// the interior's own and those of its default entity sets.
    pub entities: Vec<(u32, Vec3)>,
}

/// Everything the game contributes to the page.
#[derive(Debug, Default)]
pub struct GameLayers {
    pub region: Region,
    pub interiors: Vec<GameInterior>,
    pub water: Vec<WaterQuadShape>,
    pub terrain: Option<HeightField>,
    pub ynvs: Vec<(String, Ynv)>,
    pub ynds: Vec<(String, Ynd)>,
    pub ybns: Vec<(String, Ybn)>,
    /// What was read, for the caption.
    pub caption: Vec<String>,
    /// Problems worth a line on stderr.
    pub warnings: Vec<String>,
}

/// Reads the game's layers for `opts.region`. `layers` says which chunk
/// kinds are worth reading at all.
pub fn load(opts: &RegionOptions, layers: &[Layer], keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<GameLayers> {
    if exe.is_none() {
        bail!("--game needs --exe or GTAV_PATH so the game's archives can be read");
    }
    let mut parts = Parts::WORLD | Parts::MODELS;
    if layers.contains(&Layer::Collision) || layers.contains(&Layer::Entities) {
        parts |= Parts::INTERIORS;
    }
    let index = GameIndex::load(exe, keys, parts).context("the game index could not be built")?;
    let mut out = GameLayers::default();

    out.region = region::assemble(&index, keys, opts)?;
    let r = &out.region;
    for map in &r.maps {
        let shown = r.leaves.iter().filter(|l| l.map == map.name).count();
        log::debug!(
            "region: {} ({:#010x}, parent {:#010x}, flags {:#x}): {} of {} entities shown",
            map.inner_path, map.name, map.header.parent_hash, map.header.flags, shown, map.entity_count
        );
    }
    let mlo_leaves = r.leaves.iter().filter(|l| l.entity.is_mlo_instance).count();
    out.caption.push(format!(
        "{} maps streamed, {} entities shown at {:.0} m view distance{}",
        r.maps.len(),
        r.leaves.len(),
        r.view_distance,
        if mlo_leaves > 0 { format!(", {mlo_leaves} interiors") } else { String::new() }
    ));
    if r.switched_off > 0 {
        out.caption.push(format!("{} maps switched off at this hour or weather", r.switched_off));
    }
    if !r.unreadable.is_empty() {
        out.warnings.push(format!("{} maps could not be read: {}", r.unreadable.len(), r.unreadable.join(", ")));
    }
    if !r.orphaned.is_empty() {
        out.warnings.push(format!("{} maps left out because their parent map is not available: {}", r.orphaned.len(), r.orphaned.join(", ")));
    }

    if layers.contains(&Layer::Entities) {
        out.interiors = interiors(&index, keys, &out.region.leaves, &mut out.warnings);
    }
    if layers.contains(&Layer::Water) {
        out.water = water(&index, keys, opts.region, &mut out.warnings);
    }
    if layers.contains(&Layer::Terrain) {
        out.terrain = terrain(&index, keys, opts.region, &mut out.warnings);
    }
    if layers.contains(&Layer::Navmesh) {
        let names = cell_names(opts.region, rage_formats::ynv::cell_for_position, rage_formats::ynv::cell_file_name);
        for (name, data) in read_named(&index, &index.ynv_by_name, &names, keys, &mut out.warnings) {
            match parse_ynv(&data) {
                Ok(ynv) => out.ynvs.push((name, ynv)),
                Err(err) => out.warnings.push(format!("{name}: {err:#}")),
            }
        }
    }
    if layers.contains(&Layer::Paths) {
        let names = cell_names(opts.region, rage_formats::ynd::cell_for_position, rage_formats::ynd::cell_file_name);
        for (name, data) in read_named(&index, &index.ynd_by_name, &names, keys, &mut out.warnings) {
            match parse_ynd(&data) {
                Ok(ynd) => out.ynds.push((name, ynd)),
                Err(err) => out.warnings.push(format!("{name}: {err:#}")),
            }
        }
    }
    if layers.contains(&Layer::Collision) {
        let [x0, y0, x1, y1] = opts.region;
        let mut names: Vec<(u32, EntryLoc)> = index
            .bounds_store
            .values()
            .filter(|b| b.min.x <= x1 && b.max.x >= x0 && b.min.y <= y1 && b.max.y >= y0)
            .filter_map(|b| index.ybn_by_name.get(&b.name).map(|loc| (b.name, loc.clone())))
            .collect();
        names.sort_by_key(|(hash, _)| *hash);
        let locs: Vec<EntryLoc> = names.iter().map(|(_, loc)| loc.clone()).collect();
        for ((_, loc), data) in names.iter().zip(index.load_batch(&locs, keys)) {
            match data.and_then(|d| parse_ybn(&d).map_err(Into::into)) {
                Ok(ybn) => out.ybns.push((loc.inner_path.clone(), ybn)),
                Err(err) => out.warnings.push(format!("{}: {err:#}", loc.inner_path)),
            }
        }
        if !out.ybns.is_empty() {
            out.caption.push(format!("{} collision chunks", out.ybns.len()));
        }
    }
    Ok(out)
}

/// The cell file names covering `region`, for a grid described by its
/// two helpers.
fn cell_names(region: [f32; 4], cell_for: fn(f32, f32) -> (u32, u32), file_name: fn(u32, u32) -> String) -> Vec<String> {
    let [x0, y0, x1, y1] = region;
    let (cx0, cy0) = cell_for(x0.min(x1), y0.min(y1));
    let (cx1, cy1) = cell_for(x0.max(x1), y0.max(y1));
    let mut names = Vec::new();
    for cy in cy0..=cy1 {
        for cx in cx0..=cx1 {
            names.push(file_name(cx, cy));
        }
    }
    names
}

/// Reads the named files out of `by_name`, in one batch; a name the index
/// does not know is simply absent (the sea has no navmesh).
fn read_named(
    index: &GameIndex, by_name: &HashMap<u32, EntryLoc>, names: &[String], keys: Option<&GtaKeys>, warnings: &mut Vec<String>,
) -> Vec<(String, Vec<u8>)> {
    let found: Vec<(String, EntryLoc)> = names
        .iter()
        .filter_map(|name| {
            let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s).to_lowercase();
            by_name.get(&rage_joaat(&stem)).map(|loc| (name.clone(), loc.clone()))
        })
        .collect();
    let locs: Vec<EntryLoc> = found.iter().map(|(_, loc)| loc.clone()).collect();
    let mut out = Vec::new();
    for ((name, loc), data) in found.into_iter().zip(index.load_batch(&locs, keys)) {
        match data {
            Ok(data) => out.push((name, data)),
            Err(err) => warnings.push(format!("{}: {err:#}", loc.inner_path)),
        }
    }
    out
}

/// `RenderWorldAddInteriorEntities`: every interior placed by a visible
/// MLO instance, its entities and default entity sets placed in the world.
fn interiors(index: &GameIndex, keys: Option<&GtaKeys>, leaves: &[RegionEntity], warnings: &mut Vec<String>) -> Vec<GameInterior> {
    let instances: Vec<&RegionEntity> = leaves.iter().filter(|l| l.entity.is_mlo_instance).collect();
    if instances.is_empty() {
        return Vec::new();
    }
    // One read per type file, however many interiors it declares.
    let mut ytyps: Vec<EntryLoc> = Vec::new();
    for leaf in &instances {
        match index.mlo_ytyp.get(&leaf.entity.archetype_hash) {
            Some(loc) if !ytyps.contains(loc) => ytyps.push(loc.clone()),
            Some(_) => {}
            None => warnings.push(format!("interior {:#010x}: no type file in the index", leaf.entity.archetype_hash)),
        }
    }
    let mut mlos: HashMap<u32, rage_formats::MloDef> = HashMap::new();
    for (loc, data) in ytyps.iter().zip(index.load_batch(&ytyps, keys)) {
        match data.and_then(|d| parse_ytyp(&d).map_err(Into::into)) {
            Ok(ytyp) => {
                for mlo in ytyp.mlos {
                    mlos.insert(mlo.name_hash, mlo);
                }
            }
            Err(err) => warnings.push(format!("{}: {err:#}", loc.inner_path)),
        }
    }
    let mut out = Vec::new();
    for leaf in instances {
        let Some(mlo) = mlos.get(&leaf.entity.archetype_hash) else { continue };
        log::debug!("region: interior {:#010x} at {:?}", leaf.entity.archetype_hash, leaf.entity.position);
        let mut entities: Vec<(u32, Vec3)> = mlo.entities.iter().map(|e| (e.archetype_hash, leaf.entity.to_world(e.position))).collect();
        let defaults = leaf.instance.as_ref().map(|i| i.default_entity_sets.clone()).unwrap_or_default();
        for set in mlo.entity_sets.iter().filter(|s| defaults.contains(&s.name_hash)) {
            entities.extend(set.entities.iter().map(|e| (e.archetype_hash, leaf.entity.to_world(e.position))));
        }
        out.push(GameInterior { entities });
    }
    out
}

/// A world file by its name, read through the index.
fn world_file(index: &GameIndex, keys: Option<&GtaKeys>, name: &str) -> Option<Result<Vec<u8>>> {
    let loc = index.world_files.get(&rage_joaat(name))?;
    Some(index.load_bytes(loc, keys).with_context(|| format!("reading {}", loc.inner_path)))
}

/// `Water.Init`: the quads of `water.xml` and the island file over the box.
fn water(index: &GameIndex, keys: Option<&GtaKeys>, region: [f32; 4], warnings: &mut Vec<String>) -> Vec<WaterQuadShape> {
    let [x0, y0, x1, y1] = region;
    let mut out = Vec::new();
    for name in ["water.xml", "water_heistisland.xml"] {
        let Some(data) = world_file(index, keys, name) else {
            if name == "water.xml" {
                warnings.push("water.xml is not in the game index; no water layer".to_string());
            }
            continue;
        };
        let parsed = data.and_then(|d| parse_water_xml(&String::from_utf8_lossy(&d)));
        match parsed {
            Ok(water) => out.extend(
                water
                    .quads
                    .iter()
                    .filter(|q| q.intersects(x0, y0, x1, y1))
                    .map(|q| WaterQuadShape { x0: q.min_x, y0: q.min_y, x1: q.max_x, y1: q.max_y, z: q.z, invisible: q.invisible }),
            ),
            Err(err) => warnings.push(format!("{name}: {err:#}")),
        }
    }
    out
}

/// `Heightmaps.Init`: the heightmap whose box covers most of the region,
/// as a field of its max heights; cells with no ground are `NaN`.
fn terrain(index: &GameIndex, keys: Option<&GtaKeys>, region: [f32; 4], warnings: &mut Vec<String>) -> Option<HeightField> {
    let [x0, y0, x1, y1] = region;
    let mut best: Option<(f32, WorldHeightmap)> = None;
    for name in ["heightmap.dat", "heightmapheistisland.dat"] {
        let Some(data) = world_file(index, keys, name) else {
            if name == "heightmap.dat" {
                warnings.push("heightmap.dat is not in the game index; no terrain layer".to_string());
            }
            continue;
        };
        match data.and_then(|d| parse_heightmap(&d)) {
            Ok(h) => {
                let w = (h.bb_max.x.min(x1) - h.bb_min.x.max(x0)).max(0.0);
                let d = (h.bb_max.y.min(y1) - h.bb_min.y.max(y0)).max(0.0);
                let area = w * d;
                if area > 0.0 && best.as_ref().is_none_or(|(a, _)| area > *a) {
                    best = Some((area, h));
                }
            }
            Err(err) => warnings.push(format!("{name}: {err:#}")),
        }
    }
    let (_, h) = best?;
    Some(height_field(&h))
}

/// A heightmap's max heights as the renderer's field.
pub fn height_field(h: &WorldHeightmap) -> HeightField {
    let (step_x, step_y) = h.cell_size();
    let z = h.max_heights.iter().map(|&v| if v == 0 { f32::NAN } else { h.height_of(v) }).collect();
    HeightField { x0: h.bb_min.x, y0: h.bb_min.y, step_x, step_y, width: h.width as usize, height: h.height as usize, z }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_cover_the_box_inclusively() {
        let names = cell_names([-10.0, -10.0, 10.0, 10.0], rage_formats::ynv::cell_for_position, rage_formats::ynv::cell_file_name);
        // (-6000 + 39 * 150) = -150..0 and 0..150 on each axis: four cells.
        assert_eq!(names, vec!["navmesh[117][117].ynv", "navmesh[120][117].ynv", "navmesh[117][120].ynv", "navmesh[120][120].ynv"]);
        let names = cell_names([10.0, 10.0, 20.0, 20.0], rage_formats::ynd::cell_for_position, rage_formats::ynd::cell_file_name);
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn the_height_field_blanks_the_sea() {
        let h = WorldHeightmap {
            version_major: 1, version_minor: 1, width: 2, height: 2,
            bb_min: Vec3::new(0.0, 0.0, 0.0), bb_max: Vec3::new(100.0, 50.0, 255.0),
            max_heights: vec![0, 10, 20, 30], min_heights: vec![0; 4], little_endian: false,
        };
        let f = height_field(&h);
        assert_eq!((f.step_x, f.step_y, f.width, f.height), (100.0, 50.0, 2, 2));
        assert!(f.z[0].is_nan());
        assert_eq!(&f.z[1..], &[10.0, 20.0, 30.0]);
    }
}
