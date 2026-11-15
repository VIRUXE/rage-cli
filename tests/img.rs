//! IMG workflows use generated data, never distributed game files.
use std::{fs, path::Path, process::{Command, Output}};

fn rage(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage")).args(args)
        .env_remove("GTAV_PATH").env_remove("RAGE_KEYS")
        .env("RAGE_NO_UPDATE_CHECK", "1").output().unwrap()
}
fn ok(args: &[&str]) -> String {
    let out = rage(args);
    assert!(out.status.success(), "{args:?}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn s(p: &Path) -> &str { p.to_str().unwrap() }
fn img2(path: &Path, name: &[u8], payload: &[u8]) {
    let mut bytes = vec![0; 4096];
    bytes[..4].copy_from_slice(b"VER2");
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
    bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
    bytes[16..16 + name.len()].copy_from_slice(name);
    bytes[2048..2048 + payload.len()].copy_from_slice(payload);
    fs::write(path, bytes).unwrap();
}

#[test]
fn img1_pair_opens() {
    let t = tempfile::tempdir().unwrap();
    let img = t.path().join("test.img");
    let dir = t.path().join("test.dir");
    fs::write(&img, vec![7; 2048]).unwrap();
    let mut toc = vec![0; 32];
    toc[4..8].copy_from_slice(&1u32.to_le_bytes());
    toc[8..16].copy_from_slice(b"test.dff");
    fs::write(&dir, toc).unwrap();
    assert!(ok(&["list", s(&img)]).contains("test.dff"));
    let other = t.path().join("different.dir");
    fs::rename(&dir, &other).unwrap();
    assert!(ok(&["--img-dir", s(&other), "list", s(&img)]).contains("test.dff"));
    assert!(!rage(&["list", s(&img)]).status.success());
}

fn create_extract(format: &str) {
    let t = tempfile::tempdir().unwrap();
    let input = t.path().join("input"); fs::create_dir(&input).unwrap();
    fs::write(input.join("test.dff"), b"test model").unwrap();
    let img = t.path().join("out.img");
    ok(&["create", s(&input), "-o", s(&img), "--format", format]);
    let info = ok(&["info", s(&img)]).to_lowercase();
    assert!(info.contains(format), "{info}");
    let output = t.path().join("extract");
    ok(&["extract", s(&img), "-o", s(&output)]);
    let data = fs::read(output.join("test.dff")).unwrap();
    assert_eq!(&data[..10], b"test model");
    assert!(data[10..].iter().all(|b| *b == 0));
    ok(&["verify", s(&img)]);
}
#[test] fn img1_create_extract() { create_extract("img1"); }
#[test] fn img2_create_extract() { create_extract("img2"); }
#[test] fn img3_create_extract() { create_extract("img3"); }

#[test]
fn names_stop_at_first_nul() {
    let t = tempfile::tempdir().unwrap(); let img = t.path().join("a.img");
    img2(&img, b"test.dff\0garbage", b"x");
    let output = t.path().join("extract");
    ok(&["extract", s(&img), "-o", s(&output)]);
    assert!(output.join("test.dff").is_file());
    assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
}

#[test]
fn rejects_truncating_duplicate_names() {
    let t = tempfile::tempdir().unwrap(); let input = t.path().join("in"); fs::create_dir(&input).unwrap();
    fs::write(input.join("a_very_long_file_name_that_will_be_truncated.dff"), b"a").unwrap();
    let output = t.path().join("out.img");
    assert!(!rage(&["create", s(&input), "-o", s(&output), "--format", "img2"]).status.success());
    assert!(!output.exists());
    fs::remove_file(input.join("a_very_long_file_name_that_will_be_truncated.dff")).unwrap();
    fs::create_dir(input.join("nested")).unwrap();
    fs::write(input.join("same.dff"), b"a").unwrap();
    fs::write(input.join("nested/SAME.dff"), b"b").unwrap();
    assert!(!rage(&["create", s(&input), "-o", s(&output), "--format", "img2"]).status.success());
    assert!(!output.exists());
}

#[test]
fn verify_invalid_archive_fails() {
    let t = tempfile::tempdir().unwrap(); let img = t.path().join("bad.img");
    fs::write(&img, b"garbage").unwrap();
    assert!(!rage(&["verify", s(&img)]).status.success());
    img2(&img, b"test.dff", b"x");
    let mut bytes = fs::read(&img).unwrap();
    bytes[8..12].copy_from_slice(&999u32.to_le_bytes()); fs::write(&img, bytes).unwrap();
    assert!(!rage(&["verify", s(&img)]).status.success());
}

#[test]
fn extract_cannot_escape_output() {
    let t = tempfile::tempdir().unwrap(); let img = t.path().join("bad.img");
    img2(&img, b"../escape.dff", b"x");
    let output = t.path().join("extract");
    assert!(!rage(&["extract", s(&img), "-o", s(&output)]).status.success());
    assert!(!t.path().join("escape.dff").exists());
}

#[cfg(windows)]
#[test]
fn extract_cannot_follow_existing_junction() {
    let t = tempfile::tempdir().unwrap();
    let img = t.path().join("bad.img");
    img2(&img, b"link/file.dff", b"overwrite");
    let output = t.path().join("extract");
    let outside = t.path().join("outside");
    fs::create_dir_all(&output).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("file.dff"), b"keep").unwrap();
    let status = std::process::Command::new("cmd").args(["/C", "mklink", "/J"])
        .arg(output.join("link")).arg(&outside).output().unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    let result = rage(&["extract", s(&img), "-o", s(&output)]);
    assert!(!result.status.success(), "extraction followed a junction");
    assert_eq!(fs::read(outside.join("file.dff")).unwrap(), b"keep");
    fs::remove_dir(output.join("link")).unwrap();
}

#[test]
fn rebuild_preserves_unmodified() {
    let t = tempfile::tempdir().unwrap(); let img = t.path().join("in.img");
    img2(&img, b"test.dff", b"unchanged\0\0");
    let extra = t.path().join("extra.txd"); fs::write(&extra, b"texture").unwrap();
    let rebuilt = t.path().join("out.img");
    ok(&["archive", "rebuild", s(&img), "-o", s(&rebuilt), "--add", s(&extra)]);
    let output = t.path().join("extract"); ok(&["extract", s(&rebuilt), "-o", s(&output)]);
    assert_eq!(fs::read(output.join("test.dff")).unwrap(), fs::read(&img).unwrap()[2048..]);
    assert_eq!(&fs::read(output.join("extra.txd")).unwrap()[..7], b"texture");
    let replacement = t.path().join("test.dff"); fs::write(&replacement, b"edited").unwrap();
    let edited = t.path().join("edited.img");
    ok(&["archive", "rebuild", s(&rebuilt), "-o", s(&edited), "--replace", s(&replacement), "--remove", "extra.txd"]);
    let out = t.path().join("edited"); ok(&["extract", s(&edited), "-o", s(&out)]);
    assert_eq!(&fs::read(out.join("test.dff")).unwrap()[..6], b"edited");
    assert!(!out.join("extra.txd").exists());
}

#[test]
fn rebuild_refuses_rpf_until_resource_metadata_can_be_preserved() {
    let t = tempfile::tempdir().unwrap();
    let input = t.path().join("input");
    fs::create_dir(&input).unwrap();
    fs::write(input.join("test.bin"), b"data").unwrap();
    let archive = t.path().join("in.rpf");
    ok(&["create", s(&input), "-o", s(&archive), "--format", "rpf"]);
    let output = t.path().join("out.rpf");
    let result = rage(&["archive", "rebuild", s(&archive), "-o", s(&output)]);
    assert!(!result.status.success(), "unimplemented RPF resource-preserving rebuild accepted");
    assert!(!output.exists());
}

#[test]
fn img3_malformed_name_table_fails_without_panic() {
    let t = tempfile::tempdir().unwrap(); let img = t.path().join("bad.img");
    let mut bytes = vec![0; 52];
    bytes[..4].copy_from_slice(&[0x52, 0x2a, 0x4e, 0xa9]);
    bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&32u32.to_le_bytes());
    bytes[16..18].copy_from_slice(&16u16.to_le_bytes());
    fs::write(&img, bytes).unwrap();
    let out = rage(&["list", s(&img)]);
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("panicked"));
}

