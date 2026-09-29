//! `rage ymap lodlights` against CodeWalker.Core over retail models: a street lamp fragment
//! (`prop_streetlight_01.yft`, lights on the fragment, an audio-emitter extension on the
//! archetype) and a lamp drawable (`prop_ind_light_01a.ydr`), placed turned and scaled in a map
//! the test writes, plus a drawable-dictionary member with no lights.
//!
//! Skipped unless `GTAV_PATH` points at the game. With it set, the oracle is required:
//! `../codewalker-cli/bin/Release/codewalker-cli.exe` and a CodeWalker.Core built from the source
//! the port follows (`CODEWALKER_CORE_DIR`, or `../codewalker-cli/core/CodeWalker.Core.dll` from
//! `build-core.ps1`). Its `lodlights` command runs `YmapEntityDef.EnsureLights` and the packing
//! of `GenerateLODLightsPanel` over the same loose files, and every light must agree: hash,
//! position, direction, colour, flags, angles, corona and falloff.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rage_formats::{dump_meta, MetaValue, Vec3};

fn run(cmd: &mut Command) -> Output {
    let output = cmd.output().unwrap_or_else(|e| panic!("failed to run {cmd:?}: {e}"));
    assert!(output.status.success(), "{cmd:?} failed with {}\nstdout:\n{}\nstderr:\n{}", output.status, String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    output
}

fn rage(args: &[&str]) -> String {
    let output = run(Command::new(env!("CARGO_BIN_EXE_rage")).args(args).env("RAGE_NO_UPDATE_CHECK", "1").env("RAGE_NAMES", "nowhere/names.txt"));
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            return Some(path);
        }
    }
    None
}

fn sibling_tool() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("codewalker-cli")
}

fn oracle() -> Option<PathBuf> {
    let exe = sibling_tool().join("bin").join("Release").join("codewalker-cli.exe");
    exe.is_file().then_some(exe)
}

fn source_core() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEWALKER_CORE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let dir = sibling_tool().join("core");
    dir.join("CodeWalker.Core.dll").is_file().then_some(dir)
}

/// One light as both sides report it.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    position: Vec3,
    direction: Vec3,
    colour: u32,
    flags: u32,
    inner: u8,
    outer: u8,
    corona: u8,
    falloff: f32,
    falloff_exponent: f32,
}

fn map_xml(entities: &str) -> String {
    format!(
        r#"<CMapData><name>smoke</name><parent/><flags value="0"/><contentFlags value="1"/>
<streamingExtentsMin x="0" y="0" z="0"/><streamingExtentsMax x="0" y="0" z="0"/><entitiesExtentsMin x="0" y="0" z="0"/><entitiesExtentsMax x="0" y="0" z="0"/>
<entities>{entities}</entities>
<containerLods itemType="rage__fwContainerLodDef"/><boxOccluders itemType="BoxOccluder"/><occludeModels itemType="OccludeModel"/><physicsDictionaries/>
<instancedData><ImapLink/><PropInstanceList itemType="rage__fwPropInstanceListDef"/><GrassInstanceList itemType="rage__fwGrassInstanceListDef"/></instancedData>
<timeCycleModifiers itemType="CTimeCycleModifier"/><carGenerators itemType="CCarGen"/>
<LODLightsSOA><direction/><falloff/><falloffExponent/><timeAndStateFlags/><hash/><coneInnerAngle/><coneOuterAngleOrCapExt/><coronaIntensity/></LODLightsSOA>
<DistantLODLightsSOA><position/><RGBI/><numStreetLights value="0"/><category value="0"/></DistantLODLightsSOA>
<block><version value="0"/><flags value="0"/><name>smoke</name><exportedBy/><owner/><time/></block></CMapData>"#
    )
}

fn entity(name: &str, pos: [f32; 3], rot: [f32; 4], sxy: f32, sz: f32) -> String {
    format!(
        r#"<Item type="CEntityDef"><archetypeName>{name}</archetypeName><flags value="32"/><guid value="0"/><position x="{}" y="{}" z="{}"/><rotation x="{}" y="{}" z="{}" w="{}"/><scaleXY value="{sxy}"/><scaleZ value="{sz}"/><parentIndex value="-1"/><lodDist value="150"/><childLodDist value="0"/><lodLevel>LODTYPES_DEPTH_ORPHANHD</lodLevel><numChildren value="0"/><priorityLevel>PRI_REQUIRED</priorityLevel><extensions/><ambientOcclusionMultiplier value="255"/><artificialAmbientOcclusion value="255"/><tintValue value="0"/></Item>"#,
        pos[0], pos[1], pos[2], rot[0], rot[1], rot[2], rot[3]
    )
}

