//! A map's `flags`, `contentFlags` and its two extents boxes, worked out
//! from what it holds, the way CodeWalker's `YmapFile.CalcFlags` and
//! `CalcExtents` do on save. The game streams a map in when the camera is
//! inside `streamingExtents` and culls by `entitiesExtents`, so a map whose
//! entities moved without these following is never loaded where they are.
//!
//! The work is done on the generic tree `resource build` writes from, so a
//! map written from XML or JSON gets correct boxes without the tool
//! modelling every `CMapData` member.

use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rage_formats::ymap::rotate;
use rage_formats::{rage_joaat, MetaStruct, MetaValue, MloDef, Vec3, Vec4, Ymap};

use crate::index::{GameIndex, Parts};
use crate::rpf::GtaKeys;

/// What the extents need from an archetype.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArchBounds {
    pub bb_min: Vec3,
    pub bb_max: Vec3,
    pub lod_dist: f32,
}

/// `flags` bits recomputed here; the rest (`SCRIPTED`) are kept as given.
const FLAG_LOD: u32 = 1 << 1;
/// `contentFlags` bits kept as given: `Critical` says nothing about the
/// contents and CodeWalker would drop it.
const CONTENT_KEEP: u32 = 1 << 9;

/// The recomputed header of one map.
#[derive(Debug, Clone, PartialEq)]
pub struct Recalc {
    pub flags: u32,
    pub content_flags: u32,
    /// `(entitiesExtents, streamingExtents)`; `None` when they must be
    /// left alone (see `kept_because`).
    pub extents: Option<((Vec3, Vec3), (Vec3, Vec3))>,
    /// Why the stored extents were kept.
    pub kept_because: Option<&'static str>,
    /// Archetypes placed with no bounds found: their entities count as a
    /// point (and a `lodDist` sphere for streaming), as in CodeWalker.
    pub unbound: Vec<u32>,
}

fn h(name: &str) -> u32 {
    rage_joaat(name)
}

/// An archetype name as the tree holds it: text (hashed lowercase, or a
/// `hash_XXXXXXXX` placeholder read back) or a hash.
pub fn name_hash(v: &MetaValue) -> Option<u32> {
    match v {
        MetaValue::Str(s) if s.is_empty() => None,
        MetaValue::Str(s) => Some(
            s.strip_prefix("hash_")
                .filter(|x| x.len() == 8)
                .and_then(|x| u32::from_str_radix(x, 16).ok())
                .unwrap_or_else(|| rage_joaat(&s.to_lowercase())),
        ),
        MetaValue::Hash(h) | MetaValue::U32(h) => Some(*h),
        MetaValue::I32(i) => Some(*i as u32),
        _ => None,
    }
}

/// `rage__eLodType`, from text, an enum member or a number.
fn lod_level(v: Option<&MetaValue>) -> Option<i64> {
    const NAMES: [&str; 7] = [
        "LODTYPES_DEPTH_HD",
        "LODTYPES_DEPTH_LOD",
        "LODTYPES_DEPTH_SLOD1",
        "LODTYPES_DEPTH_SLOD2",
        "LODTYPES_DEPTH_SLOD3",
        "LODTYPES_DEPTH_ORPHANHD",
        "LODTYPES_DEPTH_SLOD4",
    ];
    match v? {
        MetaValue::Str(s) => NAMES.iter().position(|n| n.eq_ignore_ascii_case(s)).map(|i| i as i64),
        MetaValue::Enum { value, name: Some(n), .. } => NAMES.iter().position(|x| h(x) == *n).map(|i| i as i64).or(Some(i64::from(*value))),
        other => other.as_i64(),
    }
}

fn f32_of(s: &MetaStruct, name: &str) -> Option<f32> {
    s.field(name).and_then(MetaValue::as_f32)
}

fn vec3_of(s: &MetaStruct, name: &str) -> Option<Vec3> {
    s.field(name).and_then(MetaValue::as_vec3)
}

fn items<'a>(s: &'a MetaStruct, name: &str) -> &'a [MetaValue] {
    s.field(name).map_or(&[], MetaValue::items)
}

