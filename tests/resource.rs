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
    assert!(out.contains("not a drawable or texture dictionary"), "no summary:\n{out}");
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