/// A type file declaring `name` as a member of the dictionary `dict`.
fn dictionary_ytyp_xml(name: &str, dict: &str) -> String {
    format!(
        r#"<CMapTypes><extensions/><archetypes><Item type="CBaseArchetypeDef"><lodDist value="100"/><flags value="0"/><specialAttribute value="0"/><bbMin x="-1" y="-1" z="-1"/><bbMax x="1" y="1" z="1"/><bsCentre x="0" y="0" z="0"/><bsRadius value="2"/><hdTextureDist value="15"/><name>{name}</name><textureDictionary/><clipDictionary/><drawableDictionary>{dict}</drawableDictionary><physicsDictionary/><assetType>ASSET_TYPE_DRAWABLEDICTIONARY</assetType><assetName>{name}</assetName><extensions/></Item></archetypes><name>smoke_dict</name><dependencies/><compositeEntityTypes/></CMapTypes>"#
    )
}

#[test]
fn every_light_agrees_with_codewalker() {
    let Some(game) = std::env::var_os("GTAV_PATH").filter(|p| !p.is_empty()) else {
        eprintln!("GTAV_PATH is not set; skipping the retail LOD lights check");
        return;
    };
    let game = PathBuf::from(game);
    let exe = oracle().expect("the oracle: ../codewalker-cli/bin/Release/codewalker-cli.exe (dotnet build -c Release)");
    let core = source_core().expect("a CodeWalker.Core built from source: CODEWALKER_CORE_DIR or ../codewalker-cli/core (build-core.ps1)");

    let tmp = tempfile::tempdir().unwrap();
    let resource = tmp.path().join("res");
    let stream = resource.join("stream");
    std::fs::create_dir_all(&stream).unwrap();
    std::fs::write(resource.join("fxmanifest.lua"), "fx_version 'cerulean'\n").unwrap();

    // The lamps and their type file, out of the props archive; a minimap dictionary for the
    // dictionary path.
    let props = tmp.path().join("props");
    rage(&["extract", s(&game.join("x64h.rpf")), "levels/gta5/props/roadside/v_traffic_lights.rpf", "-o", s(&props)]);
    let inner = find_file(&props, "v_traffic_lights.rpf").expect("the traffic lights archive");
    rage(&["extract", s(&inner), "prop_streetlight_01.yft", "prop_ind_light_01a.ydr", "v_traffic_lights.ytyp", "-o", s(&stream)]);
    let minimap = tmp.path().join("minimap");
    rage(&["extract", s(&game.join("x64e.rpf")), "levels/gta5/minimap.rpf", "-o", s(&minimap)]);
    let inner = find_file(&minimap, "minimap.rpf").expect("the minimap archive");
    rage(&["extract", s(&inner), "minimap_0_2.ydd", "-o", s(&stream)]);
    for name in ["prop_streetlight_01.yft", "prop_ind_light_01a.ydr", "v_traffic_lights.ytyp", "minimap_0_2.ydd"] {
        assert!(stream.join(name).is_file(), "{name} was not extracted");
    }
    let member = rage_formats::parse_ydd(&std::fs::read(stream.join("minimap_0_2.ydd")).unwrap()).unwrap().into_iter().next().expect("a dictionary member");
    let member_name = format!("hash_{:08X}", member.hash);
    let ytyp_xml = tmp.path().join("dict.xml");
    std::fs::write(&ytyp_xml, dictionary_ytyp_xml(&member_name, "minimap_0_2")).unwrap();
    rage(&["resource", "build", s(&ytyp_xml), "-o", s(&stream.join("smoke_dict.ytyp"))]);

    // Yaw 30 degrees, an arbitrary unit rotation with scale, and the dictionary member.
    let entities = [
        entity("prop_streetlight_01", [2000.5, -1500.25, 30.75], [0.0, 0.0, -0.258819, 0.9659258], 1.0, 1.0),
        entity("prop_ind_light_01a", [2010.0, -1490.0, 35.0], [0.1, 0.2, 0.3, 0.9273618], 1.5, 2.0),
        entity("prop_streetlight_01", [-2000.0, 1500.0, -30.0], [0.0, 0.0, 0.0, 1.0], 1.0, 1.0),
        entity(&member_name, [2020.0, -1480.0, 31.0], [0.0, 0.0, 0.0, 1.0], 1.0, 1.0),
    ]
    .concat();
    let map_path = tmp.path().join("smoke.xml");
    std::fs::write(&map_path, map_xml(&entities)).unwrap();
    rage(&["resource", "build", "--no-recalc", s(&map_path), "-o", s(&stream.join("smoke.ymap"))]);

    let out = tmp.path().join("out");
    let stderr = rage(&["ymap", "lodlights", s(&stream.join("smoke.ymap")), "-o", s(&out), "--name", "smoke"]);
    assert!(!stderr.contains("warning"), "{stderr}");
    assert!(stderr.contains("from 4 entities"), "{stderr}");

    let oracle_out = tmp.path().join("oracle.txt");
    run(Command::new(&exe).args(["lodlights", s(&stream.join("smoke.ymap")), "--ytyp", s(&stream), "--models", s(&stream), "-o", s(&oracle_out)]).env("CODEWALKER_CORE_DIR", &core));
    let mut expected: HashMap<u32, Row> = HashMap::new();
    let mut entities_seen = 0;
    for line in std::fs::read_to_string(&oracle_out).unwrap().lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.first() {
            Some(&"entity") => {
                entities_seen += 1;
                assert!(!matches!(words.get(2), Some(&"no-archetype") | Some(&"no-model")), "the oracle could not resolve: {line}");
            }
            Some(&"light") => {
                let f = |i: usize| words[i].parse::<f32>().unwrap();
                let u = |i: usize| words[i].parse::<u32>().unwrap();
                let row = Row {
                    position: Vec3::new(f(2), f(3), f(4)),
                    direction: Vec3::new(f(5), f(6), f(7)),
                    colour: u(8),
                    flags: u(9),
                    inner: u(10) as u8,
                    outer: u(11) as u8,
                    corona: u(12) as u8,
                    falloff: f(13),
                    falloff_exponent: f(14),
                };
                assert!(expected.insert(u(1), row).is_none(), "the oracle gave two lights the hash {}", words[1]);
            }
            _ => {}
        }
    }
    assert_eq!(entities_seen, 4);
    assert!(expected.len() >= 5, "the lamps should carry several lights, got {}", expected.len());

    let lod = dump_meta(&std::fs::read(out.join("smoke_lodlights.ymap")).unwrap()).unwrap();
    let dist = dump_meta(&std::fs::read(out.join("smoke_distantlights.ymap")).unwrap()).unwrap();
    let (lod, dist) = (lod.root.as_struct().unwrap(), dist.root.as_struct().unwrap());
    let soa = |m: &rage_formats::MetaStruct, n: &str, f: &str| m.field(n).and_then(MetaValue::as_struct).unwrap().field(f).unwrap().items().to_vec();
    let hashes = soa(lod, "LODLightsSOA", "hash");
    let n = hashes.len();
    assert_eq!(n, expected.len(), "CodeWalker generated {} lights, rage {n}", expected.len());
    let ints = |f: &str| soa(lod, "LODLightsSOA", f).iter().map(|v| v.as_i64().unwrap()).collect::<Vec<_>>();
    let (directions, flags, inner, outer, corona) = (soa(lod, "LODLightsSOA", "direction"), ints("timeAndStateFlags"), ints("coneInnerAngle"), ints("coneOuterAngleOrCapExt"), ints("coronaIntensity"));
    let falloffs: Vec<f32> = soa(lod, "LODLightsSOA", "falloff").iter().map(|v| v.as_f32().unwrap()).collect();
    let exponents: Vec<f32> = soa(lod, "LODLightsSOA", "falloffExponent").iter().map(|v| v.as_f32().unwrap()).collect();
    let positions = soa(dist, "DistantLODLightsSOA", "position");
    let colours = soa(dist, "DistantLODLightsSOA", "RGBI");
    let near = |a: Vec3, b: Vec3| (a - b).length() < 2e-3;
    for i in 0..n {
        let hash = hashes[i].as_u32().unwrap();
        let want = expected.get(&hash).unwrap_or_else(|| panic!("rage gave light {i} the hash {hash}, which CodeWalker gave no light: {expected:#?}"));
        let position = positions[i].as_vec3().unwrap();
        let direction = directions[i].as_vec3().unwrap();
        assert!(near(position, want.position), "light {hash}: position {position:?}, CodeWalker {:?}", want.position);
        assert!(near(direction, want.direction), "light {hash}: direction {direction:?}, CodeWalker {:?}", want.direction);
        assert_eq!(colours[i].as_u32(), Some(want.colour), "light {hash}: colour");
        assert_eq!(flags[i], i64::from(want.flags), "light {hash}: flags");
        assert_eq!((inner[i], outer[i], corona[i]), (i64::from(want.inner), i64::from(want.outer), i64::from(want.corona)), "light {hash}: angles and corona");
        assert_eq!((falloffs[i], exponents[i]), (want.falloff, want.falloff_exponent), "light {hash}: falloff");
    }
    // Sorted by hash, as the generator sorts them.
    assert!(hashes.windows(2).all(|w| w[0].as_u32() <= w[1].as_u32()));
}
