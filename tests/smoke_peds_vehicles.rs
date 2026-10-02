//! `rage screenshot --ped` and `--vehicle` against the retail game: the index builds its peds
//! and vehicles parts, a ped composes with the names CodeWalker's rules give its variation
//! info, and a vehicle renders high-detail with a livery swapped and a carcols paint.
//!
//! Skipped unless `GTAV_PATH` points at the game. No CodeWalker binary is run; the expected
//! names are pinned from the game's own files (`a_m_y_acult_01.ymt`/`.ydd`, `police.yft`,
//! `police.ytd`, `carvariations.ymt`, `carcols.ymt`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn rage(args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .env("RAGE_NAMES", "nowhere/names.txt")
        .output()
        .expect("failed to run rage");
    assert!(
        output.status.success(),
        "rage {args:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// The image is not all background: some pixel differs from the corner.
fn has_content(path: &Path) -> bool {
    let image = rage_formats::image::open(path).expect("a readable image").to_rgba8();
    let corner = image.get_pixel(0, 0);
    image.pixels().any(|p| p != corner)
}

#[test]
fn a_ped_composes_from_its_variations() {
    let Some(game) = std::env::var_os("GTAV_PATH").filter(|p| !p.is_empty()) else {
        eprintln!("GTAV_PATH is not set; skipping the retail ped check");
        return;
    };
    let exe = PathBuf::from(game).join("GTA5.exe");
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("ped");

    let output = rage(&["--exe", s(&exe), "screenshot", "--ped", "a_m_y_acult_01", "--views", "front,iso", "--grid", "--size", "256x256", "-o", s(&out)]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in [
        "head: head_000_r / head_diff_000_a_whi",
        "berd: none",
        "hair: hair_000_u / hair_diff_000_a_uni",
        "uppr: uppr_000_r / uppr_diff_000_a_whi",
        "lowr: lowr_000_r / lowr_diff_000_a_whi",
        "accs: accs_000_r / accs_diff_000_a_whi",
        "jbib: none",
    ] {
        assert!(stdout.contains(line), "expected {line:?} in:\n{stdout}");
    }
    assert!(stdout.contains("components"), "{stdout}");
    assert!(!stdout.contains("Missing textures"), "every default texture should resolve:\n{stdout}");
    assert!(!stderr.contains("warning: "), "{stderr}");
    for name in ["a_m_y_acult_01_front.png", "a_m_y_acult_01_iso.png", "a_m_y_acult_01_grid.png"] {
        assert!(has_content(&out.join(name)), "{name} is blank");
    }

    // A second drawable for the torso, no hair, and the slot lines follow.
    let output = rage(&["--exe", s(&exe), "screenshot", "--ped", "a_m_y_acult_01", "--component", "uppr=1:1", "--component", "hair=none", "--size", "128x128", "-o", s(&out)]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("uppr: uppr_001_r / uppr_diff_001_b_whi"), "{stdout}");
    assert!(stdout.contains("hair: none"), "{stdout}");

    // An unknown ped is refused.
    let status = Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(["--exe", s(&exe), "screenshot", "--ped", "not_a_ped_at_all", "-o", s(&out)])
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("not a ped"));
}

#[test]
fn a_vehicle_renders_high_detail_with_a_livery_and_carcols_paint() {
    let Some(game) = std::env::var_os("GTAV_PATH").filter(|p| !p.is_empty()) else {
        eprintln!("GTAV_PATH is not set; skipping the retail vehicle check");
        return;
    };
    let exe = PathBuf::from(game).join("GTA5.exe");
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("veh");

    let output = rage(&[
        "--exe", s(&exe), "screenshot", "--vehicle", "police", "--hi", "--livery", "2", "--colour-from", "carcols",
        "--views", "front,left", "--size", "256x256", "-o", s(&out),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Rendering police_hi (") && stdout.contains("police_hi.yft"), "{stdout}");
    assert!(stdout.contains("Livery 2: policenew_sign_1 -> policenew_sign_3"), "{stdout}");
    assert!(stdout.contains("Paint: ") && stdout.contains("carcols colour"), "{stdout}");
    assert!(stdout.contains("(4 wheels)"), "{stdout}");
    for name in ["police_hi_front.png", "police_hi_left.png"] {
        assert!(has_content(&out.join(name)), "{name} is blank");
    }

    // The base model and a livery the game does not ship.
    let output = rage(&["--exe", s(&exe), "screenshot", "--vehicle", "police", "--livery", "9", "--size", "128x128", "-o", s(&out)]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("Rendering police (") && !stdout.contains("police_hi"), "{stdout}");
    assert!(stderr.contains("policenew_sign_10 is not in any texture dictionary"), "{stderr}");
    assert!(out.join("police.png").is_file());

    // A colour combination the model does not have is refused, not guessed.
    let status = Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(["--exe", s(&exe), "screenshot", "--vehicle", "police", "--colour-from", "carcols:9", "-o", s(&out)])
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("past the end"));
}
