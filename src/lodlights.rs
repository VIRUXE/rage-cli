//! LOD lights the way CodeWalker's project window generates them
//! (`GenerateLODLightsPanel`): each light of each placed entity's model,
//! carried into the world by its bone and the entity's placement, hashed
//! as the game hashes an entity's lights (`YmapEntityDef.EnsureLights` and
//! `ComputeLightHash`), and written as a `_lodlights` map (`CLODLight`,
//! one direction per light) whose parent `_distantlights` map
//! (`CDistantLODLight`) holds the positions and colours. The game lights
//! a custom map at distance from these; without them a placed lamp goes
//! dark as soon as its entity streams out.

use std::collections::HashMap;

use anyhow::{Context, Result};
use rage_formats::blocks::base::StructArray;
use rage_formats::blocks::bounds::BoundBlock;
use rage_formats::blocks::drawable::Drawable;
use rage_formats::blocks::light::Light;
use rage_formats::blocks::skeleton::{Bone, Skeleton, SkeletonBonesBlock};
use rage_formats::blocks::{BlockId, Graph, Reader};
use rage_formats::resource::SYSTEM_BASE;
use rage_formats::{rage_joaat, Mat4, MetaArray, MetaStruct, MetaValue, Vec3, Vec4};

/// What the generator needs of a model: its lights, its bones' absolute
/// transforms by tag, its bounding box and its collision bound's box (the
/// last two feed the light hash).
#[derive(Debug, Clone, Default)]
pub struct ModelLights {
    pub lights: Vec<Light>,
    pub bones: HashMap<u16, Mat4>,
    pub bb_min: Vec3,
    pub bb_max: Vec3,
    pub bound: Option<(Vec3, Vec3)>,
}

/// What the generator needs of an archetype: its box, and how many
/// extensions it declares (the light hash counts lights after them).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ArchLights {
    pub bb_min: Vec3,
    pub bb_max: Vec3,
    /// `drawableDictionary`, 0 when the model is a file of its own.
    pub drawable_dict: u32,
    pub extensions: u32,
}

/// An entity's placement, as CodeWalker keeps it: `orientation` is the
/// stored rotation inverted for a `CEntityDef`, kept as is for an interior.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub position: Vec3,
    pub orientation: [f32; 4],
    pub scale: Vec3,
}

impl Placement {
    /// `YmapEntityDef`'s constructor: a map stores an entity's rotation
    /// inverted, an interior's as is.
    pub fn new(position: Vec3, rotation: [f32; 4], scale_xy: f32, scale_z: f32, is_mlo: bool) -> Self {
        let [x, y, z, w] = rotation;
        let orientation = if is_mlo || rotation == [0.0, 0.0, 0.0, 1.0] {
            rotation
        } else {
            // `Quaternion.Invert`: the conjugate over the squared length.
            let n = x * x + y * y + z * z + w * w;
            [-x / n, -y / n, -z / n, w / n]
        };
        Placement { position, orientation, scale: Vec3::new(scale_xy, scale_xy, scale_z) }
    }
}

/// One generated light: a `CLODLight` row and its `CDistantLODLight` row.
#[derive(Debug, Clone, PartialEq)]
pub struct LodLight {
    pub position: Vec3,
    pub colour: u32,
    pub direction: Vec3,
    pub falloff: f32,
    pub falloff_exponent: f32,
    pub time_and_state_flags: u32,
    pub hash: u32,
    pub cone_inner_angle: u8,
    pub cone_outer_angle_or_cap_ext: u8,
    pub corona_intensity: u8,
    pub is_street_light: bool,
}

// ─── Reading models ────────────────────────────────────────────────────────

fn u16_le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u64_le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn vec3_le(b: &[u8], at: usize) -> Vec3 {
    let f = |o: usize| f32::from_le_bytes(b[at + o..at + o + 4].try_into().unwrap());
    Vec3::new(f(0), f(4), f(8))
}

/// The lights of a `.ydr`.
pub fn read_ydr(bytes: &[u8]) -> Result<ModelLights> {
    let mut r = Reader::open(bytes)?;
    let mut g = Graph::new();
    let id = Drawable::read(&mut r, &mut g, SYSTEM_BASE)?.context("the file has no drawable")?;
    Ok(model_lights(&g, id, None, None))
}

