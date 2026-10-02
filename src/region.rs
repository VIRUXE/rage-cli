//! A region of the vanilla map, assembled the way CodeWalker's world view
//! streams it: the `cache_y.dat` map nodes covering a box and their parent
//! chains (`Space.GetVisibleYmaps`), the maps a manifest switches off at an
//! hour or weather left out (`Space.IsYmapAvailable`), every map's entities
//! linked into the LOD tree (`YmapFile.EnsureEntities`, `ConnectToParent`)
//! and the leaves a map view at the region's scale would draw
//! (`RenderLodManager.Update`).

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rage_formats::{parse_ymap, rage_joaat, MloInstance, Vec3, Ymap, YmapEntity, YmapHeader};

use crate::index::{EntryLoc, GameIndex};
use crate::rpf::GtaKeys;

/// What to assemble and how far the view reaches.
#[derive(Debug, Clone)]
pub struct RegionOptions {
    /// World `x0,y0,x1,y1`.
    pub region: [f32; 4],
    /// CodeWalker's `MapViewDetail`: the LOD-picking distance is the
    /// region's longer side divided by this.
    pub detail: f32,
    /// CodeWalker's `MaxLOD` (`YmapEntity::LOD_*`); `LOD_ORPHANHD` shows
    /// every level.
    pub max_lod: u32,
    /// Only maps switched on at this hour (0-23).
    pub hour: Option<u32>,
    /// Only maps allowed in this weather (a lowercase name's hash).
    pub weather: Option<u32>,
    /// Whether script-requested maps (`flags & 1`) are shown
    /// (`ShowScriptedYmaps`).
    pub scripted: bool,
}

/// A map that was read.
#[derive(Debug, Clone)]
pub struct RegionMap {
    pub name: u32,
    pub inner_path: String,
    pub header: YmapHeader,
    pub entity_count: usize,
}

/// One entity the view shows.
#[derive(Debug, Clone)]
pub struct RegionEntity {
    pub entity: YmapEntity,
    /// Which map placed it.
    pub map: u32,
    /// The instance fields, for an interior placement.
    pub instance: Option<MloInstance>,
}

/// The assembled region.
#[derive(Debug, Default)]
pub struct Region {
    pub maps: Vec<RegionMap>,
    /// The visible leaves of the LOD tree, in map then entity order.
    pub leaves: Vec<RegionEntity>,
    /// The view distance the LOD level was picked at, in metres.
    pub view_distance: f32,
    /// Maps the cache names but that could not be read, by inner path.
    pub unreadable: Vec<String>,
    /// Maps left out because their parent map was not available
    /// (`RenderLodManager`: "skip adding ymaps until parents are available").
    pub orphaned: Vec<String>,
    /// How many maps the hour or weather switched off.
    pub switched_off: usize,
    /// How many maps were scripted and left out.
    pub scripted_skipped: usize,
}

/// Whether two XY boxes meet (edges included).
fn boxes_meet(lo: Vec3, hi: Vec3, region: [f32; 4]) -> bool {
    lo.x <= region[2] && hi.x >= region[0] && lo.y <= region[3] && hi.y >= region[1]
}

/// `Space.IsYmapAvailable`: a map data group's hours and weathers gate it.
fn available(index: &GameIndex, name: u32, hour: Option<u32>, weather: Option<u32>) -> bool {
    if let Some(hour) = hour.filter(|h| *h <= 23)
        && let Some(hours) = index.map_hours.get(&name)
        && hours & (1 << hour) == 0
    {
        return false;
    }
    if let Some(weather) = weather.filter(|w| *w != 0)
        && let Some(weathers) = index.map_weathers.get(&name)
    {
        return weathers.contains(&weather);
    }
    true
}

/// The map nodes whose entities extents meet `region`, with their parent
/// chains, as `SpaceMapDataStore.GetItems(min, max)` then
/// `GetVisibleYmaps`' parent walk select them. In name order, so a run is
/// repeatable.
pub fn select_maps(index: &GameIndex, region: [f32; 4]) -> Vec<u32> {
    let mut chosen: HashSet<u32> = HashSet::new();
    for node in index.map_nodes.values() {
        if !boxes_meet(node.entities_min, node.entities_max, region) {
            continue;
        }
        let mut hash = node.name;
        while hash != 0 && chosen.insert(hash) {
            hash = index.map_nodes.get(&hash).map_or(0, |n| n.parent);
        }
    }
    let mut out: Vec<u32> = chosen.into_iter().collect();
    out.sort_unstable();
    out
}

