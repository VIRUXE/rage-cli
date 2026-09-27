//! `rage manifest generate` over a resource the tool builds itself from XML;
//! needs no game install.

use std::path::Path;
use std::process::{Command, Output};

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

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn archetype(kind: &str, name: &str, rest: &str) -> String {
    format!(
        r#"    <Item type="{kind}">
      <lodDist value="100"/>
      <flags value="0"/>
      <specialAttribute value="0"/>
      <bbMin x="-1" y="-1" z="0"/>
      <bbMax x="1" y="1" z="2"/>
      <bsCentre x="0" y="0" z="1"/>
      <bsRadius value="2"/>
      <hdTextureDist value="50"/>
      <name>{name}</name>
      <textureDictionary/>
      <clipDictionary/>
      <drawableDictionary/>
      <physicsDictionary/>
      <assetType>ASSET_TYPE_DRAWABLE</assetType>
      <assetName>{name}</assetName>
      <extensions/>
{rest}    </Item>
"#
    )
}

fn entity(kind: &str, archetype: &str, rest: &str) -> String {
    format!(
        r#"        <Item type="{kind}">
          <archetypeName>{archetype}</archetypeName>
          <flags value="0"/>
          <guid value="0"/>
          <position x="0" y="0" z="0"/>
          <rotation x="0" y="0" z="0" w="1"/>
          <scaleXY value="1"/>
          <scaleZ value="1"/>
          <parentIndex value="-1"/>
          <lodDist value="100"/>
          <childLodDist value="0"/>
          <lodLevel>LODTYPES_DEPTH_ORPHANHD</lodLevel>
          <numChildren value="0"/>
          <priorityLevel>PRI_REQUIRED</priorityLevel>
          <extensions/>
          <ambientOcclusionMultiplier value="255"/>
          <artificialAmbientOcclusion value="255"/>
          <tintValue value="0"/>
{rest}        </Item>
"#
    )
}

fn types(name: &str, archetypes: &str) -> String {
    format!("<CMapTypes>\n  <extensions/>\n  <archetypes>\n{archetypes}  </archetypes>\n  <name>{name}</name>\n  <dependencies/>\n  <compositeEntityTypes/>\n</CMapTypes>\n")
}

/// Builds `xml` into `out` through `resource build`.
fn build(tmp: &Path, xml: &str, out: &Path) {
    let src = tmp.join(format!("{}.xml", out.file_name().unwrap().to_str().unwrap()));
    std::fs::write(&src, xml).unwrap();
    ok(&["resource", "build", "--no-recalc", s(&src), "-o", s(out)]);
}

/// A resource with a prop type file, an interior type file whose rooms use
/// one of those props, and a map placing the interior, a prop and a model
/// declared nowhere.
fn resource(tmp: &Path) -> std::path::PathBuf {
    let stream = tmp.join("res").join("stream");
    std::fs::create_dir_all(stream.join("meta")).unwrap();

    build(tmp, &types("props", &(archetype("CBaseArchetypeDef", "shop_chair", "") + &archetype("CBaseArchetypeDef", "shop_lamp", ""))), &stream.join("props.ytyp"));

    let rooms = entity("CEntityDef", "shop_shell", "") + &entity("CEntityDef", "shop_chair", "");
    let mlo = format!(
        "      <mloFlags value=\"0\"/>\n      <entities>\n{rooms}      </entities>\n      <rooms/>\n      <portals/>\n      <entitySets/>\n      <timeCycleModifiers/>\n"
    );
    let int_types = archetype("CBaseArchetypeDef", "shop_shell", "") + &archetype("CMloArchetypeDef", "int_shop", &mlo);
    build(tmp, &types("int_shop", &int_types), &stream.join("meta").join("int_shop.ytyp"));

    let instance = "          <groupId value=\"0\"/>\n          <floorId value=\"0\"/>\n          <defaultEntitySets/>\n          <numExitPortals value=\"0\"/>\n          <MLOInstflags value=\"0\"/>\n";
    let entities = entity("CMloInstanceDef", "int_shop", instance) + &entity("CEntityDef", "shop_lamp", "") + &entity("CEntityDef", "prop_from_nowhere", "");
    let map = format!(
        "<CMapData>\n  <name>shop</name>\n  <parent/>\n  <flags value=\"0\"/>\n  <contentFlags value=\"9\"/>\n  <streamingExtentsMin x=\"-100\" y=\"-100\" z=\"-100\"/>\n  <streamingExtentsMax x=\"100\" y=\"100\" z=\"100\"/>\n  <entitiesExtentsMin x=\"-1\" y=\"-1\" z=\"0\"/>\n  <entitiesExtentsMax x=\"1\" y=\"1\" z=\"2\"/>\n  <entities>\n{entities}  </entities>\n  <containerLods/>\n  <boxOccluders/>\n  <occludeModels/>\n  <physicsDictionaries/>\n</CMapData>\n"
    );
    build(tmp, &map, &stream.join("shop.ymap"));
    tmp.join("res")
}

#[test]
fn a_resource_gets_codewalkers_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let res = resource(tmp.path());

    let (xml, stderr) = ok(&["manifest", "generate", s(&res), "--format", "xml"]);
    assert!(stderr.contains("1 archetype(s) are declared by no .ytyp"), "{stderr}");
    assert!(stderr.contains("--exe"), "without the game the hint says how to resolve vanilla archetypes: {stderr}");
    let squeezed: String = xml.split_whitespace().collect();
    assert!(
        squeezed.contains(
            "<imapName>shop</imapName><manifestFlags>INTERIOR_DATA</manifestFlags><itypDepArray><Item>int_shop</Item><Item>props</Item></itypDepArray>"
        ),
        "{xml}"
    );
    assert!(
        squeezed.contains("<itypName>int_shop</itypName><manifestFlags>INTERIOR_DATA</manifestFlags><itypDepArray><Item>props</Item></itypDepArray>"),
        "{xml}"
    );
    assert!(squeezed.contains("<Name>int_shop</Name><Bounds><Item>int_shop</Item></Bounds>"), "{xml}");

    // The default: a PSO _manifest.ymf in stream/, which reads back.
    let (_, stderr) = ok(&["manifest", "generate", s(&res)]);
    let ymf = res.join("stream").join("_manifest.ymf");
    assert!(stderr.contains("1 map(s), 1 interior type file(s), 1 interior(s)"), "{stderr}");
    let (info, _) = ok(&["resource", "info", s(&ymf), "--json"]);
    let v = json::parse(&info).unwrap();
    let m = &v["manifest"];
    assert_eq!(m["imap_dependencies_2"][0]["name"], "shop");
    assert_eq!(m["imap_dependencies_2"][0]["manifest_flags"], 1);
    assert_eq!(m["imap_dependencies_2"][0]["ityp_deps"].len(), 2);
    assert_eq!(m["ityp_dependencies_2"][0]["name"], "int_shop");
    assert_eq!(m["ityp_dependencies_2"][0]["ityp_deps"][0], "props");
    assert_eq!(m["interiors"][0]["name"], "int_shop");
}

#[test]
fn a_folder_with_no_maps_or_types_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("readme.txt"), "nothing here").unwrap();
    let output = rage(&["manifest", "generate", s(tmp.path())]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no readable .ymap or .ytyp"));
}
