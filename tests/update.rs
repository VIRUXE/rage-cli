//! `rpf update` CLI wiring — help output and argument parsing only. Nothing
//! here may call the GitHub API: `update check`/`update install` are live
//! network calls and are exercised manually, not in the test suite.

use std::process::{Command, Output};

fn rpf(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .output()
        .expect("failed to run the rage binary")
}

#[test]
fn update_help_lists_both_subcommands() {
    let output = rpf(&["update", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("check"), "{stdout}");
    assert!(stdout.contains("install"), "{stdout}");
}

#[test]
fn update_check_help_mentions_json() {
    let output = rpf(&["update", "check", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--json"), "{stdout}");
}

#[test]
fn update_install_help_mentions_flags() {
    let output = rpf(&["update", "install", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--yes"), "{stdout}");
    assert!(stdout.contains("--force"), "{stdout}");
}

#[test]
fn a_normal_command_never_prints_an_update_note_on_a_piped_run() {
    // stderr is piped by Command::output(), so the daily check must never
    // fire at all here regardless of RAGE_NO_UPDATE_CHECK.
    let output = rpf(&["--version"]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("is available"), "{stderr}");
}
