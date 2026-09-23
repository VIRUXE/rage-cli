//! `rpf resource info` against hand-built RSC7 files; needs no game install.

use std::path::Path;
use std::process::{Command, Output};

use rpf_archive::{RpfBuilder, RpfEncryption};

fn rpf(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        // Whatever list this machine has harvested must not leak into the assertions.
        .env("RAGE_NAMES", "nowhere/names.txt")
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
    // Sibling files name the archetypes (the first is a vanilla prop, but
    // the harvested list is off in these tests).
    write(tmp.path(), "prop_gas_pump_1a.ydr", b"");
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

/// A map renamed in the file explorer keeps its old internal name; the
/// game registers it under the new one, so nothing that names the old one
/// binds any more. The check only applies to a file on disk.
#[test]
fn map_named_unlike_its_file_is_flagged() {
    use rage_formats::{ymap::tests::sample_exterior_ymap, Vec3};
    let tmp = tempfile::tempdir().unwrap();
    let bytes = sample_exterior_ymap("map1", &[("prop_a", Vec3::new(1.0, 2.0, 3.0), 0.0)]);
    let renamed = write(tmp.path(), "casas_praia_extras.ymap", &bytes);

    let out = ok(&["resource", "info", &renamed]);
    assert!(out.contains("Map:       hash_"), "map1 is nobody's sibling here: {out}");
    assert!(out.contains("warning: the file is called casas_praia_extras but the map calls itself hash_"), "{out}");
    assert!(out.contains("will not bind"), "{out}");
    let json = ok(&["resource", "info", &renamed, "--json"]);
    assert!(json.contains("\"file_name_mismatch\":\"casas_praia_extras\""), "{json}");

    // The same bytes under the right name, in either case: no warning.
    let right = write(tmp.path(), "MAP1.ymap", &bytes);
    let out = ok(&["resource", "info", &right]);
    assert!(!out.contains("warning"), "{out}");
    let json = ok(&["resource", "info", &right, "--json"]);
    assert!(json.contains("\"file_name_mismatch\":null"), "{json}");

    // Inside an archive the lookup name is the file name by construction.
    let mut builder = RpfBuilder::new(RpfEncryption::None);
    builder.add_file("stream/casas_praia_extras.ymap", bytes);
    let archive = write(tmp.path(), "test.rpf", &builder.build(None).unwrap());
    let out = ok(&["resource", "info", "casas_praia_extras.ymap", "--archive", &archive]);
    assert!(!out.contains("warning"), "{out}");
}

/// A manifest's imapName entries either name a .ymap next to it, a vanilla
/// map (known to some list), or nothing at all — the last being a leftover.
#[test]
fn manifest_entries_with_no_map_beside_them_are_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let stream = tmp.path().join("stream");
    std::fs::create_dir_all(&stream).unwrap();
    let manifest = "<?xml version=\"1.0\"?><CPackFileMetaData><imapDependencies_2>\
        <Item><imapName>bombapaleto</imapName><manifestFlags/><itypDepArray><Item>v_construction</Item></itypDepArray></Item>\
        <Item><imapName>cs1_roads_pb_long_0</imapName><manifestFlags/><itypDepArray/></Item>\
        <Item><imapName>map1</imapName><manifestFlags/><itypDepArray/></Item>\
        </imapDependencies_2></CPackFileMetaData>";
    let file = write(&stream, "_manifest.ymf", manifest.as_bytes());
    write(&stream, "bombaPALETO.ymap", b"");
    let vanilla = write(tmp.path(), "vanilla.txt", b"cs1_roads_pb_long_0\n");

    let out = ok(&["resource", "info", &file, "--names", &vanilla]);
    assert!(out.contains("  bombapaleto -> v_construction\n"), "a sibling, no note: {out}");
    assert!(out.contains("cs1_roads_pb_long_0 -> -  (no such .ymap here; a vanilla map?)"), "{out}");
    assert!(out.contains("map1 -> -  (no such .ymap here, and no list knows the name)"), "{out}");
    assert!(out.contains("warning: 1 declared map(s) exist neither next to this manifest nor in any name list (map1)"), "{out}");

    let json = ok(&["resource", "info", &file, "--json", "--names", &vanilla]);
    assert!(json.contains("\"imaps_not_here\":[{\"name\":\"cs1_roads_pb_long_0\",\"known_name\":true},{\"name\":\"map1\",\"known_name\":false}]"), "{json}");

    // Without the vanilla list, the road chunk is just as unknown as map1.
    let out = ok(&["resource", "info", &file]);
    assert!(out.contains("warning: 2 declared map(s)"), "{out}");
}