/// One entity in the LOD tree.
struct Node {
    map: usize,
    entity: usize,
    lod_dist: f32,
    child_lod_dist: f32,
    parent: Option<usize>,
    children: Vec<usize>,
}

/// Reads the maps covering `opts.region` and picks the visible leaves.
pub fn assemble(index: &GameIndex, keys: Option<&GtaKeys>, opts: &RegionOptions) -> Result<Region> {
    let mut region = Region::default();
    let [x0, y0, x1, y1] = opts.region;
    region.view_distance = ((x1 - x0).abs().max((y1 - y0).abs()) / opts.detail.max(1e-3)).max(1.0);

    let names = select_maps(index, opts.region);
    let mut wanted: Vec<(u32, EntryLoc)> = Vec::new();
    for name in names {
        if !available(index, name, opts.hour, opts.weather) {
            region.switched_off += 1;
            continue;
        }
        let Some(loc) = index.ymap_by_name.get(&name) else { continue };
        wanted.push((name, loc.clone()));
    }
    let locs: Vec<EntryLoc> = wanted.iter().map(|(_, loc)| loc.clone()).collect();
    let mut maps: Vec<(u32, String, Ymap)> = Vec::new();
    for ((name, loc), data) in wanted.into_iter().zip(index.load_batch(&locs, keys)) {
        match data.and_then(|data| parse_ymap(&data).with_context(|| format!("parsing {}", loc.inner_path))) {
            Ok(ymap) => {
                if !opts.scripted && ymap.header.flags & YmapHeader::FLAG_SCRIPTED != 0 {
                    region.scripted_skipped += 1;
                    continue;
                }
                maps.push((name, loc.inner_path.clone(), ymap));
            }
            Err(err) => {
                log::debug!("region: {}: {err:#}", loc.inner_path);
                region.unreadable.push(loc.inner_path.clone());
            }
        }
    }

    assemble_loaded(index, maps, opts, region)
}

