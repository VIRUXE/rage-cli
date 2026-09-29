//! `rage ymap from-menyoo`: a map made from a Menyoo spooner XML, the way
//! CodeWalker's "Import Menyoo XML" fills one in — each placed prop becomes
//! an entity, each vehicle a car generator, and peds are left out (a map
//! has no place for them).

use anyhow::{bail, Context, Result};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rage_formats::{build_meta, dump_meta, from_xml, rage_joaat, MetaValue, NameTable, Schema, Vec3};

use crate::rpf::GtaKeys;

#[derive(clap::Args)]
pub struct YmapArgs {
    #[command(subcommand)]
    pub command: YmapCommand,
}

#[derive(clap::Subcommand)]
pub enum YmapCommand {
    /// Make a .ymap from a Menyoo spooner XML: props become entities,
    /// vehicles car generators
    FromMenyoo(FromMenyooArgs),
    /// Generate the LOD lights of a resource's maps, as CodeWalker's
    /// project "LOD lights generator" does: every light of every entity's
    /// model, written as NAME_lodlights.ymap and NAME_distantlights.ymap
    Lodlights(super::ymap_lodlights::LodlightsArgs),
}

#[derive(clap::Args)]
pub struct FromMenyooArgs {
    /// The Menyoo XML (a <SpoonerPlacements> file)
    #[arg(value_name = "XML")]
    pub input: PathBuf,

    /// The .ymap to write
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,

    /// The map's own name (CMapData.name); default: the output's file name
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// lodDist of every entity; default: the placement's own, capped at
    /// 10000 as CodeWalker does (Menyoo saves 16960 for most props, which
    /// makes the map stream in from anywhere)
    #[arg(long, value_name = "N")]
    pub lod_dist: Option<f32>,

    /// Write the flags, contentFlags and extents CodeWalker starts a new map
    /// with, instead of working them out from what the map holds
    #[arg(long)]
    pub no_recalc: bool,

    /// .ytyp files (or folders of them) declaring the props, for the
    /// extents; default: every .ytyp in the output's resource folder, then
    /// the game's own through the index (--exe / GTAV_PATH)
    #[arg(long, value_name = "PATH")]
    pub ytyp: Vec<PathBuf>,
}

pub fn run(args: &YmapArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    match &args.command {
        YmapCommand::FromMenyoo(a) => run_from_menyoo(a, keys, exe),
        YmapCommand::Lodlights(a) => super::ymap_lodlights::run(a, keys, exe),
    }
}

/// One `<Placement>` of a spooner file, with what the import uses of it.
#[derive(Debug, Clone, PartialEq)]
struct Placement {
    model_hash: u32,
    /// 1 ped, 2 vehicle, 3 object.
    kind: i32,
    dynamic: bool,
    hash_name: String,
    lod_distance: f32,
    position: [f32; 3],
    /// Degrees, as Menyoo stores them.
    pitch: f32,
    roll: f32,
    yaw: f32,
    attached: bool,
    texture_variation: Option<u32>,
    livery: Option<i8>,
}

fn run_from_menyoo(args: &FromMenyooArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    let bytes = std::fs::read(&args.input).with_context(|| format!("failed to read '{}'", args.input.display()))?;
    let placements = parse_spooner(&bytes).with_context(|| format!("'{}'", args.input.display()))?;
    let names = crate::names::load(&[], Some(&args.input))?;

    let name = match &args.name {
        Some(n) => n.clone(),
        None => args.output.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
    };
    let (xml, counts) = map_xml(&name, &placements, args.lod_dist, &names);
    let mut value = from_xml(&xml).context("building the map")?;

    if !args.no_recalc
        && let MetaValue::Struct(map) = &mut value
    {
        crate::commands::resource::recalc_map(map, &args.output, &args.ytyp, keys, exe)?;
    }

    let written = build_meta(&value, Schema::builtin())?;
    for warning in &written.warnings {
        eprintln!("warning: {warning}");
    }
    dump_meta(&written.bytes).context("the written file does not read back")?;
    if let Some(parent) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&args.output, &written.bytes).with_context(|| format!("writing {}", args.output.display()))?;

    if counts.attached > 0 {
        eprintln!("note: {} placement(s) were attached to another; they are placed where Menyoo last saw them, unattached", counts.attached);
    }
    let mut summary = format!("Wrote {} ({} entities, {} car generators", args.output.display(), counts.entities, counts.cars);
    if counts.peds > 0 {
        write!(summary, "; {} peds left out", counts.peds).unwrap();
    }
    if counts.other > 0 {
        write!(summary, "; {} placements of unknown type left out", counts.other).unwrap();
    }
    eprintln!("{summary})");
    Ok(())
}