#[test]
fn rebuild_preserves_img2_directory_metadata() {
    let t = tempfile::tempdir().unwrap(); let input = t.path().join("in.img");
    img2(&input, b"test.dff\0garbage", b"x");
    let mut bytes = fs::read(&input).unwrap();
    bytes[14..16].copy_from_slice(&7u16.to_le_bytes());
    fs::write(&input, &bytes).unwrap();
    let output = t.path().join("out.img");
    ok(&["archive", "rebuild", s(&input), "-o", s(&output)]);
    assert!(fs::read(&output).unwrap() == bytes, "rebuild changed source metadata or payload");
}

#[test]
fn rebuild_preserves_img3_compressed_resource() {
    let t = tempfile::tempdir().unwrap(); let input = t.path().join("in.img");
    let mut bytes = vec![0; 4096];
    bytes[..4].copy_from_slice(&[0x52, 0x2a, 0x4e, 0xa9]);
    bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&25u32.to_le_bytes());
    bytes[16..18].copy_from_slice(&16u16.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes()); // resource virtual size flags
    bytes[24..28].copy_from_slice(&7u32.to_le_bytes()); // opaque resource type
    bytes[28..32].copy_from_slice(&1u32.to_le_bytes());
    bytes[32..34].copy_from_slice(&1u16.to_le_bytes());
    bytes[34..36].copy_from_slice(&(0x2000u16 | (2048 - 26)).to_le_bytes());
    bytes[36..45].copy_from_slice(b"test.dff\0");
    bytes[2048..2052].copy_from_slice(b"RSC5");
    // Independently constructed zlib stream: one stored 'abc' block + Adler32.
    bytes[2060..2074].copy_from_slice(&[0x78,1,1,3,0,0xfc,0xff,b'a',b'b',b'c',2,0x4d,1,0x27]);
    fs::write(&input, &bytes).unwrap();
    let output = t.path().join("out.img");
    ok(&["archive", "rebuild", s(&input), "-o", s(&output)]);
    assert!(fs::read(&output).unwrap() == bytes, "IMG3 resource metadata/header/compression changed");
    let replacement = t.path().join("test.dff"); fs::write(&replacement, vec![1; 100]).unwrap();
    let invalid = t.path().join("invalid.img");
    assert!(!rage(&["archive", "rebuild", s(&input), "-o", s(&invalid), "--replace", s(&replacement)]).status.success());
    assert!(!invalid.exists());
}
