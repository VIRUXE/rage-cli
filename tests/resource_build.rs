//! `rage resource build` against files the tool itself builds from XML; needs no game install.

use std::path::Path;
use std::process::{Command, Output};

fn rage(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .env("RAGE_NAMES", "nowhere/names.txt")
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

/// A map with two entities, the way `dump` prints one (CodeWalker's layout).
const MAP_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<CMapData>
  <name>map1</name>
  <parent/>
  <flags value="0"/>
  <contentFlags value="1"/>
  <streamingExtentsMin x="180.5" y="6550.25" z="20"/>
  <streamingExtentsMax x="220.5" y="6590.25" z="40"/>
  <entitiesExtentsMin x="190" y="6560" z="25"/>
  <entitiesExtentsMax x="210" y="6580" z="35"/>
  <entities>
    <Item type="CEntityDef">
      <archetypeName>prop_gas_pump_1a</archetypeName>
      <flags value="1572864"/>
      <guid value="1"/>
      <position x="197.263" y="6573.081" z="30.78"/>
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
      <archetypeName>prop_bench_01a</archetypeName>
      <flags value="32"/>
      <guid value="2"/>
      <position x="1" y="2" z="3"/>
      <rotation x="0" y="0" z="0.7071" w="0.7071"/>
      <scaleXY value="1"/>
      <scaleZ value="1"/>
      <parentIndex value="-1"/>
      <lodDist value="60"/>
      <childLodDist value="0"/>
      <lodLevel>LODTYPES_DEPTH_HD</lodLevel>
      <numChildren value="0"/>
      <priorityLevel>PRI_OPTIONAL_LOW</priorityLevel>
      <extensions/>
      <ambientOcclusionMultiplier value="255"/>
      <artificialAmbientOcclusion value="255"/>
      <tintValue value="0"/>
    </Item>
  </entities>
  <containerLods/>
  <boxOccluders/>
  <occludeModels/>
  <physicsDictionaries>
    <Item>cs1_02_phys</Item>
  </physicsDictionaries>
  <block>
    <version value="0"/>
    <flags value="0"/>
    <name>built for a test</name>
    <exportedBy>rage</exportedBy>
    <owner/>
    <time/>
  </block>
</CMapData>
"#;

const MANIFEST_XML: &str = r#"<CPackFileMetaData>
  <MapDataGroups/>
  <HDTxdBindingArray/>
  <imapDependencies/>
  <imapDependencies_2>
    <Item>
      <imapName>map1</imapName>
      <manifestFlags>INTERIOR_DATA</manifestFlags>
      <itypDepArray>
        <Item>casas_praia_types</Item>
        <Item>v_int_1</Item>
      </itypDepArray>
    </Item>
  </imapDependencies_2>
  <itypDependencies_2/>
  <Interiors>
    <Item>
      <Name>map1</Name>
      <Bounds>
        <Item>map1</Item>
      </Bounds>
    </Item>
  </Interiors>
</CPackFileMetaData>
"#;

#[test]
fn a_map_builds_from_xml_reads_back_and_its_name_can_be_fixed() {
    let tmp = tempfile::tempdir().unwrap();
    let xml = tmp.path().join("map.xml");
    std::fs::write(&xml, MAP_XML).unwrap();
    let original = tmp.path().join("map1.ymap");
    let (_, stderr) = ok(&["resource", "build", s(&xml), "-o", s(&original)]);
    assert!(stderr.contains("Wrote") && stderr.contains("RSC7 Meta") && stderr.contains("root CMapData"), "{stderr}");
    assert!(!stderr.contains("warning"), "{stderr}");

    let (info, _) = ok(&["resource", "info", s(&original)]);
    assert!(info.contains("RSC7  version 2"), "{info}");
    assert!(info.contains("Map:       map1"), "{info}");
    assert!(info.contains("Entities:  2"), "{info}");
    assert!(info.contains("197.263") && info.contains("6573.081"), "{info}");

    // The case from the issue: the map still calls itself map1 after the
    // file was renamed; fix it in the dump and build the file back.
    let (dump0, _) = ok(&["resource", "dump", s(&original)]);
    assert!(dump0.contains("<name>map1</name>"), "{dump0}");
    assert!(dump0.contains("<lodLevel>LODTYPES_DEPTH_ORPHANHD</lodLevel>"), "{dump0}");
    let edited = tmp.path().join("edited.xml");
    std::fs::write(&edited, dump0.replacen("<name>map1</name>", "<name>casas_praia_extras</name>", 1)).unwrap();
    let built = tmp.path().join("casas_praia_extras.ymap");
    ok(&["resource", "build", s(&edited), "-o", s(&built)]);
    let (info, _) = ok(&["resource", "info", s(&built)]);
    assert!(info.contains("Map:       casas_praia_extras"), "{info}");
    assert!(!info.contains("calls itself"), "{info}");
    let (dump1, _) = ok(&["resource", "dump", s(&built)]);
    assert_eq!(dump1, dump0.replacen("<name>map1</name>", "<name>casas_praia_extras</name>", 1));
}

#[test]
fn json_builds_the_same_file_as_xml_and_the_original_can_supply_the_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let xml = tmp.path().join("map.xml");
    std::fs::write(&xml, MAP_XML).unwrap();
    let original = tmp.path().join("map1.ymap");
    ok(&["resource", "build", s(&xml), "-o", s(&original)]);
    let (dump0, _) = ok(&["resource", "dump", s(&original)]);

    let json = tmp.path().join("map.json");
    ok(&["resource", "dump", s(&original), "--json", "-o", s(&json)]);
    // Same stem as the original, so the dump names the map the same way
    // (names resolve from sibling files; there is no name list here).
    let from_json = tmp.path().join("out").join("map1.ymap");
    ok(&["resource", "build", s(&json), "-o", s(&from_json)]);
    let (dump1, _) = ok(&["resource", "dump", s(&from_json)]);
    assert_eq!(dump0, dump1);

    // --schema, and an existing output, take the definitions from the file.
    let (_, stderr) = ok(&["resource", "build", s(&json), "-o", s(&from_json), "--schema", s(&original)]);
    assert!(stderr.matches("Using the structure definitions of").count() == 2, "{stderr}");
    let (dump2, _) = ok(&["resource", "dump", s(&from_json)]);
    assert_eq!(dump0, dump2);
}