/// The lights of the `.ydd` member named `member`; `Ok(None)` when the
/// dictionary has no such member (CodeWalker then uses no model at all).
pub fn read_ydd_member(bytes: &[u8], member: u32) -> Result<Option<ModelLights>> {
    let mut r = Reader::open(bytes)?;
    let head = r.slice(SYSTEM_BASE, 0x40).context("the file is too short for a drawable dictionary")?;
    let hashes_ptr = u64_le(head, 0x20);
    let hashes_count = u16_le(head, 0x28) as usize;
    let drawables_ptr = u64_le(head, 0x30);
    let drawables_count = u16_le(head, 0x38) as usize;
    let hashes = r.u32s(hashes_ptr, hashes_count).context("the dictionary's hash list")?;
    let pointers = r.u64s(drawables_ptr, drawables_count).context("the dictionary's drawable list")?;
    let Some(ptr) = hashes.iter().position(|&h| h == member).and_then(|i| pointers.get(i).copied()).filter(|&p| p != 0) else {
        return Ok(None);
    };
    let mut g = Graph::new();
    let id = Drawable::read(&mut r, &mut g, ptr)?.with_context(|| format!("member {member:#010x} has no drawable"))?;
    Ok(Some(model_lights(&g, id, None, None)))
}

/// The lights of a `.yft`: the fragment's own light list (`FragType`,
/// 0x110), placed by its drawable's skeleton, with the first physics LOD's
/// bound. A `FragDrawable` is a `DrawableBase` (0xA8 bytes) with the
/// fragment's matrices after it, not a `Drawable`, so its skeleton and box
/// are read here rather than through [`Drawable::read`].
pub fn read_yft(bytes: &[u8]) -> Result<ModelLights> {
    let mut r = Reader::open(bytes)?;
    let head = r.slice(SYSTEM_BASE, 0x130).context("the file is too short for a fragment")?;
    let drawable_ptr = u64_le(head, 0x30);
    let physics_ptr = u64_le(head, 0xF0);
    let lights_ptr = u64_le(head, 0x110);
    let lights_count = u16_le(head, 0x118) as usize;
    let lights: Vec<Light> = r.structs(lights_ptr, lights_count).context("the fragment's light list")?;
    // FragPhysicsLODGroup.PhysicsLOD1 (0x10) -> FragPhysicsLOD.Bound (0xE8).
    let bound = r
        .slice(physics_ptr, 0x30)
        .ok()
        .map(|group| u64_le(group, 0x10))
        .and_then(|lod| r.slice(lod, 0x130).ok().map(|lod| u64_le(lod, 0xE8)))
        .and_then(|bound| bound_box(&r, bound));
    let base = r.slice(drawable_ptr, 0xA8).context("the fragment has no drawable")?;
    let skeleton_ptr = u64_le(base, 0x18);
    let (bb_min, bb_max) = (vec3_le(base, 0x30), vec3_le(base, 0x40));
    let mut g = Graph::new();
    let skeleton = Skeleton::read(&mut r, &mut g, skeleton_ptr)?;
    let bones = skeleton.map_or_else(HashMap::new, |s| bone_transforms(&skeleton_bones(&g, s)));
    Ok(ModelLights { lights, bones, bb_min, bb_max, bound })
}

/// `Bounds.BoxMin` (0x30) and `BoxMax` (0x20) of the bound at `va`.
fn bound_box(r: &Reader, va: u64) -> Option<(Vec3, Vec3)> {
    let b = r.slice(va, 0x40).ok()?;
    Some((vec3_le(b, 0x30), vec3_le(b, 0x20)))
}

fn model_lights(g: &Graph, id: BlockId, lights: Option<Vec<Light>>, bound: Option<(Vec3, Vec3)>) -> ModelLights {
    let d = g.get::<Drawable>(id);
    let lights = lights.unwrap_or_else(|| d.lights.map_or_else(Vec::new, |l| g.get::<StructArray<Light>>(l).items.clone()));
    let bones = d.skeleton.map_or_else(HashMap::new, |s| bone_transforms(&skeleton_bones(g, s)));
    let bound = bound.or_else(|| {
        d.bound.map(|b| {
            let c = g.get::<BoundBlock>(b).common();
            (c.box_min, c.box_max)
        })
    });
    ModelLights { lights, bones, bb_min: d.bounding_box_min, bb_max: d.bounding_box_max, bound }
}

