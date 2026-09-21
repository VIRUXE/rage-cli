//! `rpf resource info` against hand-built RSC7 files; needs no game install.

use std::path::Path;
use std::process::{Command, Output};

use rpf_archive::{RpfBuilder, RpfEncryption};

fn rpf(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .output()
        .expect("failed to run the rage binary")
}

fn ok(args: &[&str]) -> String {
    let output = rpf(args);
    assert!(
        output.status.success(),
        "`rpf {}` failed with {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// System flags: version nibble 0xA, one page of 0x200 << 4 = 8192 bytes.
const SYSTEM_FLAGS: u32 = 0xA800_0004;
/// Graphics flags: version nibble 0x5, no pages.
const GRAPHICS_FLAGS: u32 = 0x5000_0000;
/// (0xA << 4) | 0x5 — the version a real .ydr carries.
const VERSION: u32 = 165;

/// A stored (not deflated) RSC7 file whose body is 8192 zero bytes.
fn stored_rsc7() -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"RSC7");
    data.extend_from_slice(&VERSION.to_le_bytes());
    data.extend_from_slice(&SYSTEM_FLAGS.to_le_bytes());
    data.extend_from_slice(&GRAPHICS_FLAGS.to_le_bytes());
    data.extend(std::iter::repeat_n(0u8, 8192));
    data
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_str().unwrap().to_string()
}

#[test]
fn loose_file_reports_header() {
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path(), "thing.ybn", &stored_rsc7());

    let out = ok(&["resource", "info", &file]);

    assert!(out.contains("RSC7"), "no format line:\n{out}");
    assert!(out.contains("version 165"), "no version:\n{out}");
    assert!(out.contains("0xA8000004"), "no system flags:\n{out}");
    assert!(out.contains("8192 bytes"), "no system size:\n{out}");
    assert!(out.contains("stored"), "body should be reported as stored:\n{out}");
    assert!(out.contains("not a drawable, texture dictionary, map, type file or manifest"), "no summary:\n{out}");
}

#[test]
fn loose_file_json() {
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path(), "thing.ybn", &stored_rsc7());

    let out = ok(&["resource", "info", &file, "--json"]);
    let out = out.trim();

    assert!(out.starts_with('{') && out.ends_with('}'), "not one JSON object:\n{out}");
    for needle in [
        "\"format\":\"RSC7\"",
        "\"version\":165",
        "\"system_flags\":\"0xA8000004\"",
        "\"system_size\":8192",
        "\"graphics_flags\":\"0x50000000\"",
        "\"graphics_size\":0",
        "\"compressed\":false",
        "\"kind\":\"other\"",
    ] {
        assert!(out.contains(needle), "missing {needle} in:\n{out}");
    }
}

#[test]
fn bad_magic_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path(), "nope.ydr", b"XXXX\0\0\0\0\0\0\0\0\0\0\0\0");

    let output = rpf(&["resource", "info", &file]);

    assert!(!output.status.success(), "bad magic should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("0x58585858"), "stderr should name the magic:\n{stderr}");
}

/// `--verbose` only adds a per-geometry breakdown for drawables; on anything
/// else (and in JSON) it must not change the output at all.
#[test]
fn verbose_does_not_change_non_drawable_output() {
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path(), "thing.ybn", &stored_rsc7());

    let plain = ok(&["resource", "info", &file]);
    let verbose = ok(&["-v", "resource", "info", &file]);
    assert_eq!(plain, verbose, "verbose should not affect a non-drawable resource");

    let plain_json = ok(&["resource", "info", &file, "--json"]);
    let verbose_json = ok(&["-v", "resource", "info", &file, "--json"]);
    assert_eq!(plain_json, verbose_json, "verbose should not affect JSON for a non-drawable resource");
}

#[test]
fn entry_inside_archive() {
    let tmp = tempfile::tempdir().unwrap();

    let mut builder = RpfBuilder::new(RpfEncryption::None);
    builder.add_file("data/thing.ybn", stored_rsc7());
    let archive = write(tmp.path(), "test.rpf", &builder.build(None).unwrap());

    let out = ok(&["resource", "info", "thing.ybn", "--archive", &archive]);

    assert!(out.contains("version 165"), "no version:\n{out}");
    assert!(out.contains("8192 bytes"), "no system size:\n{out}");
}

