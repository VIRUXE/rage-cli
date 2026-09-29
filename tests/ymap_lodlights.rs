//! `rage ymap lodlights` over a resource the tool itself builds: a drawable with two lights from
//! the fixture XML, its type file from `ytyp from-drawables`, a map placing it twice. Needs no
//! game install.

use std::path::Path;
use std::process::{Command, Output};

use rage_formats::{dump_meta, MetaValue, Vec3};

fn rage(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .env("RAGE_NAMES", "nowhere/names.txt")
        .env_remove("GTAV_PATH")
        .output()
        .expect("failed to run the rage binary")
}

fn ok(args: &[&str]) -> (String, String) {
    let output = rage(args);
    assert!(
        output.status.success(),
        "`rage {}` failed with {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned())
}

fn fails(args: &[&str]) -> String {
    let output = rage(args);
    assert!(!output.status.success(), "`rage {}` unexpectedly succeeded", args.join(" "));
    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A spot light 3 up pointing down, and a point light beside it, as `Drawable.WriteXml` writes
/// them; the tags the reader does not use are left out.
const LIGHTS_XML: &str = r#"<Lights>
    <Item>
      <Position x="0" y="0" z="3" />
      <Colour r="255" g="200" b="100" />
      <Flashiness value="0" />
      <Intensity value="20" />
      <Flags value="0" />
      <BoneId value="0" />
      <Type>Spot</Type>
      <GroupId value="0" />
      <TimeFlags value="16777215" />
      <Falloff value="8" />
      <FalloffExponent value="32" />
      <CullingPlaneNormal x="0" y="0" z="1" />
      <CullingPlaneOffset value="0" />
      <VolumeIntensity value="0" />
      <VolumeSizeScale value="1" />
      <VolumeOuterColour r="255" g="255" b="255" />
      <LightHash value="0" />
      <VolumeOuterIntensity value="0" />
      <CoronaSize value="2" />
      <VolumeOuterExponent value="1" />
      <ShadowNearClip value="0.01" />
      <CoronaIntensity value="1.5" />
      <CoronaZBias value="0.1" />
      <Direction x="0" y="0" z="-1" />
      <Tangent x="1" y="0" z="0" />
      <ConeInnerAngle value="20" />
      <ConeOuterAngle value="60" />
      <Extent x="1" y="1" z="1" />
      <ProjectedTextureHash />
    </Item>
    <Item>
      <Position x="0.5" y="0" z="2.5" />
      <Colour r="10" g="20" b="30" />
      <Flashiness value="0" />
      <Intensity value="100" />
      <Flags value="0" />
      <BoneId value="0" />
      <Type>Point</Type>
      <GroupId value="0" />
      <TimeFlags value="255" />
      <Falloff value="4" />
      <FalloffExponent value="16" />
      <CullingPlaneNormal x="0" y="0" z="1" />
      <CullingPlaneOffset value="0" />
      <VolumeIntensity value="0" />
      <VolumeSizeScale value="1" />
      <VolumeOuterColour r="255" g="255" b="255" />
      <LightHash value="0" />
      <VolumeOuterIntensity value="0" />
      <CoronaSize value="0" />
      <VolumeOuterExponent value="1" />
      <ShadowNearClip value="0.01" />
      <CoronaIntensity value="3" />
      <CoronaZBias value="0.1" />
      <Direction x="1" y="0" z="0" />
      <Tangent x="0" y="1" z="0" />
      <ConeInnerAngle value="0" />
      <ConeOuterAngle value="0" />
      <Extent x="1" y="1" z="1" />
      <ProjectedTextureHash />
    </Item>
  </Lights>"#;

/// A map placing the lamp twice: once as stored, once turned by a stored rotation of 90 degrees
/// about Z (which CodeWalker inverts: local +X becomes world -Y).
const MAP_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<CMapData>
  <name>park</name>
  <parent/>
  <flags value="0"/>
  <contentFlags value="1"/>
  <streamingExtentsMin x="0" y="0" z="0"/>
  <streamingExtentsMax x="0" y="0" z="0"/>
  <entitiesExtentsMin x="0" y="0" z="0"/>
  <entitiesExtentsMax x="0" y="0" z="0"/>
  <entities>
    <Item type="CEntityDef">
      <archetypeName>lamp_prop</archetypeName>
      <flags value="32"/>
      <guid value="1"/>
      <position x="100" y="200" z="30"/>
      <rotation x="0" y="0" z="0" w="1"/>
      <scaleXY value="1"/>
      <scaleZ value="1"/>
      <parentIndex value="-1"/>
      <lodDist value="120"/>
      <childLodDist value="0"/>
      <lodLevel>LODTYPES_DEPTH_ORPHANHD</lodLevel>
      <numChildren value="0"/>
      <priorityLevel>PRI_REQUIRED</priorityLevel>
      <extensions/>
      <ambientOcclusionMultiplier value="255"/>
      <artificialAmbientOcclusion value="255"/>
      <tintValue value="0"/>
    </Item>
    <Item type="CEntityDef">
      <archetypeName>lamp_prop</archetypeName>
      <flags value="32"/>
      <guid value="2"/>
      <position x="0" y="0" z="0"/>
      <rotation x="0" y="0" z="0.70710678" w="0.70710678"/>
      <scaleXY value="1"/>
      <scaleZ value="1"/>
      <parentIndex value="-1"/>
      <lodDist value="120"/>
      <childLodDist value="0"/>
      <lodLevel>LODTYPES_DEPTH_ORPHANHD</lodLevel>
      <numChildren value="0"/>
      <priorityLevel>PRI_REQUIRED</priorityLevel>
      <extensions/>
      <ambientOcclusionMultiplier value="255"/>
      <artificialAmbientOcclusion value="255"/>
      <tintValue value="0"/>
    </Item>
  </entities>
  <containerLods itemType="rage__fwContainerLodDef"/>
  <boxOccluders itemType="BoxOccluder"/>
  <occludeModels itemType="OccludeModel"/>
  <physicsDictionaries/>
  <instancedData>
    <ImapLink/>
    <PropInstanceList itemType="rage__fwPropInstanceListDef"/>
    <GrassInstanceList itemType="rage__fwGrassInstanceListDef"/>
  </instancedData>
  <timeCycleModifiers itemType="CTimeCycleModifier"/>
  <carGenerators itemType="CCarGen"/>
  <LODLightsSOA><direction/><falloff/><falloffExponent/><timeAndStateFlags/><hash/><coneInnerAngle/><coneOuterAngleOrCapExt/><coronaIntensity/></LODLightsSOA>
  <DistantLODLightsSOA><position/><RGBI/><numStreetLights value="0"/><category value="0"/></DistantLODLightsSOA>
  <block><version value="0"/><flags value="0"/><name>park</name><exportedBy/><owner/><time/></block>
</CMapData>"#;

/// The resource: `res/fxmanifest.lua`, `res/stream/lamp_prop.ydr`, `res/stream/lamp_prop.ytyp`
/// and `res/stream/park.ymap`.
fn build_resource(tmp: &Path) -> std::path::PathBuf {
    let resource = tmp.join("res");
    let stream = resource.join("stream");
    std::fs::create_dir_all(&stream).unwrap();
    std::fs::write(resource.join("fxmanifest.lua"), "fx_version 'cerulean'\n").unwrap();

    let fixture = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/one_triangle.ydr.xml")).unwrap();
    assert!(fixture.contains("<Lights />"), "the fixture drawable should end with an empty light list");
    let lamp_xml = tmp.join("lamp_prop.xml");
    std::fs::write(&lamp_xml, fixture.replace("<Lights />", LIGHTS_XML)).unwrap();
    ok(&["resource", "build", s(&lamp_xml), "-o", s(&stream.join("lamp_prop.ydr"))]);
    ok(&["ytyp", "from-drawables", s(&stream), "-o", s(&stream.join("lamp_prop.ytyp"))]);

    let map_xml = tmp.join("park.xml");
    std::fs::write(&map_xml, MAP_XML).unwrap();
    ok(&["resource", "build", "--no-recalc", s(&map_xml), "-o", s(&stream.join("park.ymap"))]);
    resource
}

fn vec3s(map: &rage_formats::MetaStruct, soa: &str, member: &str) -> Vec<Vec3> {
    map.field(soa).and_then(MetaValue::as_struct).unwrap().field(member).unwrap().items().iter().map(|v| v.as_vec3().unwrap()).collect()
}

fn ints(map: &rage_formats::MetaStruct, soa: &str, member: &str) -> Vec<i64> {
    map.field(soa).and_then(MetaValue::as_struct).unwrap().field(member).unwrap().items().iter().map(|v| v.as_i64().unwrap()).collect()
}

fn near(a: Vec3, b: [f32; 3]) -> bool {
    (a.x - b[0]).abs() < 1e-4 && (a.y - b[1]).abs() < 1e-4 && (a.z - b[2]).abs() < 1e-4
}

#[test]
fn the_two_maps_hold_every_light_of_every_placed_entity() {
    let tmp = tempfile::tempdir().unwrap();
    let resource = build_resource(tmp.path());
    let stream = resource.join("stream");

    // Named after the resource folder, written beside the maps.
    let (_, stderr) = ok(&["ymap", "lodlights", s(&stream)]);
    assert!(stderr.contains("4 lights from 2 entities in 1 map(s)"), "{stderr}");
    assert!(!stderr.contains("warning"), "{stderr}");
    let lod_path = stream.join("res_lodlights.ymap");
    let dist_path = stream.join("res_distantlights.ymap");

    let (info, _) = ok(&["resource", "info", s(&lod_path)]);
    assert!(info.contains("Map:       res_lodlights  parent res_distantlights"), "{info}");
    assert!(info.contains("content 0x80 LOD lights"), "{info}");
    let (info, _) = ok(&["resource", "info", s(&dist_path)]);
    assert!(info.contains("content 0x100 Distant lights"), "{info}");
    assert!(info.contains("0x2 LOD"), "{info}");

    let lod = dump_meta(&std::fs::read(&lod_path).unwrap()).unwrap();
    let dist = dump_meta(&std::fs::read(&dist_path).unwrap()).unwrap();
    assert!(lod.warnings.is_empty() && dist.warnings.is_empty(), "{:?} {:?}", lod.warnings, dist.warnings);
    let (lod, dist) = (lod.root.as_struct().unwrap(), dist.root.as_struct().unwrap());

    // The rows are sorted by hash, so match them by position.
    let positions = vec3s(dist, "DistantLODLightsSOA", "position");
    let directions = vec3s(lod, "LODLightsSOA", "direction");
    let colours = ints(dist, "DistantLODLightsSOA", "RGBI");
    let flags = ints(lod, "LODLightsSOA", "timeAndStateFlags");
    let inner = ints(lod, "LODLightsSOA", "coneInnerAngle");
    let outer = ints(lod, "LODLightsSOA", "coneOuterAngleOrCapExt");
    let corona = ints(lod, "LODLightsSOA", "coronaIntensity");
    let hashes = ints(lod, "LODLightsSOA", "hash");
    assert_eq!(positions.len(), 4);
    assert_eq!((directions.len(), colours.len(), flags.len(), hashes.len()), (4, 4, 4, 4));
    assert!(hashes.windows(2).all(|w| w[0] <= w[1]), "{hashes:?}");
    assert_eq!(hashes.iter().collect::<std::collections::HashSet<_>>().len(), 4, "every light has its own hash");

    let spot_colour = (106 << 24) | (255 << 16) | (200 << 8) | 100; // intensity 20 * 5.3125 = 106.25
    let point_colour = (255 << 24) | (10 << 16) | (20 << 8) | 30; // intensity 100 * 5.3125 clamps to 255
    let expect = [
        // (position, direction, colour, time and state flags, inner, outer, corona)
        ([100.0, 200.0, 33.0], [0.0, 0.0, -1.0], spot_colour, 16777215 | (2 << 26), 28, 85, 9),
        ([100.5, 200.0, 32.5], [1.0, 0.0, 0.0], point_colour, 255 | (1 << 26), 0, 0, 0),
        ([0.0, 0.0, 3.0], [0.0, 0.0, -1.0], spot_colour, 16777215 | (2 << 26), 28, 85, 9),
        ([0.0, -0.5, 2.5], [0.0, -1.0, 0.0], point_colour, 255 | (1 << 26), 0, 0, 0),
    ];
    for (pos, dir, colour, t, ci, co, cr) in expect {
        let i = positions.iter().position(|p| near(*p, pos)).unwrap_or_else(|| panic!("no light at {pos:?} among {positions:?}"));
        assert!(near(directions[i], dir), "{:?} at {pos:?}", directions[i]);
        assert_eq!(colours[i], colour, "colour at {pos:?}");
        assert_eq!(flags[i], t, "flags at {pos:?}");
        assert_eq!((inner[i], outer[i], corona[i]), (ci, co, cr), "angles at {pos:?}");
    }
    assert_eq!(ints(dist, "DistantLODLightsSOA", "category"), Vec::<i64>::new(), "category is a scalar");
    let category = dist.field("DistantLODLightsSOA").and_then(MetaValue::as_struct).unwrap().field("category").and_then(MetaValue::as_i64);
    assert_eq!(category, Some(1));
    // The distant map's box: the lights (0, -0.5, 2.5)..(100.5, 200, 33), grown by 20 and 3000.
    let header = rage_formats::parse_ymap_header(&std::fs::read(&dist_path).unwrap()).unwrap();
    assert!(near(header.entities_extents_min, [-20.0, -20.5, -17.5]), "{:?}", header.entities_extents_min);
    assert!(near(header.entities_extents_max, [120.5, 220.0, 53.0]), "{:?}", header.entities_extents_max);
    assert!(near(header.streaming_extents_min, [-3000.0, -3000.5, -2997.5]), "{:?}", header.streaming_extents_min);
    let header = rage_formats::parse_ymap_header(&std::fs::read(&lod_path).unwrap()).unwrap();
    assert!(near(header.streaming_extents_max, [1050.5, 1150.0, 983.0]), "{:?}", header.streaming_extents_max);

    // A second run over the folder sees the two new maps too (they place nothing) and gives
    // the same result under another name, in another folder.
    let out = tmp.path().join("out");
    let (_, stderr) = ok(&["ymap", "lodlights", s(&stream), "-o", s(&out), "--name", "night"]);
    assert!(stderr.contains("4 lights from 2 entities in 3 map(s)"), "{stderr}");
    assert!(out.join("night_lodlights.ymap").is_file() && out.join("night_distantlights.ymap").is_file());
}

#[test]
fn a_map_whose_models_are_missing_is_reported_and_nothing_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let resource = build_resource(tmp.path());
    let stream = resource.join("stream");
    std::fs::remove_file(stream.join("lamp_prop.ydr")).unwrap();
    let err = fails(&["ymap", "lodlights", s(&stream.join("park.ymap"))]);
    assert!(err.contains("no model for 1 archetypes: lamp_prop"), "{err}");
    assert!(err.contains("no lights found"), "{err}");
    assert!(!stream.join("res_lodlights.ymap").exists());

    // Without the type file the archetype itself is unknown (and, with no file of that name
    // left beside the map, its name too).
    std::fs::remove_file(stream.join("lamp_prop.ytyp")).unwrap();
    let err = fails(&["ymap", "lodlights", s(&stream)]);
    assert!(err.contains("no archetype for 1 entities' models: hash_31DFF00B"), "{err}");
    assert!(err.contains("pass --ytyp"), "{err}");
}