fn mul(a: Vec3, b: Vec3) -> Vec3 {
    Vec3::new(a.x * b.x, a.y * b.y, a.z * b.z)
}

fn splat(v: f32) -> Vec3 {
    Vec3::new(v, v, v)
}

fn corners(lo: Vec3, hi: Vec3) -> [Vec3; 8] {
    [
        lo,
        Vec3::new(lo.x, lo.y, hi.z),
        Vec3::new(lo.x, hi.y, lo.z),
        Vec3::new(lo.x, hi.y, hi.z),
        Vec3::new(hi.x, lo.y, lo.z),
        Vec3::new(hi.x, lo.y, hi.z),
        Vec3::new(hi.x, hi.y, lo.z),
        hi,
    ]
}

/// A running min/max box, starting inverted as CodeWalker's does (so an
/// empty map ends with `+MAX..-MAX`, which is what it writes too).
#[derive(Clone, Copy)]
struct Aabb(Vec3, Vec3);

impl Aabb {
    fn empty() -> Self {
        Aabb(splat(f32::MAX), splat(f32::MIN))
    }
    fn add(&mut self, lo: Vec3, hi: Vec3) {
        self.0 = self.0.min(lo);
        self.1 = self.1.max(hi);
    }
    fn pair(self) -> (Vec3, Vec3) {
        (self.0, self.1)
    }
}