/// The LOD tree and its visible leaves over maps already read, as
/// `RenderLodManager.Update` builds and walks them.
fn assemble_loaded(index: &GameIndex, maps: Vec<(u32, String, Ymap)>, opts: &RegionOptions, mut region: Region) -> Result<Region> {
    // A map whose parent is not loaded never joins the tree.
    let loaded: HashSet<u32> = maps.iter().map(|(name, _, _)| *name).collect();
    let (maps, orphaned): (Vec<_>, Vec<_>) =
        maps.into_iter().partition(|(_, _, ymap)| ymap.header.parent_hash == 0 || loaded.contains(&ymap.header.parent_hash));
    region.orphaned = orphaned.into_iter().map(|(_, path, _)| path).collect();
    let map_index: HashMap<u32, usize> = maps.iter().enumerate().map(|(i, (name, _, _))| (*name, i)).collect();

    // `YmapFile.EnsureEntities`: within a map, an entity is a root when its
    // parent index is out of range, its parent sits in the parent map, or
    // its parent's LOD level is not above its own; `ConnectToParent` then
    // links a root with a parent index to that entity of the parent map.
    let mut nodes: Vec<Node> = Vec::new();
    let mut first: Vec<usize> = Vec::with_capacity(maps.len());
    for (m, (_, _, ymap)) in maps.iter().enumerate() {
        first.push(nodes.len());
        for (e, ent) in ymap.entities.iter().enumerate() {
            let lod_dist = if ent.lod_dist > 0.0 {
                ent.lod_dist
            } else {
                index.archetype_lod_dist.get(&ent.archetype_hash).copied().unwrap_or(ent.lod_dist)
            };
            let child_lod_dist = if ent.child_lod_dist < 0.0 { lod_dist * 0.5 } else { ent.child_lod_dist };
            nodes.push(Node { map: m, entity: e, lod_dist, child_lod_dist, parent: None, children: Vec::new() });
        }
    }
    for (m, (_, _, ymap)) in maps.iter().enumerate() {
        let count = ymap.entities.len();
        for (e, ent) in ymap.entities.iter().enumerate() {
            let pind = ent.parent_index;
            let in_map = pind >= 0 && (pind as usize) < count && !ent.lod_in_parent_ymap();
            let parent = if in_map {
                let p = &ymap.entities[pind as usize];
                let orphan = YmapEntity::LOD_ORPHANHD;
                let root = p.lod_level <= ent.lod_level || (p.lod_level == orphan && ent.lod_level != orphan);
                if root { None } else { Some(first[m] + pind as usize) }
            } else if pind >= 0 {
                map_index
                    .get(&ymap.header.parent_hash)
                    .filter(|pm| (pind as usize) < maps[**pm].2.entities.len())
                    .map(|pm| first[*pm] + pind as usize)
            } else {
                None
            };
            if let Some(p) = parent {
                nodes[first[m] + e].parent = Some(p);
                nodes[p].children.push(first[m] + e);
            }
        }
    }

    // `RenderLodManager.Update` at a map view: every entity is at the same
    // distance, the view's.
    let dist = region.view_distance;
    let max_lod = opts.max_lod;
    let entity_of = |n: &Node| maps[n.map].2.entities[n.entity];
    let mut leaves: Vec<usize> = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        if node.parent.is_some() {
            continue;
        }
        let ent = entity_of(node);
        if visible_at_max(&ent, max_lod) && dist <= node.lod_dist {
            descend(&nodes, &maps, i, dist, max_lod, &mut leaves);
        }
    }

    // `EntityVisible`: within the view. Without the archetype's box the
    // position stands for the entity.
    leaves.retain(|i| {
        let node = &nodes[*i];
        let ent = entity_of(node);
        match index.archetype_box.get(&ent.archetype_hash) {
            Some((lo, hi)) => {
                let corners = [
                    Vec3::new(lo.x, lo.y, lo.z), Vec3::new(hi.x, lo.y, lo.z), Vec3::new(hi.x, hi.y, lo.z), Vec3::new(lo.x, hi.y, lo.z),
                    Vec3::new(lo.x, lo.y, hi.z), Vec3::new(hi.x, lo.y, hi.z), Vec3::new(hi.x, hi.y, hi.z), Vec3::new(lo.x, hi.y, hi.z),
                ]
                .map(|c| ent.to_world(c));
                let (mut wlo, mut whi) = (corners[0], corners[0]);
                for c in &corners[1..] {
                    wlo = Vec3::new(wlo.x.min(c.x), wlo.y.min(c.y), wlo.z.min(c.z));
                    whi = Vec3::new(whi.x.max(c.x), whi.y.max(c.y), whi.z.max(c.z));
                }
                boxes_meet(wlo, whi, opts.region)
            }
            None => boxes_meet(ent.position, ent.position, opts.region),
        }
    });

    for i in leaves {
        let node = &nodes[i];
        let (name, _, ymap) = &maps[node.map];
        let entity = ymap.entities[node.entity];
        let instance = if entity.is_mlo_instance {
            ymap.mlo_instances.iter().find(|m| m.entity == entity).cloned()
        } else {
            None
        };
        region.leaves.push(RegionEntity { entity, map: *name, instance });
    }
    region.maps = maps
        .iter()
        .map(|(name, path, ymap)| RegionMap { name: *name, inner_path: path.clone(), header: ymap.header, entity_count: ymap.entities.len() })
        .collect();
    Ok(region)
}

/// `EntityVisibleAtMaxLodLevel`.
fn visible_at_max(ent: &YmapEntity, max_lod: u32) -> bool {
    max_lod == YmapEntity::LOD_ORPHANHD || !(ent.lod_level == YmapEntity::LOD_ORPHANHD || ent.lod_level < max_lod)
}

/// `EntityChildrenVisibleAtMaxLodLevel`.
fn children_visible_at_max(ent: &YmapEntity, max_lod: u32) -> bool {
    max_lod == YmapEntity::LOD_ORPHANHD || !(ent.lod_level == YmapEntity::LOD_ORPHANHD || ent.lod_level <= max_lod)
}

