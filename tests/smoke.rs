//! End-to-end smoke test for the image commands against a real GTA V install.
//!
//! Skipped unless `GTAV_PATH` points at the game, since it needs both the
//! retail archives and the keys read out of GTA5.exe.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rage_formats::image;

/// Runs the built `rpf` binary and fails the test if it does not exit 0.
fn rpf(args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .output()
        .expect("failed to run the rage binary");

    assert!(
        output.status.success(),
        "`rpf {}` failed with {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    output
}

/// Checks a per-texture stdout line against the frozen format:
/// `  <name> — <w>x<h>x<d> <format> <levels> mip(s) (<bytes> bytes)`.
/// No regex crate is used: the line is hand-parsed token by token.
fn assert_per_texture_line(line: &str) {
    let rest = line
        .strip_prefix("  ")
        .unwrap_or_else(|| panic!("line missing leading two spaces: {line:?}"));
    let (name, rest) = rest
        .split_once(" — ")
        .unwrap_or_else(|| panic!("line missing ' — ' separator: {line:?}"));
    assert!(!name.is_empty(), "empty texture name in line: {line:?}");

    let tokens: Vec<&str> = rest.split_whitespace().collect();
    assert_eq!(tokens.len(), 6, "unexpected token count in line: {line:?} -> {tokens:?}");
    let (dims, format, levels, mip_word, bytes_open, bytes_close) =
        (tokens[0], tokens[1], tokens[2], tokens[3], tokens[4], tokens[5]);

    let dim_parts: Vec<&str> = dims.split('x').collect();
    assert_eq!(dim_parts.len(), 3, "dims '{dims}' is not WxHxD in line: {line:?}");
    for part in &dim_parts {
        assert!(
            !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()),
            "non-numeric dim '{part}' in line: {line:?}"
        );
    }

    assert!(!format.is_empty(), "empty format in line: {line:?}");
    assert!(
        !levels.is_empty() && levels.chars().all(|c| c.is_ascii_digit()),
        "levels '{levels}' is not numeric in line: {line:?}"
    );
    assert_eq!(mip_word, "mip(s)", "expected 'mip(s)' in line: {line:?}");

    let bytes_num = bytes_open
        .strip_prefix('(')
        .unwrap_or_else(|| panic!("expected '(<n>' in line: {line:?}"));
    assert!(
        !bytes_num.is_empty() && bytes_num.chars().all(|c| c.is_ascii_digit()),
        "byte count '{bytes_num}' is not numeric in line: {line:?}"
    );
    assert_eq!(bytes_close, "bytes)", "expected 'bytes)' in line: {line:?}");
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Decodes an image the CLI wrote, failing with the path if it is not readable.
fn decode(path: &Path) -> image::RgbaImage {
    let bytes = std::fs::read(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    image::load_from_memory(&bytes)
        .unwrap_or_else(|err| panic!("{} did not decode: {err}", path.display()))
        .to_rgba8()
}

/// Every file in `dir` with the given extension, sorted.
fn files_with_extension(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("{}: {err}", dir.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == extension))
        .collect();
    found.sort();
    found
}

/// Depth-first search for a file called `name` somewhere under `dir`.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|file| file.eq_ignore_ascii_case(name)) {
            return Some(path);
        }
    }
    None
}