/// Everything placed with no archetype bounds, and the boxes of what was.
/// `bounds` answers for an archetype name hash.
pub fn calc(map: &MetaStruct, bounds: &dyn Fn(u32) -> Option<ArchBounds>) -> Recalc {
    let mlo_type = h("CMloInstanceDef");
    let mut flags = 0u32;
    let mut content = 0u32;
    let mut ents = Aabb::empty();
    let mut strm = Aabb::empty();
    let mut unbound = Vec::new();

    for item in items(map, "entities") {
        let Some(e) = item.as_struct() else { continue };
        let is_mlo = e.type_hash == mlo_type;
        match lod_level(e.field("lodLevel")) {
            Some(0 | 5) => content |= 1,
            Some(1) => {
                content |= 1 << 1;
                flags |= FLAG_LOD;
            }
            Some(2) => {
                content |= 1 << 4;
                flags |= FLAG_LOD;
            }
            Some(3 | 4 | 6) => {
                content |= (1 << 2) | (1 << 4);
                flags |= FLAG_LOD;
            }
            _ => {}
        }
        if is_mlo {
            content |= 1 << 3;
        }

        let pos = vec3_of(e, "position").unwrap_or(splat(0.0));
        let sxy = f32_of(e, "scaleXY").unwrap_or(1.0);
        let scale = Vec3::new(sxy, sxy, f32_of(e, "scaleZ").unwrap_or(1.0));
        let mut lod = f32_of(e, "lodDist").unwrap_or(0.0);
        let q = e.field("rotation").and_then(|v| match v {
            MetaValue::Vec4(q) => Some(*q),
            _ => None,
        });
        let q = q.unwrap_or(Vec4::new(0.0, 0.0, 0.0, 1.0));
        let len = (q.x * q.x + q.y * q.y + q.z * q.z + q.w * q.w).sqrt();
        let q = if len > 0.0 { [q.x / len, q.y / len, q.z / len, q.w / len] } else { [0.0, 0.0, 0.0, 1.0] };
        // A map stores an entity's rotation inverted, an interior's as is.
        let ori = if is_mlo { q } else { [-q[0], -q[1], -q[2], q[3]] };

        let arch = e.field("archetypeName").and_then(name_hash).and_then(|a| {
            let b = bounds(a);
            if b.is_none() && !unbound.contains(&a) {
                unbound.push(a);
            }
            b
        });
        match arch {
            Some(a) => {
                if lod <= 0.0 {
                    lod = a.lod_dist;
                }
                let (lo, hi) = (mul(a.bb_min, scale), mul(a.bb_max, scale));
                let mut b = Aabb::empty();
                for c in corners(lo, hi) {
                    let p = rotate(c, ori) + pos;
                    b.add(p, p);
                }
                let mut s = Aabb::empty();
                for c in corners(lo - splat(lod), hi + splat(lod)) {
                    let p = rotate(c, ori) + pos;
                    s.add(p, p);
                }
                ents.add(b.0, b.1);
                strm.add(s.0, s.1);
            }
            None => {
                ents.add(pos, pos);
                strm.add(pos - splat(lod), pos + splat(lod));
            }
        }
    }

    if !items(map, "physicsDictionaries").is_empty() {
        content |= 1 << 6;
    }
    let grass = map.field("instancedData").and_then(MetaValue::as_struct).map_or(&[][..], |d| items(d, "GrassInstanceList"));
    if !grass.is_empty() {
        content |= 1 << 10;
    }
    for batch in grass.iter().filter_map(MetaValue::as_struct) {
        let aabb = batch.field("BatchAABB").and_then(MetaValue::as_struct);
        let (Some(lo), Some(hi)) = (aabb.and_then(|a| vec3_of(a, "min")), aabb.and_then(|a| vec3_of(a, "max"))) else { continue };
        let lod = f32_of(batch, "lodDist").unwrap_or(0.0);
        ents.add(lo, hi);
        strm.add(lo - splat(lod), hi + splat(lod));
    }

    for cargen in items(map, "carGenerators").iter().filter_map(MetaValue::as_struct) {
        let pos = vec3_of(cargen, "position").unwrap_or(splat(0.0));
        let len = f32_of(cargen, "perpendicularLength").unwrap_or(0.0);
        ents.add(pos - splat(len), pos + splat(len));
        strm.add(pos - splat(len * 2.0), pos + splat(len * 2.0));
    }

    let mut kept_because = None;
    let lod_lights = map.field("LODLightsSOA").and_then(MetaValue::as_struct);
    if lod_lights.is_some_and(|l| !items(l, "direction").is_empty()) {
        content |= 1 << 7;
        // Their positions live in the parent map's distant lights.
        kept_because = Some("its LOD lights take their positions from the parent map's distant lights");
    }
    let distant = map.field("DistantLODLightsSOA").and_then(MetaValue::as_struct);
    let positions: Vec<Vec3> = distant.map_or(&[][..], |d| items(d, "position")).iter().filter_map(MetaValue::as_vec3).collect();
    if !positions.is_empty() {
        flags |= FLAG_LOD;
        content |= 1 << 8;
        let mut b = Aabb::empty();
        for p in &positions {
            b.add(*p, *p);
        }
        ents.add(b.0 - splat(20.0), b.1 + splat(20.0));
        strm.add(b.0 - splat(3000.0), b.1 + splat(3000.0));
    }

    let boxes = items(map, "boxOccluders");
    let models = items(map, "occludeModels");
    if !boxes.is_empty() || !models.is_empty() {
        content |= 1 << 5;
    }
    for b in boxes.iter().filter_map(MetaValue::as_struct) {
        let g = |n: &str| f32_of(b, n).unwrap_or(0.0) / 4.0;
        let pos = Vec3::new(g("iCenterX"), g("iCenterY"), g("iCenterZ"));
        let size = Vec3::new(g("iLength"), g("iWidth"), g("iHeight"));
        let r = size.dot(size).sqrt() * 0.5;
        ents.add(pos - splat(r), pos + splat(r));
        strm.add(pos - splat(r), pos + splat(r));
    }
    for m in models.iter().filter_map(MetaValue::as_struct) {
        let (Some(lo), Some(hi)) = (vec3_of(m, "bmin"), vec3_of(m, "bmax")) else { continue };
        ents.add(lo, hi);
        strm.add(lo, hi);
    }

    // A map with nothing in it (a stub overriding a vanilla map to empty
    // it) has no extents to work out; CodeWalker would write an inverted
    // box, the stored one is kept instead.
    if kept_because.is_none() && ents.0.x > ents.1.x && strm.0.x > strm.1.x {
        kept_because = Some("the map holds nothing to measure them from");
    }

    Recalc {
        flags,
        content_flags: content,
        extents: kept_because.is_none().then(|| (ents.pair(), strm.pair())),
        kept_because,
        unbound,
    }
}