#[test]
fn a_pso_manifest_builds_from_xml_and_from_its_own_dump() {
    let tmp = tempfile::tempdir().unwrap();
    let xml = tmp.path().join("manifest.xml");
    std::fs::write(&xml, MANIFEST_XML).unwrap();
    let built = tmp.path().join("stream").join("_manifest.ymf");
    let (_, stderr) = ok(&["resource", "build", s(&xml), "-o", s(&built)]);
    assert!(stderr.contains("PSO from") && stderr.contains("root CPackFileMetaData"), "{stderr}");
    assert!(!stderr.contains("warning"), "{stderr}");

    let (info, _) = ok(&["resource", "info", s(&built)]);
    assert!(info.contains("Format:    PSO"), "{info}");
    assert!(info.contains("Map dependencies (1)") && info.contains("[INTERIOR_DATA]"), "{info}");
    assert!(info.contains("Interiors (1)"), "{info}");

    let (dump0, _) = ok(&["resource", "dump", s(&built)]);
    let dumped = tmp.path().join("dumped.xml");
    std::fs::write(&dumped, &dump0).unwrap();
    let again = tmp.path().join("again.ymf");
    ok(&["resource", "build", s(&dumped), "-o", s(&again)]);
    let (dump1, _) = ok(&["resource", "dump", s(&again)]);
    assert_eq!(dump0, dump1);

    // A sample whose structures CodeWalker does not know needs the file itself.
    let sample = tmp.path().join("sample.pso");
    std::fs::write(&sample, rage_formats::pso::tests::sample_pso(false)).unwrap();
    let (sample_dump, _) = ok(&["resource", "dump", s(&sample)]);
    let sample_xml = tmp.path().join("sample.xml");
    std::fs::write(&sample_xml, &sample_dump).unwrap();
    let err = fails(&["resource", "build", s(&sample_xml), "-o", s(&tmp.path().join("sample2.pso"))]);
    assert!(err.contains("no PSO schema for the root"), "{err}");
    ok(&["resource", "build", s(&sample_xml), "-o", s(&tmp.path().join("sample2.pso")), "--schema", s(&sample)]);
    let (sample_dump2, _) = ok(&["resource", "dump", s(&tmp.path().join("sample2.pso"))]);
    assert_eq!(sample_dump, sample_dump2);
}

#[test]
fn the_container_comes_from_the_extension_or_format() {
    let tmp = tempfile::tempdir().unwrap();
    let xml = tmp.path().join("x.xml");
    std::fs::write(&xml, "<CMapData><name>a</name><flags value=\"0\"/></CMapData>").unwrap();
    let err = fails(&["resource", "build", s(&xml), "-o", s(&tmp.path().join("x.bin"))]);
    assert!(err.contains("cannot tell the container"), "{err}");
    let err = fails(&["resource", "build", s(&xml), "-o", s(&tmp.path().join("x.bin")), "--format", "rbf"]);
    assert!(err.contains("expected meta or pso"), "{err}");
    let (_, stderr) = ok(&["resource", "build", s(&xml), "-o", s(&tmp.path().join("x.bin")), "--format", "meta"]);
    assert!(stderr.contains("RSC7 Meta"), "{stderr}");

    // An unknown root structure is an error; a bad member is a warning
    // unless --strict.
    let bad = tmp.path().join("bad.xml");
    std::fs::write(&bad, "<NotAStructure><name>a</name></NotAStructure>").unwrap();
    let err = fails(&["resource", "build", s(&bad), "-o", s(&tmp.path().join("bad.ymap"))]);
    assert!(err.contains("no schema for the root structure"), "{err}");
    let odd = tmp.path().join("odd.xml");
    std::fs::write(&odd, "<CMapData><name>a</name><flags value=\"many\"/></CMapData>").unwrap();
    let (_, stderr) = ok(&["resource", "build", s(&odd), "-o", s(&tmp.path().join("odd.ymap"))]);
    assert!(stderr.contains("warning") && stderr.contains("is not a integer"), "{stderr}");
    let err = fails(&["resource", "build", s(&odd), "-o", s(&tmp.path().join("odd.ymap")), "--strict"]);
    assert!(err.contains("--strict"), "{err}");
}
