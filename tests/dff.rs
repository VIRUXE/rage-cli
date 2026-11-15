mod common;
use std::{fs, path::Path, process::Command};
fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}
fn ok(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env_remove("GTAV_PATH")
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
#[test]
fn dff_info_and_editable_build() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("model.dff");
    let doc = t.path().join("model.json");
    let output = t.path().join("rebuilt.dff");
    let bytes = common::triangle_dff();
    fs::write(&source, &bytes).unwrap();
    let info = json::parse(&ok(&["resource", "info", s(&source), "--json"])).unwrap();
    assert_eq!(info["frames"][0]["name"], "triangle");
    assert_eq!(info["geometries"][0]["vertices"], 3);
    ok(&["resource", "dump", s(&source), "-o", s(&doc)]);
    ok(&["resource", "build", s(&doc), "-o", s(&output)]);
    assert!(fs::read(&output).unwrap() == bytes);
    let mut value = json::parse(&fs::read_to_string(&doc).unwrap()).unwrap();
    value["scene"]["geometries"][0]["morph_targets"][0]["positions"][1][0] = 2.into();
    fs::write(&doc, value.pretty(2)).unwrap();
    ok(&["resource", "build", s(&doc), "-o", s(&output)]);
    let dff = rage_formats::renderware::dff::Dff::parse(&fs::read(output).unwrap()).unwrap();
    assert_eq!(dff.geometries[0].morph_targets[0].positions[1].x, 2.);
}