/// Replaces a member's value keeping the numeric kind it was written with,
/// or adds it.
fn set(s: &mut MetaStruct, name: &str, value: MetaValue) {
    let key = h(name);
    match s.fields.iter_mut().find(|(k, _)| *k == key) {
        Some((_, old)) => *old = value,
        None => s.fields.push((key, value)),
    }
}

fn set_bits(s: &mut MetaStruct, name: &str, bits: u32) {
    let value = match s.field(name) {
        Some(MetaValue::I32(_)) => MetaValue::I32(bits as i32),
        Some(MetaValue::Flags { enum_hash, names, .. }) if names.is_empty() => MetaValue::Flags { enum_hash: *enum_hash, bits, names: vec![] },
        _ => MetaValue::U32(bits),
    };
    set(s, name, value);
}

/// The stored bits and boxes of a map tree.
pub fn stored(map: &MetaStruct) -> (u32, u32) {
    let bits = |n: &str| map.field(n).and_then(MetaValue::as_u32).unwrap_or(0);
    (bits("flags"), bits("contentFlags"))
}

/// Writes `r` into the map, keeping the bits this module does not own.
/// Returns what changed, for the caller to report.
pub fn apply(map: &mut MetaStruct, r: &Recalc) -> Vec<String> {
    let (old_flags, old_content) = stored(map);
    let flags = (old_flags & !FLAG_LOD) | r.flags;
    let content = (old_content & CONTENT_KEEP) | r.content_flags;
    let mut changes = Vec::new();
    if flags != old_flags {
        changes.push(format!("flags {old_flags} -> {flags}"));
    }
    if content != old_content {
        changes.push(format!("contentFlags {old_content} -> {content}"));
    }
    set_bits(map, "flags", flags);
    set_bits(map, "contentFlags", content);
    if let Some(((emin, emax), (smin, smax))) = r.extents {
        let old = |n: &str| vec3_of(map, n);
        let moved = |a: Option<Vec3>, b: Vec3| a.is_none_or(|a| (a - b).dot(a - b) > 1.0e-6);
        if moved(old("entitiesExtentsMin"), emin) || moved(old("entitiesExtentsMax"), emax) {
            changes.push("entitiesExtents".into());
        }
        if moved(old("streamingExtentsMin"), smin) || moved(old("streamingExtentsMax"), smax) {
            changes.push("streamingExtents".into());
        }
        set(map, "streamingExtentsMin", MetaValue::Vec3(smin));
        set(map, "streamingExtentsMax", MetaValue::Vec3(smax));
        set(map, "entitiesExtentsMin", MetaValue::Vec3(emin));
        set(map, "entitiesExtentsMax", MetaValue::Vec3(emax));
    }
    changes
}

fn unit(q: [f32; 4]) -> [f32; 4] {
    let len = q.iter().map(|v| v * v).sum::<f32>().sqrt();
    if len > 0.0 { q.map(|v| v / len) } else { [0.0, 0.0, 0.0, 1.0] }
}

/// An interior's box the way CodeWalker works it out when it loads one
/// (`MloInstanceData.UpdateBBs`) rather than the stored `bbMin`/`bbMax`,
/// which the game's own interiors leave at zero: every room that owns
/// entities spans their boxes, each turned about its own centre by the
/// rotation as stored, and the whole always takes in the MLO origin.
pub fn mlo_box(mlo: &MloDef, bounds: &dyn Fn(u32) -> Option<ArchBounds>) -> (Vec3, Vec3) {
    let mut all = Aabb(splat(0.0), splat(0.0));
    for room in &mlo.rooms {
        if room.attached_objects.is_empty() {
            continue;
        }
        let mut b = Aabb::empty();
        for &i in &room.attached_objects {
            let Some(e) = mlo.entities.get(i as usize) else { continue };
            let Some(a) = bounds(e.archetype_hash) else {
                log::debug!("mlo {:08x}: room entity {i} archetype {:08x} has no bounds", mlo.name_hash, e.archetype_hash);
                continue;
            };
            let scale = Vec3::new(e.scale_xy, e.scale_xy, e.scale_z);
            let (lo, hi) = (mul(a.bb_min, scale), mul(a.bb_max, scale));
            let centre = (lo + hi) * 0.5;
            let q = unit(e.rotation);
            for c in corners(lo, hi) {
                let p = rotate(c - centre, q) + centre + e.position;
                b.add(p, p);
            }
        }
        all.add(b.0, b.1);
    }
    all.pair()
}

