#![cfg(windows)]
use std::{path::Path, process::Command};
static LOCKFILE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run(args: &[&str]) -> std::process::Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/with-local-toolchain.ps1");
    Command::new("powershell.exe").args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script).args(args).output().unwrap()
}

#[test]
fn local_toolchain_has_one_formats_instance_and_restores_lockfile() {
    let _guard = LOCKFILE.lock().unwrap();
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock");
    let before = std::fs::read(&lock).unwrap();
    let output = run(&["tree", "--offline", "-i", "rage-formats", "--depth", "0"]);
    assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout).replace('\\', "/");
    assert!(stdout.contains("rage-formats v0.5.1 (") && stdout.contains("/rage-formats)"), "{stdout}");
    assert_eq!(stdout.lines().filter(|l| l.starts_with("rage-formats v")).count(), 1);
    assert!(std::fs::read(lock).unwrap() == before, "wrapper changed the caller's lockfile");
}

#[test]
fn local_toolchain_forwards_failure() {
    let _guard = LOCKFILE.lock().unwrap();
    let output = run(&["this-command-does-not-exist"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no such command"));
}
