//! `rage resource convert` against a Gen9 texture dictionary the test writes byte by byte; needs no game install.

use std::path::Path;
use std::process::{Command, Output};

use rage_formats::{build_rsc7, GRAPHICS_BASE, SYSTEM_BASE};

fn rage(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .env("RAGE_NAMES", "nowhere/names.txt")
        .output()
        .expect("failed to run the rage binary")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A Gen9 `.ytd` (resource version 5) holding one 8x8 texture called `wall_a` with two mips (BC1: 32 + 8
/// bytes), laid out as the Enhanced game writes it: a 64-byte dictionary, then its page map, hash list,
/// pointer list, the 128-byte texture header and the name; the pixels in the graphics section.
fn gen9_ytd(format: u8, dimension: u8) -> Vec<u8> {
    let mut sys = vec![0u8; 0x110];
    let put16 = |b: &mut [u8], o: usize, v: u16| b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    let put32 = |b: &mut [u8], o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    let put64 = |b: &mut [u8], o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    let va = |o: u64| SYSTEM_BASE + o;

    // dictionary
    put32(&mut sys, 0x04, 1);
    put64(&mut sys, 0x08, va(0x40));
    put32(&mut sys, 0x18, 1);
    put64(&mut sys, 0x20, va(0x60));
    put16(&mut sys, 0x28, 1);
    put16(&mut sys, 0x2A, 1);
    put64(&mut sys, 0x30, va(0x68));
    put16(&mut sys, 0x38, 1);
    put16(&mut sys, 0x3A, 1);
    // page map: one system page, one graphics page
    sys[0x48] = 1;
    sys[0x49] = 1;
    // hash list and pointer list
    put32(&mut sys, 0x60, rage_formats::rage_joaat("wall_a"));
    put64(&mut sys, 0x68, va(0x80));
    // the Gen9 texture header
    put32(&mut sys, 0x84, 1);
    put32(&mut sys, 0x88, 1);
    put32(&mut sys, 0x90, 0x0026_0208);
    put16(&mut sys, 0x98, 8);
    put16(&mut sys, 0x9A, 8);
    put16(&mut sys, 0x9C, 1);
    sys[0x9E] = dimension;
    sys[0x9F] = format;
    sys[0xA0] = 255;
    sys[0xA2] = 2;
    put64(&mut sys, 0xA8, va(0x100));
    put64(&mut sys, 0xB8, GRAPHICS_BASE);
    put32(&mut sys, 0xC0, 20);
    put32(&mut sys, 0xC4, 2);
    sys[0x100..0x107].copy_from_slice(b"wall_a\0");

    let pixels: Vec<u8> = (0..40).collect();
    build_rsc7(5, &sys, &pixels)
}

#[test]
fn converts_a_gen9_texture_dictionary_to_the_legacy_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("wall.ytd");
    std::fs::write(&input, gen9_ytd(0x47, 1)).unwrap();

    let before = text(&rage(&["resource", "info", s(&input)]));
    assert!(before.contains("version 5") && before.contains("Enhanced (Gen9)"), "{before}");
    assert!(before.contains("wall_a") && before.contains("8x8x1 DXT1 2 mip(s)"), "{before}");

    let out = tmp.path().join("out");
    let run = rage(&["resource", "convert", s(&input), "-o", s(&out)]);
    assert!(run.status.success(), "{}", text(&run));
    assert!(text(&run).contains("Converted 1, skipped 0, failed 0"), "{}", text(&run));

    let after = text(&rage(&["resource", "info", s(&out.join("wall.ytd"))]));
    assert!(after.contains("version 13") && after.contains("Edition:   Legacy"), "{after}");
    assert!(after.contains("wall_a") && after.contains("8x8x1 DXT1 2 mip(s) (40 bytes)"), "{after}");

    // the input is untouched, and converting the result again has nothing to do
    assert!(text(&rage(&["resource", "info", s(&input)])).contains("version 5"));
    let again = rage(&["resource", "convert", s(&out), "-o", s(&tmp.path().join("out2"))]);
    assert!(again.status.success(), "{}", text(&again));
    assert!(text(&again).contains("already a legacy file") && !tmp.path().join("out2").exists(), "{}", text(&again));
}

#[test]
fn a_folder_keeps_its_layout_and_dry_run_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("pack/stream/vehicles");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("wall.ytd"), gen9_ytd(0x47, 1)).unwrap();
    std::fs::write(dir.join("readme.txt"), "not a resource").unwrap();

    let out = tmp.path().join("out");
    let dry = rage(&["resource", "convert", s(&tmp.path().join("pack")), "-o", s(&out), "--dry-run"]);
    assert!(dry.status.success() && text(&dry).contains("Would convert 1"), "{}", text(&dry));
    assert!(!out.exists());

    let run = rage(&["resource", "convert", s(&tmp.path().join("pack")), "-o", s(&out)]);
    assert!(run.status.success(), "{}", text(&run));
    assert!(out.join("stream/vehicles/wall.ytd").is_file());
    assert!(!out.join("stream/vehicles/readme.txt").exists());
}

#[test]
fn refuses_a_texture_the_legacy_game_cannot_hold_and_says_why() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("hdr.ytd");
    std::fs::write(&input, gen9_ytd(0x5F, 1)).unwrap(); // BC6H
    let run = rage(&["resource", "convert", s(&input), "-o", s(&tmp.path().join("out"))]);
    assert!(!run.status.success());
    let t = text(&run);
    assert!(t.contains("FAILED") && t.contains("BC6H_UF16") && t.contains("no legacy equivalent"), "{t}");
    assert!(!tmp.path().join("out/hdr.ytd").exists());

    std::fs::write(&input, gen9_ytd(0x47, 2)).unwrap(); // a cube map
    let t = text(&rage(&["resource", "convert", s(&input), "-o", s(&tmp.path().join("out"))]));
    assert!(t.contains("cube map"), "{t}");
}

#[test]
fn only_legacy_is_a_target() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("wall.ytd");
    std::fs::write(&input, gen9_ytd(0x47, 1)).unwrap();
    let t = text(&rage(&["resource", "convert", s(&input), "-o", s(&tmp.path().join("out")), "--to", "gen9"]));
    assert!(t.contains("only `legacy` is supported"), "{t}");
}