/// A bone as the transforms need it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BonePose {
    pub tag: u16,
    /// Index into the skeleton's bone list, -1 for a root.
    pub parent: i16,
    pub rotation: Vec4,
    pub translation: Vec3,
    pub scale: Vec3,
}

fn skeleton_bones(g: &Graph, skeleton: BlockId) -> Vec<BonePose> {
    let Some(block) = g.get::<Skeleton>(skeleton).bones else { return Vec::new() };
    g.get::<SkeletonBonesBlock>(block)
        .bones
        .iter()
        .map(|&b| {
            let bone = g.get::<Bone>(b);
            BonePose { tag: bone.tag, parent: bone.parent, rotation: bone.rotation, translation: bone.translation, scale: bone.scale }
        })
        .collect()
}

/// `Bone.UpdateAnimTransform` for every bone, by tag: the local affine
/// transform (its diagonal scaled, as SharpDX's `ScaleVector` does) times
/// the parent's absolute one.
pub fn bone_transforms(bones: &[BonePose]) -> HashMap<u16, Mat4> {
    fn absolute(bones: &[BonePose], i: usize, memo: &mut Vec<Option<Mat4>>, depth: usize) -> Mat4 {
        if let Some(m) = memo[i] {
            return m;
        }
        let b = &bones[i];
        let mut local = Mat4::from_quat_pos(b.rotation, b.translation);
        let s = local.scale_vector();
        local.set_scale_vector(Vec3::new(s.x * b.scale.x, s.y * b.scale.y, s.z * b.scale.z));
        let parent = usize::try_from(b.parent).ok().filter(|&p| p < bones.len() && p != i && depth < bones.len());
        let m = match parent {
            Some(p) => mul_d3d(&local, &absolute(bones, p, memo, depth + 1)),
            None => local,
        };
        memo[i] = Some(m);
        m
    }
    let mut memo = vec![None; bones.len()];
    let mut out = HashMap::new();
    for i in 0..bones.len() {
        let m = absolute(bones, i, &mut memo, 0);
        // `BonesMap[bone.Tag] = bone`: a repeated tag keeps the later bone.
        out.insert(bones[i].tag, m);
    }
    out
}

fn row(m: &Mat4, i: usize) -> [f32; 4] {
    let r = m.row(i);
    [r.x, r.y, r.z, r.w]
}

/// SharpDX `Matrix.Multiply(a, b)`: `a` then `b` on a row vector.
fn mul_d3d(a: &Mat4, b: &Mat4) -> Mat4 {
    let rows: [[f32; 4]; 4] = std::array::from_fn(|i| row(b, i));
    let mut out = Mat4::identity();
    for i in 0..4 {
        let r = row(a, i);
        let sum = |c: usize| r[0] * rows[0][c] + r[1] * rows[1][c] + r[2] * rows[2][c] + r[3] * rows[3][c];
        out.set_row(i, Vec4::new(sum(0), sum(1), sum(2), sum(3)));
    }
    out
}

/// CodeWalker's `Quaternion.Multiply(Vector3)`: `v` rotated by `q`.
pub fn quat_multiply(q: [f32; 4], v: Vec3) -> Vec3 {
    let [x, y, z, w] = q;
    let (axx, ayy, azz) = (x * 2.0, y * 2.0, z * 2.0);
    let (awxx, awyy, awzz) = (w * axx, w * ayy, w * azz);
    let (axxx, axyy, axzz) = (x * axx, x * ayy, x * azz);
    let (ayyy, ayzz, azzz) = (y * ayy, y * azz, z * azz);
    Vec3::new(
        v.x * ((1.0 - ayyy) - azzz) + v.y * (axyy - awzz) + v.z * (axzz + awyy),
        v.x * (axyy + awzz) + v.y * ((1.0 - axxx) - azzz) + v.z * (ayzz - awxx),
        v.x * (axzz - awyy) + v.y * (ayzz + awxx) + v.z * ((1.0 - axxx) - ayyy),
    )
}

// ─── Placing and hashing ───────────────────────────────────────────────────