/// The placements of a spooner file, in file order.
fn parse_spooner(bytes: &[u8]) -> Result<Vec<Placement>> {
    // Menyoo declares ISO-8859-1; the tags and numbers are ASCII either way,
    // and a stray Latin-1 byte in a note must not stop the import.
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t.to_owned(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    };
    let text = text.trim_start_matches('\u{feff}');
    let opts = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(text, opts).context("not well-formed XML")?;
    let root = doc.root_element();
    if root.tag_name().name() != "SpoonerPlacements" {
        bail!("not a Menyoo spooner file (the root is <{}>, not <SpoonerPlacements>)", root.tag_name().name());
    }
    root.children().filter(|n| n.has_tag_name("Placement")).map(placement).collect()
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, tag: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|n| n.has_tag_name(tag))
}

fn text_of(node: roxmltree::Node, tag: &str) -> String {
    child(node, tag).and_then(|n| n.text()).unwrap_or("").trim().to_owned()
}

// Read the way CodeWalker does: a value that does not parse is zero/false.
fn int_of(node: roxmltree::Node, tag: &str) -> i32 {
    text_of(node, tag).parse().unwrap_or(0)
}

fn float_of(node: roxmltree::Node, tag: &str) -> f32 {
    text_of(node, tag).parse().unwrap_or(0.0)
}

fn bool_of(node: roxmltree::Node, tag: &str) -> bool {
    text_of(node, tag).eq_ignore_ascii_case("true")
}

fn placement(node: roxmltree::Node) -> Result<Placement> {
    let hash_text = text_of(node, "ModelHash").to_lowercase();
    let hex = hash_text.strip_prefix("0x").unwrap_or(&hash_text);
    let model_hash = u32::from_str_radix(hex, 16).with_context(|| format!("a placement's ModelHash '{hash_text}' is not a hex number"))?;
    let prop = |group: &str, name: &str| child(node, group).map(|g| text_of(g, name)).filter(|s| !s.is_empty());
    let pr = child(node, "PositionRotation");
    let f = |tag: &str| pr.map_or(0.0, |p| float_of(p, tag));
    Ok(Placement {
        model_hash,
        kind: int_of(node, "Type"),
        dynamic: bool_of(node, "Dynamic"),
        hash_name: text_of(node, "HashName"),
        lod_distance: float_of(node, "LodDistance"),
        position: [f("X"), f("Y"), f("Z")],
        pitch: f("Pitch"),
        roll: f("Roll"),
        yaw: f("Yaw"),
        attached: child(node, "Attachment").and_then(|a| a.attribute("isAttached")).is_some_and(|v| v.eq_ignore_ascii_case("true")),
        texture_variation: prop("ObjectProperties", "TextureVariation").and_then(|v| v.parse().ok()),
        livery: prop("VehicleProperties", "Livery").and_then(|v| v.parse().ok()),
    })
}

/// The entity rotation of a placement: CodeWalker's
/// `Quaternion.RotationYawPitchRoll` of the negated angles (SharpDX takes
/// yaw about Y, pitch about X, roll about Z, so Menyoo's roll goes in as
/// its yaw and Menyoo's yaw as its roll). A map stores the inverse
/// rotation, which the negation gives.
fn rotation(p: &Placement) -> [f32; 4] {
    let k = -(std::f64::consts::PI / 180.0);
    let (yaw, pitch, roll) = (p.roll as f64 * k, p.pitch as f64 * k, p.yaw as f64 * k);
    let (sr, cr) = (roll * 0.5).sin_cos();
    let (sp, cp) = (pitch * 0.5).sin_cos();
    let (sy, cy) = (yaw * 0.5).sin_cos();
    [
        (cy * sp * cr + sy * cp * sr) as f32,
        (sy * cp * cr - cy * sp * sr) as f32,
        (cy * cp * sr - sy * sp * cr) as f32,
        (cy * cp * cr + sy * sp * sr) as f32,
    ]
}

