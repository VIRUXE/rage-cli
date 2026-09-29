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

/// The triangle fixture with a box bound, built; with `copy` the root's pointer at the first offset is
/// copied over the one at the second (offsets into the system section).
fn boxed_triangle(copy: Option<(usize, usize)>) -> Vec<u8> {
    let xml = XML.replace("</Drawable>", &format!("{BOX_BOUND}</Drawable>"));
    let file = rage_formats::build_ydr_from_xml(&xml, None).unwrap();
    let Some((from, to)) = copy else { return file };
    let (mut sys, gfx) = rage_formats::prepare_rsc7(&file).unwrap();
    let flags = |at: usize| u32::from_le_bytes(file[at..at + 4].try_into().unwrap());
    let ptr = sys[from..from + 8].to_vec();
    sys[to..to + 8].copy_from_slice(&ptr);
    rage_formats::build_rsc7_with_flags(flags(4), flags(8), &sys, flags(12), &gfx)
}

#[test]
fn info_on_a_drawable_whose_pointers_cross_falls_back_to_the_old_parser() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.ydr");
    std::fs::write(&good, boxed_triangle(None)).unwrap();
    // the skeleton pointer (0x18) made equal to the shader group pointer (0x10)
    let bad = dir.path().join("bad.ydr");
    std::fs::write(&bad, boxed_triangle(Some((0x10, 0x18)))).unwrap();
    let (out, err) = ok(&["resource", "info", s(&bad)]);
    assert!(!err.contains("panicked"), "{err}");
    assert!(out.contains("tri_prop") && out.contains("1 models, 1 geometries"), "{out}");
    // the old parser's lines, without the block reader's bound line that the good file has
    let (good_out, _) = ok(&["resource", "info", s(&good)]);
    assert!(good_out.lines().any(|l| l.contains("bound:") && l.contains("Box")), "{good_out}");
    let old_lines = |text: &str| -> Vec<String> {
        text.lines().filter(|l| !l.starts_with("File:") && !l.starts_with("Body:") && !l.contains("bound:")).map(str::to_owned).collect()
    };
    assert!(!out.contains("bound:"), "{out}");
    assert_eq!(old_lines(&out), old_lines(&good_out));
    let err = fails(&["resource", "dump", s(&bad), "--no-dds"]);
    assert!(err.contains("already read as ShaderGroup") && !err.contains("panicked"), "{err}");
}

#[test]
fn build_warns_of_a_texture_that_is_not_embedded_and_strict_refuses_it() {
    let dir = tempfile::tempdir().unwrap();
    let xml = dir.path().join("tri.xml");
    std::fs::write(&xml, XML).unwrap();
    let ydr = dir.path().join("tri.ydr");
    let (_, err) = ok(&["resource", "build", s(&xml), "-o", s(&ydr)]);
    let warnings: Vec<&str> = err.lines().filter(|l| l.starts_with("warning: ")).collect();
    assert_eq!(warnings, ["warning: shader 0 parameter DiffuseSampler: texture 'missing_tex' is not embedded (resolved at runtime from the archetype's txd)"], "{err}");
    assert!(ydr.is_file());

    let strict = dir.path().join("strict.ydr");
    let err = fails(&["resource", "build", s(&xml), "-o", s(&strict), "--strict"]);
    assert!(err.contains("missing_tex") && err.contains("1 warning(s) (--strict)"), "{err}");
    assert!(!strict.exists(), "nothing is written");
}

#[test]
fn a_bound_document_does_not_build_a_drawable() {
    let dir = tempfile::tempdir().unwrap();
    let xml = dir.path().join("c.xml");
    std::fs::write(&xml, YBN_XML).unwrap();
    let ydr = dir.path().join("c.ydr");
    let err = fails(&["resource", "build", s(&xml), "-o", s(&ydr)]);
    assert!(err.contains("root element is <BoundsFile>, not <Drawable>"), "{err}");
    assert!(!ydr.exists());
    let tri = dir.path().join("tri.xml");
    std::fs::write(&tri, XML).unwrap();
    let err = fails(&["resource", "build", s(&tri), "-o", s(&dir.path().join("t.ybn"))]);
    assert!(err.contains("root element is <Drawable>, not <BoundsFile> or <Bounds>"), "{err}");
}