/// `YmapEntityDef.ComputeLightHash`: the game's hash of an entity light,
/// over the entity's world box (in tenths) and the light's index.
pub fn compute_light_hash(ints: &[u32], seed: u32) -> u32 {
    let mut v3 = ints.len();
    let mut v5 = seed.wrapping_add(0xDEAD_BEEF).wrapping_add(4 * ints.len() as u32);
    let mut v6 = v5;
    let mut v7 = v5;
    let mut c = 0;
    let rounds = if ints.len() >= 4 { (ints.len() - 4) / 3 + 1 } else { 0 };
    for _ in 0..rounds {
        let v9 = ints[c + 2].wrapping_add(v5);
        let v10 = ints[c + 1].wrapping_add(v6);
        let v11 = ints[c].wrapping_sub(v9);
        let v13 = v10.wrapping_add(v9);
        let v14 = v7.wrapping_add(v11) ^ v9.rotate_left(4);
        let v15 = v10.wrapping_sub(v14);
        let v17 = v13.wrapping_add(v14);
        let v18 = v15 ^ v14.rotate_left(6);
        let v19 = v13.wrapping_sub(v18);
        let v21 = v17.wrapping_add(v18);
        let v22 = v19 ^ v18.rotate_left(8);
        let v23 = v17.wrapping_sub(v22);
        let v25 = v21.wrapping_add(v22);
        let v26 = v23 ^ v22.rotate_left(16);
        let v27 = v21.wrapping_sub(v26);
        let v29 = v27 ^ v26.rotate_right(13);
        let v30 = v25.wrapping_sub(v29);
        v7 = v25.wrapping_add(v26);
        v6 = v7.wrapping_add(v29);
        v5 = v30 ^ v29.rotate_left(4);
        v3 -= 3;
        c += 3;
    }
    if v3 == 3 {
        v5 = v5.wrapping_add(ints[c + 2]);
    }
    if v3 >= 2 {
        v6 = v6.wrapping_add(ints[c + 1]);
    }
    if v3 >= 1 {
        let v34 = (v6 ^ v5).wrapping_sub(v6.rotate_left(14));
        let v35 = (v34 ^ v7.wrapping_add(ints[c])).wrapping_sub(v34.rotate_left(11));
        let v36 = (v35 ^ v6).wrapping_sub(v35.rotate_right(7));
        let v37 = (v36 ^ v34).wrapping_sub(v36.rotate_left(16));
        let v38 = v37.rotate_left(4);
        let v39 = (((v35 ^ v37).wrapping_sub(v38)) ^ v36).wrapping_sub((v35 ^ v37).wrapping_sub(v38).rotate_left(14));
        return (v39 ^ v37).wrapping_sub(v39.rotate_right(8));
    }
    v5
}

/// `BoundingBox.Transform(position, orientation, scale)`: the axis-aligned
/// box around the scaled, turned and moved box.
fn transform_box(lo: Vec3, hi: Vec3, p: &Placement) -> (Vec3, Vec3) {
    let center = (hi + lo) * 0.5;
    let extent = (hi - lo) * 0.5;
    let scaled = |v: Vec3| Vec3::new(v.x * p.scale.x, v.y * p.scale.y, v.z * p.scale.z);
    let ncenter = quat_multiply(p.orientation, scaled(center)) + p.position;
    // `TransformNormal` by the absolute matrix: each axis' extent spread
    // over the axes it turns into.
    let e = scaled(extent);
    let mut nextent = Vec3::ZERO;
    for (axis, len) in [(Vec3::new(1.0, 0.0, 0.0), e.x), (Vec3::new(0.0, 1.0, 0.0), e.y), (Vec3::new(0.0, 0.0, 1.0), e.z)] {
        let turned = quat_multiply(p.orientation, axis).abs();
        nextent = nextent + turned * len;
    }
    (ncenter - nextent.abs(), ncenter + nextent.abs())
}

/// C#'s `(uint)` of a float: truncation, negatives wrapping.
fn as_u32(v: f32) -> u32 {
    v as i64 as u32
}

/// C#'s `(byte)Math.Round(v)`: half to even, then the low byte.
fn round_byte(v: f32) -> u8 {
    f64::from(v).round_ties_even() as i64 as u8
}