/// `v` rotated by the unit quaternion `q`.
fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let [x, y, z, w] = q;
    let cross = |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let t = cross([x, y, z], v).map(|c| 2.0 * c);
    let u = cross([x, y, z], t);
    [v[0] + w * t[0] + u[0], v[1] + w * t[1] + u[1], v[2] + w * t[2] + u[2]]
}

#[derive(Debug, Default, PartialEq)]
struct Counts {
    entities: usize,
    cars: usize,
    peds: usize,
    other: usize,
    attached: usize,
}

/// A placement's model by name: its HashName when that is what the hash
/// is of, else a name from the name lists, else the `hash_` placeholder.
fn model_name(p: &Placement, names: &NameTable) -> String {
    if !p.hash_name.is_empty() && rage_joaat(&p.hash_name.to_lowercase()) == p.model_hash {
        return p.hash_name.clone();
    }
    names.resolve(p.model_hash).into_owned()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn entity_xml(p: &Placement, lod_dist: Option<f32>, names: &NameTable) -> String {
    let [x, y, z] = p.position;
    let [qx, qy, qz, qw] = rotation(p);
    // CodeWalker caps the draw distance: Menyoo saves 16960 for "always".
    let lod = lod_dist.unwrap_or(if p.lod_distance < 10000.0 { p.lod_distance } else { 10000.0 });
    format!(
        "<Item type=\"CEntityDef\"><archetypeName>{}</archetypeName><flags value=\"{}\"/><guid value=\"0\"/>\
         <position x=\"{x}\" y=\"{y}\" z=\"{z}\"/><rotation x=\"{qx}\" y=\"{qy}\" z=\"{qz}\" w=\"{qw}\"/>\
         <scaleXY value=\"1\"/><scaleZ value=\"1\"/><parentIndex value=\"-1\"/><lodDist value=\"{lod}\"/><childLodDist value=\"0\"/>\
         <lodLevel>LODTYPES_DEPTH_ORPHANHD</lodLevel><numChildren value=\"0\"/><priorityLevel>PRI_REQUIRED</priorityLevel><extensions/>\
         <ambientOcclusionMultiplier value=\"255\"/><artificialAmbientOcclusion value=\"255\"/><tintValue value=\"{}\"/></Item>",
        esc(&model_name(p, names)),
        if p.dynamic { 0 } else { 32 },
        p.texture_variation.unwrap_or(0),
    )
}

/// A car generator facing the way the vehicle was placed, with
/// CodeWalker's defaults (flags 3680, a 2.6 m half-width, random colours).
fn car_gen_xml(p: &Placement, names: &NameTable) -> String {
    let [x, y, z] = p.position;
    let [qx, qy, qz, qw] = rotation(p);
    let dir = rotate([-qx, -qy, -qz, qw], [0.0, 5.0, 0.0]);
    format!(
        "<Item><position x=\"{x}\" y=\"{y}\" z=\"{z}\"/><orientX value=\"{}\"/><orientY value=\"{}\"/>\
         <perpendicularLength value=\"2.6\"/><carModel>{}</carModel><flags value=\"3680\"/>\
         <bodyColorRemap1 value=\"-1\"/><bodyColorRemap2 value=\"-1\"/><bodyColorRemap3 value=\"-1\"/><bodyColorRemap4 value=\"-1\"/>\
         <popGroup/><livery value=\"{}\"/></Item>",
        dir[0],
        dir[1],
        esc(&model_name(p, names)),
        p.livery.unwrap_or(0),
    )
}

/// The map as XML in the layout `resource dump` writes, with CodeWalker's
/// new-map header (contentFlags 65, extents left for the recalculation).
fn map_xml(name: &str, placements: &[Placement], lod_dist: Option<f32>, names: &NameTable) -> (String, Counts) {
    let mut counts = Counts::default();
    let (mut entities, mut cars) = (String::new(), String::new());
    for p in placements {
        match p.kind {
            1 => counts.peds += 1,
            2 => {
                cars.push_str(&car_gen_xml(p, names));
                counts.cars += 1;
            }
            3 => {
                entities.push_str(&entity_xml(p, lod_dist, names));
                counts.entities += 1;
            }
            _ => counts.other += 1,
        }
        if p.attached && matches!(p.kind, 2 | 3) {
            counts.attached += 1;
        }
    }
    (map_document(name, "", 0, 65, None, &entities, &cars), counts)
}

/// A `CMapData` document in the layout `resource dump` writes, with every
/// list empty but the `entities` and `carGenerators` given as XML, and
/// zero extents when `extents` (entities, streaming) is `None`.
pub(crate) fn map_document(name: &str, parent: &str, flags: u32, content_flags: u32, extents: Option<((Vec3, Vec3), (Vec3, Vec3))>, entities: &str, cars: &str) -> String {
    let name = esc(name);
    let parent = esc(parent);
    let ((emin, emax), (smin, smax)) = extents.unwrap_or_default();
    let xyz = |tag: &str, v: Vec3| format!("<{tag} x=\"{}\" y=\"{}\" z=\"{}\"/>", v.x, v.y, v.z);
    format!(
        "<CMapData><name>{name}</name><parent>{parent}</parent><flags value=\"{flags}\"/><contentFlags value=\"{content_flags}\"/>\
         {}{}{}{}\
         <entities>{entities}</entities>\
         <containerLods itemType=\"rage__fwContainerLodDef\"/><boxOccluders itemType=\"BoxOccluder\"/><occludeModels itemType=\"OccludeModel\"/>\
         <physicsDictionaries/><instancedData><ImapLink/><PropInstanceList itemType=\"rage__fwPropInstanceListDef\"/>\
         <GrassInstanceList itemType=\"rage__fwGrassInstanceListDef\"/></instancedData>\
         <timeCycleModifiers itemType=\"CTimeCycleModifier\"/><carGenerators itemType=\"CCarGen\">{cars}</carGenerators>\
         <LODLightsSOA><direction/><falloff/><falloffExponent/><timeAndStateFlags/><hash/><coneInnerAngle/><coneOuterAngleOrCapExt/><coronaIntensity/></LODLightsSOA>\
         <DistantLODLightsSOA><position/><RGBI/><numStreetLights value=\"0\"/><category value=\"0\"/></DistantLODLightsSOA>\
         <block><version value=\"0\"/><flags value=\"0\"/><name>{name}</name><exportedBy>rage-cli</exportedBy><owner/><time>{}</time></block></CMapData>",
        xyz("streamingExtentsMin", smin),
        xyz("streamingExtentsMax", smax),
        xyz("entitiesExtentsMin", emin),
        xyz("entitiesExtentsMax", emax),
        crate::names::today(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPOONER: &str = r#"<?xml version="1.0" encoding="ISO-8859-1"?>
<SpoonerPlacements>
  <Note />
  <ReferenceCoords><X>-180.6</X><Y>100.8</Y><Z>100.0</Z></ReferenceCoords>
  <Placement>
    <ModelHash>0x6BA514AC</ModelHash>
    <Type>3</Type>
    <Dynamic>false</Dynamic>
    <HashName>prop_bench_01a</HashName>
    <ObjectProperties><TextureVariation>2</TextureVariation></ObjectProperties>
    <LodDistance>16960</LodDistance>
    <PositionRotation><X>10</X><Y>20</Y><Z>30</Z><Pitch>0</Pitch><Roll>0</Roll><Yaw>90</Yaw></PositionRotation>
    <Attachment isAttached="false" />
  </Placement>
  <Placement>
    <ModelHash>0xB779A091</ModelHash>
    <Type>2</Type>
    <Dynamic>true</Dynamic>
    <HashName>adder</HashName>
    <VehicleProperties><Livery>3</Livery></VehicleProperties>
    <LodDistance>200</LodDistance>
    <PositionRotation><X>1</X><Y>2</Y><Z>3</Z><Pitch>0</Pitch><Roll>0</Roll><Yaw>90</Yaw></PositionRotation>
    <Attachment isAttached="false" />
  </Placement>
  <Placement>
    <ModelHash>0x12345678</ModelHash>
    <Type>1</Type>
    <PositionRotation><X>0</X><Y>0</Y><Z>0</Z><Pitch>0</Pitch><Roll>0</Roll><Yaw>0</Yaw></PositionRotation>
  </Placement>
</SpoonerPlacements>"#;

    fn build(placements: &[Placement], lod_dist: Option<f32>) -> (Vec<u8>, Counts) {
        let (xml, counts) = map_xml("test_map", placements, lod_dist, &NameTable::default());
        let written = build_meta(&from_xml(&xml).unwrap(), Schema::builtin()).unwrap();
        assert!(written.warnings.is_empty(), "{:?}", written.warnings);
        (written.bytes, counts)
    }

    #[test]
    fn reads_placements() {
        let p = parse_spooner(SPOONER.as_bytes()).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].model_hash, rage_joaat("prop_bench_01a"));
        assert_eq!(p[0].texture_variation, Some(2));
        assert_eq!(p[0].yaw, 90.0);
        assert!(p[1].dynamic);
        assert_eq!(p[1].livery, Some(3));
        assert_eq!(p[2].kind, 1);
    }

    #[test]
    fn latin1_is_not_an_error() {
        let mut bytes = SPOONER.replace("<Note />", "<Note>caf\u{0}</Note>").into_bytes();
        let at = bytes.iter().position(|&b| b == 0).unwrap();
        bytes[at] = 0xE9;
        assert_eq!(parse_spooner(&bytes).unwrap().len(), 3);
    }

    #[test]
    fn rotation_matches_codewalker() {
        // A yaw of 90 degrees, as CodeWalker stores it: z = -sin(45), w = cos(45).
        let p = &parse_spooner(SPOONER.as_bytes()).unwrap()[0];
        let [x, y, z, w] = rotation(p);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!(x.abs() < 1e-6 && y.abs() < 1e-6, "{x} {y}");
        assert!((z + h).abs() < 1e-6 && (w - h).abs() < 1e-6, "{z} {w}");
    }

    #[test]
    fn writes_entities_and_car_generators() {
        let (bytes, counts) = build(&parse_spooner(SPOONER.as_bytes()).unwrap(), None);
        assert_eq!(counts, Counts { entities: 1, cars: 1, peds: 1, other: 0, attached: 0 });
        let map = rage_formats::parse_ymap(&bytes).unwrap();
        assert_eq!(map.entities.len(), 1);
        let e = &map.entities[0];
        assert_eq!(e.archetype_hash, rage_joaat("prop_bench_01a"));
        assert_eq!(e.flags, 32);
        assert_eq!(e.lod_dist, 10000.0);
        assert_eq!(e.position, rage_formats::Vec3::new(10.0, 20.0, 30.0));
        assert!((crate::commands::resource::entity_yaw_degrees(e) - 90.0).abs() < 1e-3);

        let root = dump_meta(&bytes).unwrap().root;
        let cars = root.as_struct().unwrap().field("carGenerators").and_then(MetaValue::as_array).unwrap();
        assert_eq!(cars.items.len(), 1);
        let c = cars.items[0].as_struct().unwrap();
        let f = |n: &str| c.field(n).and_then(MetaValue::as_f32).unwrap();
        assert_eq!(c.field("carModel").and_then(MetaValue::as_hash), Some(rage_joaat("adder")));
        assert_eq!(c.field("flags").and_then(MetaValue::as_u32), Some(3680));
        assert_eq!(c.field("livery").and_then(MetaValue::as_i64), Some(3));
        // Facing +90 degrees (west): forward (0, 5) turns to (-5, 0).
        assert!((f("orientX") + 5.0).abs() < 1e-4 && f("orientY").abs() < 1e-4, "{} {}", f("orientX"), f("orientY"));
        assert_eq!(f("perpendicularLength"), 2.6);
    }

    #[test]
    fn lod_dist_overrides_the_placements() {
        let (bytes, _) = build(&parse_spooner(SPOONER.as_bytes()).unwrap(), Some(150.0));
        assert_eq!(rage_formats::parse_ymap(&bytes).unwrap().entities[0].lod_dist, 150.0);
    }

    #[test]
    fn a_wrong_hash_name_falls_back_to_the_hash() {
        let mut p = parse_spooner(SPOONER.as_bytes()).unwrap().remove(0);
        p.hash_name = "not_it".into();
        assert_eq!(model_name(&p, &NameTable::default()), format!("hash_{:08X}", p.model_hash));
    }
}