/// `RecurseAddVisibleLeaves`: down the tree while `GetEntityChildren`
/// hands the children over (all of them present, and the view inside the
/// child distance or one of them in its own range), else a leaf.
fn descend(nodes: &[Node], maps: &[(u32, String, Ymap)], i: usize, dist: f32, max_lod: u32, leaves: &mut Vec<usize>) {
    let node = &nodes[i];
    let ent = &maps[node.map].2.entities[node.entity];
    let show_children = children_visible_at_max(ent, max_lod)
        && !node.children.is_empty()
        && node.children.len() as u32 >= ent.num_children
        && (dist <= node.child_lod_dist || node.children.iter().any(|c| dist <= nodes[*c].lod_dist));
    if show_children {
        for c in &node.children {
            descend(nodes, maps, *c, dist, max_lod, leaves);
        }
    } else {
        leaves.push(i);
    }
}

/// The LOD level named on the command line, `hd` to `slod4`.
pub fn parse_lod_level(s: &str) -> Result<u32> {
    let s = s.to_ascii_lowercase();
    YmapEntity::LOD_NAMES
        .iter()
        .position(|n| *n == s)
        .map(|i| i as u32)
        .with_context(|| format!("unknown LOD level '{s}'; use one of {}", YmapEntity::LOD_NAMES.join(", ")))
}

