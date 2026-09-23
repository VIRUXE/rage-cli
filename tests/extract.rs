//! `rage extract` against a synthetic archive; needs no game install.

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
        "`rage {}` failed with {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `update.rpf`-shaped: a few metadata files in two folders, plus a nested archive.
fn fixture(dir: &Path) -> String {
    let mut inner = RpfBuilder::new(RpfEncryption::None);
    inner.add_file("models/prop_a.ydr", b"drawable a".to_vec());
    inner.add_file("models/prop_b.ydr", b"drawable b".to_vec());
    let inner_bytes = inner.build(None).unwrap();

    let mut outer = RpfBuilder::new(RpfEncryption::None);
    outer.add_file("common/data/handling.meta", b"<CHandlingDataMgr/>".to_vec());
    outer.add_file("common/data/levels/gta5/vehicles.meta", b"<CVehicleModelInfo__InitDataList/>".to_vec());
    outer.add_file("common/data/carcols.ymt", b"PSIN".to_vec());
    outer.add_file("common/data/carvariations.ymt", b"PSIN".to_vec());
    outer.add_file("levels/inner.rpf", inner_bytes);
    let path = dir.join("update.rpf");
    std::fs::write(&path, outer.build(None).unwrap()).unwrap();
    path.to_str().unwrap().to_string()
}

fn written(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn several_paths_come_out_in_one_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = fixture(tmp.path());
    let out_dir = tmp.path().join("meta");

    let out = ok(&[
        "extract", &archive, "-o", out_dir.to_str().unwrap(),
        "common/data/handling.meta", "common/data/levels/gta5/vehicles.meta", "*.ymt",
    ]);
    assert!(out.contains("Extracting 4 files"), "{out}");
    assert_eq!(
        written(&out_dir),
        ["common/data/carcols.ymt", "common/data/carvariations.ymt", "common/data/handling.meta", "common/data/levels/gta5/vehicles.meta"]
    );
    assert_eq!(std::fs::read(out_dir.join("common/data/handling.meta")).unwrap(), b"<CHandlingDataMgr/>");
}

#[test]
fn a_repeated_or_missing_pattern_is_harmless() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = fixture(tmp.path());
    let out_dir = tmp.path().join("out");

    let out = ok(&["extract", &archive, "-o", out_dir.to_str().unwrap(), "*.ymt", "common/data/carcols.ymt", "nope.meta"]);
    assert!(out.contains("File not found: nope.meta"), "{out}");
    assert!(out.contains("Extracting 2 files"), "{out}");

    let out = ok(&["extract", &archive, "-o", out_dir.to_str().unwrap(), "nope.meta"]);
    assert!(out.contains("No files to extract"), "{out}");
}

#[test]
fn one_pattern_and_none_still_work() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = fixture(tmp.path());

    let one = tmp.path().join("one");
    ok(&["extract", &archive, "common/data/handling.meta", "-o", one.to_str().unwrap()]);
    assert_eq!(written(&one), ["common/data/handling.meta"]);

    let all = tmp.path().join("all");
    ok(&["extract", &archive, "-o", all.to_str().unwrap()]);
    assert_eq!(written(&all).len(), 5);

    let deep = tmp.path().join("deep");
    ok(&["extract", &archive, "-o", deep.to_str().unwrap(), "--recursive", "*.ydr", "*.meta"]);
    assert_eq!(
        written(&deep),
        ["common/data/handling.meta", "common/data/levels/gta5/vehicles.meta", "levels/inner.rpf/models/prop_a.ydr", "levels/inner.rpf/models/prop_b.ydr"]
    );
}