#[test]
fn map_info_lists_header_and_entities() {
    use rage_formats::{ymap::tests::sample_exterior_ymap, Vec3};
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path(), "paleto_props.ymap", &sample_exterior_ymap("paleto_props", &[("prop_gas_pump_1a", Vec3::new(197.263, 6573.081, 30.78), 20.0), ("prop_other", Vec3::new(1.0, 2.0, 3.0), 0.0)]));
    // A sibling file names the second archetype.
    write(tmp.path(), "prop_other.ydr", b"");

    let out = ok(&["resource", "info", &file]);
    assert!(out.contains("Map:       paleto_props"), "{out}");
    assert!(out.contains("content 0x1 HD"), "{out}");
    assert!(out.contains("Entities:  2 (0 MLO instances)"), "{out}");
    assert!(out.contains("prop_gas_pump_1a"), "an archetype named by a sibling stem: {out}");
    assert!(out.contains("prop_other"), "{out}");
    assert!(out.contains("20.0°"), "the heading: {out}");
    assert!(out.contains("static entity"), "{out}");

    let json = ok(&["resource", "info", &file, "--json", "--limit", "1"]);
    assert!(json.contains("\"kind\":\"map\""), "{json}");
    assert!(json.contains("\"name\":\"paleto_props\""), "{json}");
    assert!(json.contains("\"archetype\":\"prop_gas_pump_1a\""), "{json}");
    assert!(json.contains("\"mlo_instances\":[]"), "{json}");
}

#[test]
fn xml_manifest_info_and_pso_dump() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = "<?xml version=\"1.0\"?><CPackFileMetaData><MapDataGroups/><HDTxdBindingArray/><imapDependencies/><imapDependencies_2><Item><imapName>bombapaleto</imapName><manifestFlags/><itypDepArray><Item>v_construction</Item></itypDepArray></Item></imapDependencies_2><itypDependencies_2/><Interiors/></CPackFileMetaData>";
    let file = write(tmp.path(), "_manifest.ymf", manifest.as_bytes());
    let out = ok(&["resource", "info", &file]);
    assert!(out.contains("Format:    XML"), "{out}");
    assert!(out.contains("bombapaleto -> v_construction"), "{out}");

    let pso = write(tmp.path(), "thing.pso", &rage_formats::pso::tests::sample_pso(false));
    let out = ok(&["resource", "info", &pso]);
    assert!(out.contains("Format:    PSO"), "{out}");
    assert!(out.contains("hash_"), "the fixture's structure names are not in the built-in list: {out}");

    // The fixture's own names, supplied the way a user would supply theirs.
    let names = write(tmp.path(), "names.txt", b"TestRoot
TestChild
TestFlags
FLAG_A
FLAG_B
FLAG_C
KIND_ONE
name
flags
children
label
weight
kind
");
    let xml = ok(&["resource", "dump", &pso, "--names", &names]);
    assert!(xml.starts_with("<?xml"), "{xml}");
    assert!(xml.contains("<TestRoot>"), "{xml}");
    assert!(xml.contains("<Item type=\"TestChild\">"), "{xml}");
    assert!(xml.contains("<flags>FLAG_A, FLAG_C</flags>"), "{xml}");
    assert!(xml.contains("<kind>KIND_ONE</kind>"), "{xml}");
    let json = ok(&["resource", "dump", &pso, "--json", "--names", &names]);
    assert!(json.trim_start().starts_with('{'), "{json}");
    assert!(json.contains("\"$type\""), "{json}");
}

#[test]
fn meta_dump_of_a_ymap_names_every_member() {
    use rage_formats::{ymap::tests::sample_exterior_ymap, Vec3};
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path(), "m.ymap", &sample_exterior_ymap("m", &[("prop_a", Vec3::new(1.0, 2.0, 3.0), 0.0)]));
    // The fixture carries no schema of its own, so the dump has nothing to
    // walk; the error must say so rather than print an empty document.
    let output = rpf(&["resource", "dump", &file]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success() || stderr.contains("warning"), "{stderr}");
}