/// A weather name as the manifests hash it.
pub fn weather_hash(name: &str) -> u32 {
    match name.strip_prefix("0x").or_else(|| name.strip_prefix("0X")).and_then(|d| u32::from_str_radix(d, 16).ok()) {
        Some(hash) => hash,
        None => rage_joaat(&name.to_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rage_formats::MapDataNode;

    fn node(name: u32, parent: u32, lo: (f32, f32), hi: (f32, f32)) -> MapDataNode {
        MapDataNode {
            name,
            parent,
            content_flags: 0,
            streaming_min: Vec3::new(lo.0 - 100.0, lo.1 - 100.0, 0.0),
            streaming_max: Vec3::new(hi.0 + 100.0, hi.1 + 100.0, 0.0),
            entities_min: Vec3::new(lo.0, lo.1, 0.0),
            entities_max: Vec3::new(hi.0, hi.1, 0.0),
            flags: [0; 4],
        }
    }

    #[test]
    fn maps_meeting_the_box_come_with_their_parents() {
        let mut index = GameIndex::default();
        index.map_nodes.insert(1, node(1, 10, (0.0, 0.0), (100.0, 100.0)));
        index.map_nodes.insert(10, node(10, 20, (-500.0, -500.0), (500.0, 500.0)));
        index.map_nodes.insert(20, node(20, 0, (-5000.0, -5000.0), (5000.0, 5000.0)));
        index.map_nodes.insert(2, node(2, 10, (300.0, 300.0), (400.0, 400.0)));
        index.map_nodes.insert(3, node(3, 0, (100.0, 100.0), (200.0, 200.0)));
        assert_eq!(select_maps(&index, [50.0, 50.0, 60.0, 60.0]), vec![1, 10, 20]);
        assert_eq!(select_maps(&index, [100.0, 100.0, 100.0, 100.0]), vec![1, 3, 10, 20], "an edge counts");
        assert_eq!(select_maps(&index, [600.0, 600.0, 700.0, 700.0]), vec![20]);
    }

    #[test]
    fn hours_and_weathers_gate_a_map() {
        let mut index = GameIndex::default();
        index.map_hours.insert(7, 0b1111_0000_0000_0000_0000_0000); // on 20..23
        index.map_weathers.insert(8, vec![rage_joaat("rain")]);
        assert!(available(&index, 7, None, None));
        assert!(available(&index, 7, Some(22), None));
        assert!(!available(&index, 7, Some(12), None));
        assert!(available(&index, 9, Some(12), None), "an unlisted map is always on");
        assert!(!available(&index, 8, None, Some(rage_joaat("clear"))));
        assert!(available(&index, 8, None, Some(rage_joaat("rain"))));
        assert!(available(&index, 8, Some(12), None), "weather only gates when asked");
    }

    /// A LOD block in the parent map with two HD props in the child map
    /// and an orphan beside them, walked at three zoom levels and with a
    /// LOD cap, as `RenderLodManager` walks a map view.
    #[test]
    fn the_lod_tree_picks_leaves_by_view_distance() {
        use rage_formats::ymap::tests::{sample_lod_ymap, SampleEntity};
        let lod = SampleEntity { archetype: "slod_block", position: Vec3::new(50.0, 50.0, 0.0), lod_dist: 2000.0, child_lod_dist: 500.0, lod_level: YmapEntity::LOD_LOD, num_children: 2, ..SampleEntity::default() };
        let hd = |name: &'static str, x: f32| SampleEntity { archetype: name, position: Vec3::new(x, 50.0, 0.0), parent_index: 0, flags: 32 | 8, lod_dist: 100.0, ..SampleEntity::default() };
        let orphan = SampleEntity { archetype: "prop_orphan", position: Vec3::new(90.0, 90.0, 0.0), lod_level: YmapEntity::LOD_ORPHANHD, lod_dist: 100.0, ..SampleEntity::default() };
        let parent = parse_ymap(&sample_lod_ymap("block_lod", None, &[lod])).unwrap();
        let child = parse_ymap(&sample_lod_ymap("block", Some("block_lod"), &[hd("prop_a", 40.0), hd("prop_b", 60.0), orphan])).unwrap();
        let maps = || vec![(rage_joaat("block_lod"), "block_lod.ymap".to_string(), parent.clone()), (rage_joaat("block"), "block.ymap".to_string(), child.clone())];
        let run = |detail: f32, max_lod: u32| {
            let opts = RegionOptions { region: [0.0, 0.0, 1000.0, 1000.0], detail, max_lod, hour: None, weather: None, scripted: true };
            let region = Region { view_distance: 1000.0 / detail, ..Default::default() };
            let region = assemble_loaded(&GameIndex::default(), maps(), &opts, region).unwrap();
            let mut names: Vec<u32> = region.leaves.iter().map(|l| l.entity.archetype_hash).collect();
            names.sort_unstable();
            names
        };
        let sorted = |names: &[&str]| { let mut v: Vec<u32> = names.iter().map(|n| rage_joaat(n)).collect(); v.sort_unstable(); v };

        assert_eq!(run(1.0, YmapEntity::LOD_ORPHANHD), sorted(&["slod_block"]), "a kilometre out only the LOD shows");
        assert_eq!(run(4.0, YmapEntity::LOD_ORPHANHD), sorted(&["prop_a", "prop_b"]), "inside the child distance the HD props replace it");
        assert_eq!(run(20.0, YmapEntity::LOD_ORPHANHD), sorted(&["prop_a", "prop_b", "prop_orphan"]), "close up the orphan is in range too");
        assert_eq!(run(20.0, YmapEntity::LOD_LOD), sorted(&["slod_block"]), "capped at LOD, the HD children and the orphan stay hidden");

        // The child map on its own has no parent to hang from.
        let opts = RegionOptions { region: [0.0, 0.0, 1000.0, 1000.0], detail: 20.0, max_lod: YmapEntity::LOD_ORPHANHD, hour: None, weather: None, scripted: true };
        let region = assemble_loaded(&GameIndex::default(), maps()[1..].to_vec(), &opts, Region { view_distance: 50.0, ..Default::default() }).unwrap();
        assert_eq!(region.orphaned, vec!["block.ymap".to_string()]);
        assert!(region.leaves.is_empty());
    }

    #[test]
    fn lod_levels_and_weathers_parse_by_name() {
        assert_eq!(parse_lod_level("HD").unwrap(), YmapEntity::LOD_HD);
        assert_eq!(parse_lod_level("slod4").unwrap(), YmapEntity::LOD_SLOD4);
        assert!(parse_lod_level("ultra").is_err());
        assert_eq!(weather_hash("RAIN"), rage_joaat("rain"));
        assert_eq!(weather_hash("0x10"), 16);
    }
}
