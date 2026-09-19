//! `rpf search` against synthetic nested archives; needs no game install.

use std::path::Path;
use std::process::{Command, Output};

use rpf_archive::{rage_joaat, RpfBuilder, RpfEncryption};

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

fn lines(s: &str) -> Vec<String> {
    s.lines().map(str::to_string).collect()
}

/// Writes `outer.rpf` (embedding `inner.rpf`) and a sibling `other.rpf` into `dir`.
fn fixture(dir: &Path) -> (String, String) {
    let mut inner = RpfBuilder::new(RpfEncryption::None);
    inner.add_file("models/prop_a.ydr", b"not really a drawable".to_vec());
    inner.add_file("models/prop_b.ydr", b"another one".to_vec());
    inner.add_file("data/readme.txt", b"the needle is in here".to_vec());
    let inner_bytes = inner.build(None).unwrap();

    let mut outer = RpfBuilder::new(RpfEncryption::None);
    outer.add_file("top.meta", b"<root>NEEDLE upper</root>".to_vec());
    outer.add_file("levels/inner.rpf", inner_bytes);
    let outer_path = dir.join("outer.rpf");
    std::fs::write(&outer_path, outer.build(None).unwrap()).unwrap();

    let mut other = RpfBuilder::new(RpfEncryption::None);
    other.add_file("x/prop_c.ydr", vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00]);
    let other_path = dir.join("sub").join("other.rpf");
    std::fs::create_dir_all(other_path.parent().unwrap()).unwrap();
    std::fs::write(&other_path, other.build(None).unwrap()).unwrap();

    (outer_path.to_str().unwrap().to_string(), other_path.to_str().unwrap().to_string())
}

#[test]
fn name_pattern_descends_into_nested_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let (outer, _) = fixture(tmp.path());

    let out = ok(&["search", &outer, "*.ydr"]);
    let got = lines(&out);
    assert_eq!(got.len(), 2, "{out}");
    assert!(got[0].ends_with("outer.rpf/levels/inner.rpf/models/prop_a.ydr"), "{out}");
    assert!(got[0].starts_with(&outer), "{out}");

    let out = ok(&["search", &outer, "prop_b.ydr"]);
    assert_eq!(lines(&out).len(), 1, "{out}");

    let out = ok(&["search", &outer, "*inner.rpf/*"]);
    assert_eq!(lines(&out).len(), 3, "{out}");

    // A wildcard-free pattern is a substring match, so the archive and everything under it.
    let out = ok(&["search", &outer, "inner.rpf"]);
    let got = lines(&out);
    assert_eq!(got.len(), 4, "{out}");
    assert!(got[0].ends_with("outer.rpf/levels/inner.rpf"), "{out}");
}

#[test]
fn content_hex_and_hash_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let (outer, other) = fixture(tmp.path());

    let out = ok(&["search", &outer, "--content", "needle"]);
    assert_eq!(lines(&out), vec![format!("{outer}:outer.rpf/levels/inner.rpf/data/readme.txt")]);

    let out = ok(&["search", &outer, "--content", "needle", "-i"]);
    assert_eq!(lines(&out).len(), 2, "{out}");

    let out = ok(&["search", &outer, "*.ydr", "--content", "needle"]);
    assert_eq!(out.trim(), "No files found");

    let out = ok(&["search", &other, "--hex", "DE AD BE EF", "-d"]);
    assert!(out.contains("prop_c.ydr"), "{out}");
    assert!(out.lines().last().unwrap().trim_end().ends_with(" 0"), "offset column: {out}");

    let hash = format!("0x{:08X}", rage_joaat("prop_a"));
    let out = ok(&["search", &outer, "--hash", &hash]);
    assert_eq!(lines(&out).len(), 1, "{out}");
    assert!(out.contains("prop_a.ydr"), "{out}");

    let decimal = rage_joaat("prop_a.ydr").to_string();
    let out = ok(&["search", &outer, "--hash", &decimal]);
    assert!(out.contains("prop_a.ydr"), "{out}");
}

#[test]
fn json_and_directory_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let (outer, _) = fixture(tmp.path());
    let dir = tmp.path().to_str().unwrap();

    let out = ok(&["search", dir, "*.ydr"]);
    assert_eq!(lines(&out).len(), 3, "{out}");
    assert!(out.contains("other.rpf/x/prop_c.ydr"), "{out}");

    let out = ok(&["search", &outer, "readme.txt", "--json", "--content", "needle"]);
    let trimmed = out.trim();
    assert!(trimmed.starts_with('[') && trimmed.ends_with(']'), "{out}");
    for key in ["\"archive\":", "\"path\":\"outer.rpf/levels/inner.rpf/data/readme.txt\"",
                "\"name\":\"readme.txt\"", "\"kind\":\"binary\"", "\"hash\":\"0x", "\"stem_hash\":\"0x", "\"offset\":4"] {
        assert!(trimmed.contains(key), "missing {key} in {out}");
    }

    let out = ok(&["search", &outer, "nothing-matches-this", "--json"]);
    assert_eq!(out.trim(), "[]");

    let out = ok(&["search", dir, "*.ydr", "--limit", "1"]);
    assert_eq!(lines(&out).len(), 1, "{out}");
}

#[test]
fn a_filter_is_required() {
    let tmp = tempfile::tempdir().unwrap();
    let (outer, _) = fixture(tmp.path());
    let output = rpf(&["search", &outer]);
    assert!(!output.status.success());
}