/// `EnsureLights` and the generator's loop for one entity: where each
/// model light sits in the world, and its `CLODLight` values.
pub fn entity_lights(p: &Placement, arch: &ArchLights, model: &ModelLights) -> Vec<LodLight> {
    let mut abmin = arch.bb_min.min(model.bb_min);
    let mut abmax = arch.bb_max.max(model.bb_max);
    if let Some((lo, hi)) = model.bound {
        abmin = abmin.min(lo);
        abmax = abmax.max(hi);
    }
    let (lo, hi) = transform_box(abmin, abmax, p);
    let mut ints = [as_u32(lo.x * 10.0), as_u32(lo.y * 10.0), as_u32(lo.z * 10.0), as_u32(hi.x * 10.0), as_u32(hi.y * 10.0), as_u32(hi.z * 10.0), 0];

    model
        .lights
        .iter()
        .enumerate()
        .map(|(i, la)| {
            ints[6] = arch.extensions.wrapping_add(i as u32);
            let xform = model.bones.get(&la.bone_id).copied().unwrap_or_else(Mat4::identity);
            let position = quat_multiply(p.orientation, xform.transform_point(la.position).xyz()) + p.position;
            let direction = quat_multiply(p.orientation, xform.transform_vector(la.direction));

            let intensity = f64::from(la.intensity * 5.3125).round_ties_even().clamp(0.0, 255.0) as u32;
            let colour = (intensity << 24) + (u32::from(la.r) << 16) + (u32::from(la.g) << 8) + u32::from(la.b);
            // CodeWalker reads the street-light bit and then discards it ("TODO: fix this!").
            let is_street_light = false;
            let light_type = u32::from(la.light_type);
            let unk = u32::from(is_street_light);
            let time_and_state_flags = la.time_flags | (light_type << 26) | (unk << 24);
            let cone_inner_angle = round_byte(la.cone_inner_angle * 1.4117647);
            let cone_outer_angle_or_cap_ext = if light_type == 4 { round_byte(la.extent.x * 1.82) } else { round_byte(la.cone_outer_angle * 1.4117647) };
            let corona_intensity = if la.corona_size != 0.0 { (la.corona_intensity * 6.0) as i64 as u8 } else { 0 };
            LodLight {
                position,
                colour,
                direction,
                falloff: la.falloff,
                falloff_exponent: la.falloff_exponent,
                time_and_state_flags,
                hash: compute_light_hash(&ints, 0),
                cone_inner_angle,
                cone_outer_angle_or_cap_ext,
                corona_intensity,
                is_street_light,
            }
        })
        .collect()
}

// ─── The two maps ──────────────────────────────────────────────────────────

/// The generator's order: street lights first, then by hash.
pub fn sort_lights(lights: &mut [LodLight]) {
    lights.sort_by(|a, b| b.is_street_light.cmp(&a.is_street_light).then(a.hash.cmp(&b.hash)));
}

/// The box around the lights.
fn bounds(lights: &[LodLight]) -> (Vec3, Vec3) {
    let mut lo = Vec3::new(f32::MAX, f32::MAX, f32::MAX);
    let mut hi = Vec3::new(f32::MIN, f32::MIN, f32::MIN);
    for l in lights {
        lo = lo.min(l.position);
        hi = hi.max(l.position);
    }
    (lo, hi)
}

/// The `CMapData` header of a generated map: CodeWalker's `CalcFlags` and
/// `CalcExtents` for a map holding only LOD lights (`lod`) or only distant
/// lights.
#[derive(Debug, Clone, PartialEq)]
pub struct MapHeader {
    pub name: String,
    pub parent: String,
    pub flags: u32,
    pub content_flags: u32,
    pub entities_extents: (Vec3, Vec3),
    pub streaming_extents: (Vec3, Vec3),
}

pub fn headers(name: &str, lights: &[LodLight]) -> (MapHeader, MapHeader) {
    let (lo, hi) = bounds(lights);
    let grow = |d: f32| (lo - Vec3::new(d, d, d), hi + Vec3::new(d, d, d));
    let lodname = format!("{name}_lodlights");
    let distname = format!("{name}_distantlights");
    let lod = MapHeader { name: lodname, parent: distname.clone(), flags: 0, content_flags: 1 << 7, entities_extents: grow(20.0), streaming_extents: grow(950.0) };
    let dist = MapHeader { name: distname, parent: String::new(), flags: 2, content_flags: 1 << 8, entities_extents: grow(20.0), streaming_extents: grow(3000.0) };
    (lod, dist)
}

fn array(items: Vec<MetaValue>) -> MetaValue {
    MetaValue::Array(MetaArray { item_type: None, typed_items: false, items })
}