/// Where archetype bounds come from: the `.ytyp` files given (a later
/// file wins, as a later-loaded one does in game), then the game index,
/// loaded the first time something is not found locally. Interiors get
/// the box [`mlo_box`] works out from their definition.
pub struct Lookup<'a> {
    local: HashMap<u32, ArchBounds>,
    local_mlos: HashMap<u32, MloDef>,
    exe: Option<&'a Path>,
    keys: Option<&'a GtaKeys>,
    index: OnceCell<Option<GameIndex>>,
    game_mlos: RefCell<HashMap<u32, Option<MloDef>>>,
    /// Archetypes the game supplied.
    pub from_game: RefCell<HashSet<u32>>,
}

impl<'a> Lookup<'a> {
    pub fn new(files: &[PathBuf], exe: Option<&'a Path>, keys: Option<&'a GtaKeys>) -> Self {
        let mut local = HashMap::new();
        let mut local_mlos = HashMap::new();
        for path in files {
            let Ok(data) = std::fs::read(path) else { continue };
            let Ok(ytyp) = rage_formats::parse_ytyp(&data) else {
                eprintln!("warning: {} is not a readable .ytyp; its archetypes are not used", path.display());
                continue;
            };
            for a in ytyp.archetypes {
                local.insert(a.name_hash, ArchBounds { bb_min: a.bb_min, bb_max: a.bb_max, lod_dist: a.lod_dist });
            }
            for m in ytyp.mlos {
                local_mlos.insert(m.name_hash, m);
            }
        }
        Lookup { local, local_mlos, exe, keys, index: OnceCell::new(), game_mlos: RefCell::default(), from_game: RefCell::default() }
    }

    fn index(&self) -> Option<&GameIndex> {
        self.index.get_or_init(|| GameIndex::load(self.exe, self.keys, Parts::MODELS | Parts::INTERIORS)).as_ref()
    }

    /// The stored bounds.
    fn stored(&self, a: u32) -> Option<ArchBounds> {
        if let Some(b) = self.local.get(&a) {
            return Some(*b);
        }
        let index = self.index()?;
        let (bb_min, bb_max) = *index.archetype_box.get(&a)?;
        let lod_dist = index.archetype_lod_dist.get(&a).copied().unwrap_or(0.0);
        self.from_game.borrow_mut().insert(a);
        Some(ArchBounds { bb_min, bb_max, lod_dist })
    }

    fn with_mlo<R>(&self, a: u32, f: impl FnOnce(&MloDef) -> R) -> Option<R> {
        if let Some(m) = self.local_mlos.get(&a) {
            return Some(f(m));
        }
        if !self.local.contains_key(&a) && self.exe.is_some() && !self.game_mlos.borrow().contains_key(&a) {
            let found = self.index().and_then(|index| {
                let loc = index.mlo_ytyp.get(&a)?;
                let data = index.load_bytes(loc, self.keys).ok()?;
                rage_formats::parse_ytyp(&data).ok()?.mlos.into_iter().find(|m| m.name_hash == a)
            });
            self.game_mlos.borrow_mut().insert(a, found);
        }
        self.game_mlos.borrow().get(&a)?.as_ref().map(f)
    }

