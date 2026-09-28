//! `rage resource dump`, `build` and `info` on drawables and bounds; needs no game install.

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


const XML: &str = include_str!("fixtures/one_triangle.ydr.xml");
const YBN_XML: &str = include_str!("fixtures/composite.ybn.xml");

#[test]
fn builds_dumps_and_inspects_a_drawable() {
    let dir = tempfile::tempdir().unwrap();
    let xml = dir.path().join("tri.xml");
    std::fs::write(&xml, XML).unwrap();
    let ydr = dir.path().join("tri.ydr");
    let (_, err) = ok(&["resource", "build", s(&xml), "-o", s(&ydr)]);
    assert!(err.contains("RSC7 drawable"), "{err}");
    let back = dir.path().join("back.xml");
    ok(&["resource", "dump", s(&ydr), "-o", s(&back)]);
    assert_eq!(std::fs::read_to_string(back).unwrap(), XML);
    let (out, _) = ok(&["resource", "info", s(&ydr)]);
    assert!(out.contains("tri_prop") && out.contains("shaders:  1") && out.contains("1 models, 1 geometries"), "{out}");
}

#[test]
fn a_missing_texture_file_names_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let xml = XML.replace("<TextureDictionary />", "<TextureDictionary><Item><Name>gone</Name><Unk32 value=\"0\"/><Usage>DEFAULT</Usage><UsageFlags/><ExtraFlags value=\"0\"/><Width value=\"4\"/><Height value=\"4\"/><MipLevels value=\"1\"/><Format>D3DFMT_DXT1</Format><FileName>gone.dds</FileName></Item></TextureDictionary>");
    let p = dir.path().join("t.xml");
    std::fs::write(&p, xml).unwrap();
    let err = fails(&["resource", "build", s(&p), "-o", s(&dir.path().join("t.ydr"))]);
    assert!(err.contains("gone.dds"), "{err}");
    assert!(err.contains(dir.path().to_str().unwrap()), "{err}");
}

#[test]
fn ybn_from_xml() {
    let dir = tempfile::tempdir().unwrap();
    let xml = dir.path().join("c.xml");
    std::fs::write(&xml, YBN_XML).unwrap();
    let ybn = dir.path().join("c.ybn");
    let (_, err) = ok(&["resource", "build", s(&xml), "-o", s(&ybn)]);
    assert!(err.contains("RSC7 bounds") && err.contains("Composite"), "{err}");
    let (dumped, _) = ok(&["resource", "dump", s(&ybn)]);
    assert_eq!(dumped, YBN_XML);
    let (out, _) = ok(&["resource", "info", s(&ybn)]);
    assert!(out.contains("Composite"), "{out}");
}

#[test]
fn drawables_do_not_dump_as_json_and_the_format_is_sniffed_from_the_xml() {
    let dir = tempfile::tempdir().unwrap();
    let xml = dir.path().join("tri.xml");
    std::fs::write(&xml, XML).unwrap();
    let ydr = dir.path().join("tri.ydr");
    ok(&["resource", "build", s(&xml), "-o", s(&ydr)]);
    let err = fails(&["resource", "dump", s(&ydr), "--json"]);
    assert!(err.contains("XML only"), "{err}");
    let odd = dir.path().join("out.bin");
    let (_, err) = ok(&["resource", "build", s(&xml), "-o", s(&odd)]);
    assert!(err.contains("RSC7 drawable"), "{err}");
}

const BOX_BOUND: &str = include_str!("fixtures/box_bounds_fragment.xml");

#[test]
fn info_lists_a_drawables_bound() {
    let dir = tempfile::tempdir().unwrap();
    let with_bound = XML.replace("</Drawable>", &format!("{BOX_BOUND}</Drawable>"));
    let xml = dir.path().join("boxed.xml");
    std::fs::write(&xml, with_bound).unwrap();
    let ydr = dir.path().join("boxed.ydr");
    ok(&["resource", "build", s(&xml), "-o", s(&ydr)]);
    let (out, _) = ok(&["resource", "info", s(&ydr)]);
    assert!(out.lines().any(|l| l.contains("bound:") && l.contains("Box")), "{out}");
    let (json, _) = ok(&["resource", "info", s(&ydr), "--json"]);
    assert!(json.contains("\"bound\":{\"kind\":\"Box\"}"), "{json}");
}