fn st(ty: &str, fields: Vec<(&str, MetaValue)>) -> MetaValue {
    MetaValue::Struct(MetaStruct { type_hash: rage_joaat(ty), fields: fields.into_iter().map(|(n, v)| (rage_joaat(n), v)).collect() })
}

/// `CLODLight` with the lights' rows, or empty.
pub fn lod_lights_soa(lights: &[LodLight]) -> MetaValue {
    st(
        "CLODLight",
        vec![
            ("direction", array(lights.iter().map(|l| MetaValue::Vec3(l.direction)).collect())),
            ("falloff", array(lights.iter().map(|l| MetaValue::F32(l.falloff)).collect())),
            ("falloffExponent", array(lights.iter().map(|l| MetaValue::F32(l.falloff_exponent)).collect())),
            ("timeAndStateFlags", array(lights.iter().map(|l| MetaValue::U32(l.time_and_state_flags)).collect())),
            ("hash", array(lights.iter().map(|l| MetaValue::U32(l.hash)).collect())),
            ("coneInnerAngle", array(lights.iter().map(|l| MetaValue::U8(l.cone_inner_angle)).collect())),
            ("coneOuterAngleOrCapExt", array(lights.iter().map(|l| MetaValue::U8(l.cone_outer_angle_or_cap_ext)).collect())),
            ("coronaIntensity", array(lights.iter().map(|l| MetaValue::U8(l.corona_intensity)).collect())),
        ],
    )
}

/// `CDistantLODLight` with the lights' rows, or empty. The generator
/// writes category 1 (medium) and counts the street lights.
pub fn distant_lights_soa(lights: &[LodLight]) -> MetaValue {
    let street = lights.iter().filter(|l| l.is_street_light).count() as u16;
    st(
        "CDistantLODLight",
        vec![
            ("position", array(lights.iter().map(|l| MetaValue::Vec3(l.position)).collect())),
            ("RGBI", array(lights.iter().map(|l| MetaValue::U32(l.colour)).collect())),
            ("numStreetLights", MetaValue::U16(street)),
            ("category", MetaValue::U16(if lights.is_empty() { 0 } else { 1 })),
        ],
    )
}