    /// The bounds [`calc`] needs for archetype `a`.
    pub fn get(&self, a: u32) -> Option<ArchBounds> {
        let stored = self.stored(a);
        match self.with_mlo(a, |m| mlo_box(m, &|x| self.stored(x))) {
            Some((bb_min, bb_max)) => Some(ArchBounds { bb_min, bb_max, lod_dist: stored.map_or(0.0, |b| b.lod_dist) }),
            None => stored,
        }
    }
}

/// How many of a parsed map's entities stand outside its stored boxes:
/// `(outside entitiesExtents, outside streamingExtents)`. Only positions
/// are checked, so a count here is certain, never a matter of bounds.
pub fn strays(ymap: &Ymap) -> (usize, usize) {
    let hd = &ymap.header;
    let inside = |p: Vec3, lo: Vec3, hi: Vec3| {
        const SLACK: f32 = 0.01;
        p.x >= lo.x - SLACK && p.y >= lo.y - SLACK && p.z >= lo.z - SLACK && p.x <= hi.x + SLACK && p.y <= hi.y + SLACK && p.z <= hi.z + SLACK
    };
    let mut out = (0, 0);
    // `entities` holds the interior placements too.
    for p in ymap.entities.iter().map(|e| e.position) {
        if !inside(p, hd.entities_extents_min, hd.entities_extents_max) {
            out.0 += 1;
        }
        if !inside(p, hd.streaming_extents_min, hd.streaming_extents_max) {
            out.1 += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rage_formats::MetaArray;

    fn entity(kind: &str, arch: &str, pos: Vec3, rot: Vec4, lod: f32, level: &str) -> MetaValue {
        MetaValue::Struct(MetaStruct {
            type_hash: h(kind),
            fields: vec![
                (h("archetypeName"), MetaValue::Str(arch.into())),
                (h("position"), MetaValue::Vec3(pos)),
                (h("rotation"), MetaValue::Vec4(rot)),
                (h("scaleXY"), MetaValue::F32(1.0)),
                (h("scaleZ"), MetaValue::F32(1.0)),
                (h("lodDist"), MetaValue::F32(lod)),
                (h("lodLevel"), MetaValue::Str(level.into())),
            ],
        })
    }

    fn map(entities: Vec<MetaValue>) -> MetaStruct {
        MetaStruct {
            type_hash: h("CMapData"),
            fields: vec![
                (h("flags"), MetaValue::I32(1)),
                (h("contentFlags"), MetaValue::I32(512 | 64)),
                (h("entities"), MetaValue::Array(MetaArray { item_type: None, typed_items: true, items: entities })),
            ],
        }
    }

    fn table(name: &str, b: ArchBounds) -> impl Fn(u32) -> Option<ArchBounds> {
        let key = rage_joaat(name);
        move |a| (a == key).then_some(b)
    }

    const IDENTITY: Vec4 = Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 };

    #[test]
    fn an_unrotated_prop_spans_its_box_and_its_lod_distance() {
        let m = map(vec![entity("CEntityDef", "Prop_A", Vec3::new(10.0, 20.0, 30.0), IDENTITY, 100.0, "LODTYPES_DEPTH_HD")]);
        let b = ArchBounds { bb_min: Vec3::new(-1.0, -2.0, 0.0), bb_max: Vec3::new(1.0, 2.0, 3.0), lod_dist: 50.0 };
        let r = calc(&m, &table("prop_a", b));
        let ((emin, emax), (smin, smax)) = r.extents.unwrap();
        assert_eq!((emin, emax), (Vec3::new(9.0, 18.0, 30.0), Vec3::new(11.0, 22.0, 33.0)));
        assert_eq!((smin, smax), (Vec3::new(-91.0, -82.0, -70.0), Vec3::new(111.0, 122.0, 133.0)));
        assert_eq!((r.flags, r.content_flags), (0, 1));
        assert!(r.unbound.is_empty());
    }

    #[test]
    fn the_archetype_lod_distance_stands_in_for_a_missing_one() {
        let m = map(vec![entity("CEntityDef", "prop_a", Vec3::new(0.0, 0.0, 0.0), IDENTITY, 0.0, "LODTYPES_DEPTH_HD")]);
        let b = ArchBounds { bb_min: splat(-1.0), bb_max: splat(1.0), lod_dist: 50.0 };
        let (_, (smin, smax)) = calc(&m, &table("prop_a", b)).extents.unwrap();
        assert_eq!((smin, smax), (splat(-51.0), splat(51.0)));
    }

    #[test]
    fn entities_turn_by_the_inverse_of_the_stored_rotation_and_interiors_do_not() {
        // 90 degrees about z: (x, y) -> (-y, x) for the forward rotation.
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let q = Vec4::new(0.0, 0.0, s, s);
        let b = ArchBounds { bb_min: Vec3::new(0.0, 0.0, 0.0), bb_max: Vec3::new(4.0, 1.0, 1.0), lod_dist: 0.0 };
        let ent = calc(&map(vec![entity("CEntityDef", "a", splat(0.0), q, 1.0, "LODTYPES_DEPTH_HD")]), &table("a", b));
        let ((lo, hi), _) = ent.extents.unwrap();
        assert!((lo.y + 4.0).abs() < 1e-5 && hi.y.abs() < 1e-5, "{lo:?} {hi:?}");
        let mlo = calc(&map(vec![entity("CMloInstanceDef", "a", splat(0.0), q, 1.0, "LODTYPES_DEPTH_ORPHANHD")]), &table("a", b));
        let ((lo, hi), _) = mlo.extents.unwrap();
        assert!(lo.y.abs() < 1e-5 && (hi.y - 4.0).abs() < 1e-5, "{lo:?} {hi:?}");
        assert_eq!(mlo.content_flags, 1 | 8);
    }

    #[test]
    fn an_unknown_archetype_counts_as_a_point_and_is_reported() {
        let m = map(vec![entity("CEntityDef", "hash_0000BEEF", Vec3::new(5.0, 5.0, 5.0), IDENTITY, 10.0, "LODTYPES_DEPTH_LOD")]);
        let r = calc(&m, &|_| None);
        assert_eq!(r.unbound, vec![0xBEEF]);
        let ((emin, emax), (smin, _)) = r.extents.unwrap();
        assert_eq!((emin, emax, smin), (splat(5.0), splat(5.0), splat(-5.0)));
        assert_eq!((r.flags, r.content_flags), (2, 2));
    }

    #[test]
    fn apply_keeps_the_scripted_and_critical_bits_and_the_number_kind() {
        let mut m = map(vec![entity("CEntityDef", "a", splat(0.0), IDENTITY, 10.0, "LODTYPES_DEPTH_SLOD2")]);
        let r = calc(&m, &|_| None);
        let changes = apply(&mut m, &r);
        assert_eq!(m.field("flags"), Some(&MetaValue::I32(1 | 2)));
        assert_eq!(m.field("contentFlags"), Some(&MetaValue::I32(512 | 4 | 16)));
        assert_eq!(m.field("entitiesExtentsMin"), Some(&MetaValue::Vec3(splat(0.0))));
        assert!(changes.iter().any(|c| c.starts_with("contentFlags")), "{changes:?}");
    }

    #[test]
    fn an_empty_map_keeps_its_extents() {
        let r = calc(&map(vec![]), &|_| None);
        assert!(r.extents.is_none() && r.kept_because.is_some());
        assert_eq!((r.flags, r.content_flags), (0, 0));
    }

    #[test]
    fn lod_lights_leave_the_extents_alone() {
        let mut m = map(vec![]);
        m.fields.push((
            h("LODLightsSOA"),
            MetaValue::Struct(MetaStruct {
                type_hash: 0,
                fields: vec![(h("direction"), MetaValue::Array(MetaArray { item_type: None, typed_items: false, items: vec![MetaValue::Vec3(splat(1.0))] }))],
            }),
        ));
        let r = calc(&m, &|_| None);
        assert!(r.extents.is_none() && r.kept_because.is_some());
        assert_eq!(r.content_flags, 1 << 7);
    }
}