#[test]
fn textures_and_screenshot_produce_readable_images() {
    let Ok(gtav) = std::env::var("GTAV_PATH") else {
        println!("GTAV_PATH not set; skipping smoke test");
        return;
    };

    let tmp = tempfile::tempdir().expect("failed to make a temp dir");
    let tex_dir = tmp.path().join("tex");
    let x64a = format!("{gtav}/x64a.rpf");

    // 1. Export a texture dictionary, capped and with a contact sheet.
    let textures_output = rpf(&[
        "textures", &x64a, "binoculars.ytd",
        "-o", tex_dir.to_str().unwrap(), "--sheet", "--max-size", "256",
    ]);

    // Pin the per-texture stdout format: "  <name> — WxHxD <format> N mip(s) (N bytes)".
    let stdout = stdout_of(&textures_output);
    let per_texture_lines: Vec<&str> =
        stdout.lines().filter(|line| line.starts_with("  ") && line.contains(" — ")).collect();
    assert!(!per_texture_lines.is_empty(), "no per-texture lines in stdout:\n{stdout}");
    for line in &per_texture_lines {
        assert_per_texture_line(line);
    }

    let sheet = tex_dir.join("binoculars_sheet.png");
    assert!(sheet.is_file(), "the contact sheet is missing");
    decode(&sheet);

    // The cap applies to each exported texture. The contact sheet is a
    // composite of them and is sized by --cell, so it is checked separately.
    let textures: Vec<PathBuf> =
        files_with_extension(&tex_dir, "png").into_iter().filter(|p| *p != sheet).collect();
    assert!(!textures.is_empty(), "no textures written to {}", tex_dir.display());

    for png in &textures {
        let image = decode(png);
        let longest = image.width().max(image.height());
        assert!(longest <= 256, "{} is {longest}px, over the 256px cap", png.display());
    }

    // 1b. `ytd` alias + `--dds` restores raw DDS output.
    let dds_dir = tmp.path().join("dds");
    rpf(&["ytd", &x64a, "binoculars.ytd", "-o", dds_dir.to_str().unwrap(), "--dds"]);
    let dds_files = files_with_extension(&dds_dir, "dds");
    assert!(!dds_files.is_empty(), "no .dds files written to {}", dds_dir.display());
    for dds in &dds_files {
        let bytes = std::fs::read(dds).unwrap_or_else(|err| panic!("{}: {err}", dds.display()));
        assert!(bytes.len() >= 4, "{} is too short to be a DDS file", dds.display());
        assert_eq!(&bytes[..4], b"DDS ", "{} does not start with the DDS magic", dds.display());
    }

    // 2. `search` sees into nested archives without extracting anything.
    let found = stdout_of(&rpf(&["search", &format!("{gtav}/x64b.rpf"), "*icons.rpf/*.ydr", "--limit", "1"]));
    assert!(
        found.trim().contains("x64b.rpf/levels/gta5/generic/icons.rpf/") && found.trim().ends_with(".ydr"),
        "unexpected search output:\n{found}"
    );

    // 3. Render a drawable. Retail archives keep drawables inside nested RPFs,
    //    so one has to come out to disk before it can be opened.
    let nested_dir = tmp.path().join("nested");
    rpf(&[
        "extract", &format!("{gtav}/x64b.rpf"), "*icons.rpf",
        "-o", nested_dir.to_str().unwrap(),
    ]);

    let icons = find_file(&nested_dir, "icons.rpf").expect("icons.rpf was not extracted");
    let icons = icons.to_str().unwrap().to_string();

    let listed = stdout_of(&rpf(&["list", &icons, "*.ydr"]));
    let drawable = listed
        .lines()
        .map(str::trim)
        .find(|line| line.to_ascii_lowercase().ends_with(".ydr"))
        .unwrap_or_else(|| panic!("no .ydr listed in icons.rpf:\n{listed}"));
    let stem = Path::new(drawable).file_stem().unwrap().to_str().unwrap().to_string();

    let shot_dir = tmp.path().join("shot");
    rpf(&[
        "screenshot", &icons, drawable, "-o", shot_dir.to_str().unwrap(),
        "--views", "iso,front", "--grid", "--size", "256x256",
    ]);

    let iso = shot_dir.join(format!("{stem}_iso.png"));
    for name in [format!("{stem}_front.png"), format!("{stem}_grid.png")] {
        decode(&shot_dir.join(name));
    }

    // A render that is nothing but background means nothing was drawn.
    let drawn = decode(&iso).pixels().filter(|pixel| pixel.0 != [230, 230, 230, 255]).count();
    assert!(drawn > 0, "{} is entirely background", iso.display());

    // 4. `resource info` on an archive entry reuses the per-texture line format.
    let info = stdout_of(&rpf(&["resource", "info", "binoculars.ytd", "--archive", &x64a]));
    assert!(info.contains("Format:    RSC7"), "no header in:\n{info}");
    let per_texture_lines: Vec<&str> =
        info.lines().filter(|line| line.starts_with("  ") && line.contains(" — ")).collect();
    assert!(!per_texture_lines.is_empty(), "no per-texture lines in:\n{info}");
    for line in &per_texture_lines {
        assert_per_texture_line(line);
    }

    // 5. ...and on a drawable extracted to disk it describes the geometry.
    let loose_dir = tmp.path().join("loose");
    rpf(&["extract", &icons, drawable, "-o", loose_dir.to_str().unwrap()]);
    let loose = find_file(&loose_dir, Path::new(drawable).file_name().unwrap().to_str().unwrap())
        .expect("the drawable was not extracted");

    let info = stdout_of(&rpf(&["resource", "info", loose.to_str().unwrap()]));
    assert!(info.contains("Drawables: 1"), "expected one drawable in:\n{info}");
    assert!(info.contains("shaders:"), "no shader table in:\n{info}");
    let triangles: usize = info
        .lines()
        .find(|line| line.trim_start().starts_with("high:"))
        .and_then(|line| line.split_whitespace().rev().nth(1)?.parse().ok())
        .unwrap_or_else(|| panic!("no high LOD line in:\n{info}"));
    assert!(triangles > 0, "high LOD has no triangles:\n{info}");

    let json = stdout_of(&rpf(&["resource", "info", loose.to_str().unwrap(), "--json"]));
    assert!(json.trim().starts_with('{') && json.contains("\"kind\":\"drawables\""), "bad json:\n{json}");

    // 6. A vehicle fragment renders with its wheels. The Festive Surprise
    //    pack is the smallest DLC with cars; its vehicle archive is nested
    //    one level down inside dlc.rpf.
    let dlc = format!("{gtav}/update/x64/dlcpacks/mpchristmas2/dlc.rpf");
    if !Path::new(&dlc).is_file() {
        println!("{dlc} not installed; skipping the fragment render");
        return;
    }
    let dlc_dir = tmp.path().join("dlc");
    rpf(&["extract", &dlc, "*xmas2vehicles.rpf", "-o", dlc_dir.to_str().unwrap()]);
    let vehicles = find_file(&dlc_dir, "xmas2vehicles.rpf").expect("xmas2vehicles.rpf was not extracted");

    let car_dir = tmp.path().join("car");
    let summary = stdout_of(&rpf(&[
        "screenshot", vehicles.to_str().unwrap(), "jester2.yft",
        "-o", car_dir.to_str().unwrap(), "--views", "iso", "--size", "256x256",
    ]));
    assert!(summary.contains("5 parts (4 wheels)"), "wheels not drawn:\n{summary}");
    let car = decode(&car_dir.join("jester2.png"));
    let drawn = car.pixels().filter(|pixel| pixel.0 != [230, 230, 230, 255]).count();
    assert!(drawn > 0, "the vehicle render is entirely background");
}