/// Replaces (or adds) the member `name` of `map`.
pub fn set_member(map: &mut MetaStruct, name: &str, value: MetaValue) {
    let key = rage_joaat(name);
    match map.fields.iter_mut().find(|(k, _)| *k == key) {
        Some((_, v)) => *v = value,
        None => map.fields.push((key, value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f32, y: f32, z: f32) -> Vec3 {
        Vec3::new(x, y, z)
    }

    fn near(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-4
    }

    /// 90 degrees about Z as (x, y, z, w).
    const Z90: [f32; 4] = [0.0, 0.0, std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2];

    #[test]
    fn the_hash_is_the_reference_one() {
        // The values CodeWalker.Core's YmapEntityDef.ComputeLightHash gives for these words
        // (`codewalker-cli lighthash ...`): a box in tenths and a light index, one with the
        // negative coordinates a float-to-uint cast wraps.
        assert_eq!(compute_light_hash(&[10, 20, 30, 40, 50, 60, 0], 0), 187307637);
        assert_eq!(compute_light_hash(&[10, 20, 30, 40, 50, 60, 1], 0), 3196790452);
        assert_eq!(compute_light_hash(&[990, 1990, 300, 1010, 2010, 360, 1], 0), 2542007328);
        assert_eq!(compute_light_hash(&[4294967284, 4294967196, 0, 120, 3000, 4294967295, 5], 0), 2896900932);
    }

    #[test]
    fn a_placement_inverts_the_stored_rotation_but_not_an_interiors() {
        let p = Placement::new(v(1.0, 2.0, 3.0), Z90, 1.0, 1.0, false);
        assert!(near(quat_multiply(p.orientation, v(1.0, 0.0, 0.0)), v(0.0, -1.0, 0.0)));
        let m = Placement::new(v(1.0, 2.0, 3.0), Z90, 1.0, 1.0, true);
        assert!(near(quat_multiply(m.orientation, v(1.0, 0.0, 0.0)), v(0.0, 1.0, 0.0)));
        assert_eq!(Placement::new(Vec3::ZERO, [0.0, 0.0, 0.0, 1.0], 2.0, 3.0, false).scale, v(2.0, 2.0, 3.0));
    }

    #[test]
    fn a_child_bone_is_placed_by_its_parent() {
        // The parent turns 90 degrees about Z and sits at (0, 5, 0); the child sits 1 along
        // the parent's X, turned 90 degrees about X. Its origin lands at (0, 6, 0).
        let x90 = [std::f32::consts::FRAC_1_SQRT_2, 0.0, 0.0, std::f32::consts::FRAC_1_SQRT_2];
        let bones = [
            BonePose { tag: 0, parent: -1, rotation: Vec4::new(Z90[0], Z90[1], Z90[2], Z90[3]), translation: v(0.0, 5.0, 0.0), scale: v(1.0, 1.0, 1.0) },
            BonePose { tag: 7, parent: 0, rotation: Vec4::new(x90[0], x90[1], x90[2], x90[3]), translation: v(1.0, 0.0, 0.0), scale: v(1.0, 1.0, 1.0) },
        ];
        let xf = bone_transforms(&bones);
        let child = &xf[&7];
        assert!(near(child.transform_point(Vec3::ZERO).xyz(), v(0.0, 6.0, 0.0)), "{:?}", child.transform_point(Vec3::ZERO));
        // A point 1 along the child's Y: the child turns it to +Z, the parent leaves Z alone.
        assert!(near(child.transform_point(v(0.0, 1.0, 0.0)).xyz(), v(0.0, 6.0, 1.0)));
        // Directions ignore the translation.
        assert!(near(child.transform_vector(v(0.0, 1.0, 0.0)), v(0.0, 0.0, 1.0)));
    }

    fn light(pos: Vec3, dir: Vec3) -> Light {
        Light {
            position: pos,
            direction: dir,
            r: 255,
            g: 142,
            b: 81,
            intensity: 10.0,
            light_type: 2,
            time_flags: 15728703,
            falloff: 5.0,
            falloff_exponent: 30.0,
            cone_inner_angle: 30.0,
            cone_outer_angle: 90.0,
            corona_size: 1.5,
            corona_intensity: 1.0,
            extent: v(1.0, 1.0, 1.0),
            ..Light::default()
        }
    }

    #[test]
    fn an_entity_light_is_turned_and_moved_and_packed_as_the_generator_does() {
        let model = ModelLights { lights: vec![light(v(0.0, 0.0, 5.0), v(0.0, 0.0, -1.0)), light(v(1.0, 0.0, 5.0), v(1.0, 0.0, 0.0))], bones: HashMap::new(), bb_min: v(-1.0, -1.0, 0.0), bb_max: v(1.0, 1.0, 6.0), bound: None };
        let arch = ArchLights { bb_min: v(-1.0, -1.0, 0.0), bb_max: v(1.0, 1.0, 6.0), drawable_dict: 0, extensions: 1 };
        // Stored rotation Z90 means the entity turns by -90 degrees: local +X becomes world -Y.
        let p = Placement::new(v(100.0, 200.0, 30.0), Z90, 1.0, 1.0, false);
        let out = entity_lights(&p, &arch, &model);
        assert_eq!(out.len(), 2);
        assert!(near(out[0].position, v(100.0, 200.0, 35.0)));
        assert!(near(out[0].direction, v(0.0, 0.0, -1.0)));
        assert!(near(out[1].position, v(100.0, 199.0, 35.0)));
        assert!(near(out[1].direction, v(0.0, -1.0, 0.0)));
        // Intensity 10 * 5.3125 = 53.125 -> 53; colour IRGB.
        assert_eq!(out[0].colour, (53 << 24) | (255 << 16) | (142 << 8) | 81);
        // Spot (2) in the top bits of the time flags.
        assert_eq!(out[0].time_and_state_flags, 15728703 | (2 << 26));
        assert_eq!((out[0].cone_inner_angle, out[0].cone_outer_angle_or_cap_ext), (42, 127));
        assert_eq!(out[0].corona_intensity, 6);
        assert_eq!(out[0].falloff, 5.0);
        // The box (99, 199, 30)..(101, 201, 36) in tenths, light 1 and 2 (after one extension).
        assert_eq!(out[0].hash, compute_light_hash(&[990, 1990, 300, 1010, 2010, 360, 1], 0));
        assert_eq!(out[1].hash, compute_light_hash(&[990, 1990, 300, 1010, 2010, 360, 2], 0));
        assert_ne!(out[0].hash, out[1].hash);
    }

    #[test]
    fn a_capsule_packs_its_extent_and_a_light_without_corona_size_has_no_corona() {
        let mut l = light(Vec3::ZERO, v(0.0, 0.0, -1.0));
        l.light_type = 4;
        l.extent = v(50.0, 0.0, 0.0);
        l.corona_size = 0.0;
        let model = ModelLights { lights: vec![l], ..ModelLights::default() };
        let p = Placement::new(Vec3::ZERO, [0.0, 0.0, 0.0, 1.0], 1.0, 1.0, false);
        let out = entity_lights(&p, &ArchLights::default(), &model);
        assert_eq!(out[0].cone_outer_angle_or_cap_ext, 91);
        assert_eq!(out[0].corona_intensity, 0);
        assert_eq!(out[0].time_and_state_flags >> 26, 4);
    }

    #[test]
    fn a_bone_carries_its_light() {
        let bones = [BonePose { tag: 3, parent: -1, rotation: Vec4::new(Z90[0], Z90[1], Z90[2], Z90[3]), translation: v(0.0, 0.0, 4.0), scale: v(1.0, 1.0, 1.0) }];
        let mut l = light(v(1.0, 0.0, 0.0), v(1.0, 0.0, 0.0));
        l.bone_id = 3;
        let model = ModelLights { lights: vec![l], bones: bone_transforms(&bones), ..ModelLights::default() };
        let p = Placement::new(v(10.0, 0.0, 0.0), [0.0, 0.0, 0.0, 1.0], 1.0, 1.0, false);
        let out = entity_lights(&p, &ArchLights::default(), &model);
        assert!(near(out[0].position, v(10.0, 1.0, 4.0)), "{:?}", out[0].position);
        assert!(near(out[0].direction, v(0.0, 1.0, 0.0)));
    }

    #[test]
    fn the_box_hash_sees_the_scaled_and_turned_entity_box() {
        let p = Placement::new(v(10.0, 0.0, 0.0), Z90, 2.0, 1.0, false);
        let (lo, hi) = transform_box(v(-1.0, -2.0, 0.0), v(1.0, 2.0, 3.0), &p);
        // X extent 2 (scaled) turns into Y, Y extent 4 into X.
        assert!(near(lo, v(6.0, -2.0, 0.0)), "{lo:?}");
        assert!(near(hi, v(14.0, 2.0, 3.0)), "{hi:?}");
        assert_eq!(as_u32(-12.5), 0xFFFF_FFF4);
        assert_eq!(round_byte(2.5), 2);
        assert_eq!(round_byte(3.5), 4);
        assert_eq!(round_byte(300.0), 44);
    }

    #[test]
    fn the_maps_headers_follow_calc_flags_and_calc_extents() {
        let mut lights = vec![
            LodLight { position: v(0.0, 0.0, 0.0), colour: 1, direction: v(0.0, 0.0, -1.0), falloff: 1.0, falloff_exponent: 1.0, time_and_state_flags: 0, hash: 9, cone_inner_angle: 0, cone_outer_angle_or_cap_ext: 0, corona_intensity: 0, is_street_light: false },
            LodLight { position: v(10.0, 20.0, 30.0), colour: 2, direction: v(0.0, 0.0, -1.0), falloff: 1.0, falloff_exponent: 1.0, time_and_state_flags: 0, hash: 4, cone_inner_angle: 0, cone_outer_angle_or_cap_ext: 0, corona_intensity: 0, is_street_light: false },
        ];
        sort_lights(&mut lights);
        assert_eq!((lights[0].hash, lights[1].hash), (4, 9));
        let (lod, dist) = headers("park", &lights);
        assert_eq!((lod.name.as_str(), lod.parent.as_str(), lod.flags, lod.content_flags), ("park_lodlights", "park_distantlights", 0, 128));
        assert_eq!((dist.name.as_str(), dist.parent.as_str(), dist.flags, dist.content_flags), ("park_distantlights", "", 2, 256));
        assert_eq!(lod.entities_extents, (v(-20.0, -20.0, -20.0), v(30.0, 40.0, 50.0)));
        assert_eq!(lod.streaming_extents, (v(-950.0, -950.0, -950.0), v(960.0, 970.0, 980.0)));
        assert_eq!(dist.entities_extents, lod.entities_extents);
        assert_eq!(dist.streaming_extents, (v(-3000.0, -3000.0, -3000.0), v(3010.0, 3020.0, 3030.0)));
    }
}
