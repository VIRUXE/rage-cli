//! `rage textures encode` / `build` and loose-file export; needs no game install.

use std::path::Path;
use std::process::{Command, Output};

use rage_formats::image::{Rgba, RgbaImage};
use rage_formats::{parse_dds, parse_ytd, TextureFormat};

fn rage(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .output()
        .expect("failed to run the rage binary")
}

fn ok(args: &[&str]) -> String {
    let output = rage(args);
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

fn fails(args: &[&str]) -> String {
    let output = rage(args);
    assert!(!output.status.success(), "`rage {}` unexpectedly succeeded", args.join(" "));
    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A gradient with an optional translucent corner.
fn write_png(dir: &Path, name: &str, w: u32, h: u32, alpha: bool) -> std::path::PathBuf {
    let img = RgbaImage::from_fn(w, h, |x, y| {
        let a = if alpha && x < w / 2 && y < h / 2 { 128 } else { 255 };
        Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 90, a])
    });
    let path = dir.join(format!("{name}.png"));
    img.save(&path).unwrap();
    path
}

#[test]
fn encode_picks_formats_and_full_mip_chains() {
    let tmp = tempfile::tempdir().unwrap();
    let opaque = write_png(tmp.path(), "wall_d", 64, 32, false);
    let alpha = write_png(tmp.path(), "glass", 16, 16, true);
    let normal = write_png(tmp.path(), "wall_n", 32, 32, false);
    let out = tmp.path().join("dds");

    let stdout = ok(&["textures", "encode", s(&opaque), s(&alpha), s(&normal), "-o", s(&out)]);
    assert!(stdout.contains("  wall_d — 64x32x1 DXT1 4 mip(s) (1360 bytes)"), "{stdout}");
    assert!(stdout.contains("  glass — 16x16x1 DXT5 3 mip(s) (336 bytes)"), "{stdout}");
    assert!(stdout.contains("  wall_n — 32x32x1 ATI2 4 mip(s) (1360 bytes)"), "{stdout}");
    assert!(stdout.contains("Encoded 3 image(s)"), "{stdout}");

    let dds = parse_dds(&std::fs::read(out.join("wall_d.dds")).unwrap()).unwrap();
    assert_eq!((dds.width, dds.height, dds.format, dds.levels), (64, 32, TextureFormat::DXT1, 4));
    let dds = parse_dds(&std::fs::read(out.join("wall_n.dds")).unwrap()).unwrap();
    assert_eq!(dds.format, TextureFormat::ATI2);
}

#[test]
fn encode_honours_format_mips_and_a_single_file_output() {
    let tmp = tempfile::tempdir().unwrap();
    let png = write_png(tmp.path(), "lights", 512, 256, false);
    let out = tmp.path().join("kanjo_lights.dds");

    let stdout = ok(&["ytd", "encode", s(&png), "-o", s(&out), "--format", "bc7", "--mips", "3"]);
    assert!(stdout.contains("lights — 512x256x1 BC7 3 mip(s)"), "{stdout}");
    let dds = parse_dds(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!((dds.format, dds.levels, dds.pixel_data.len()), (TextureFormat::BC7, 3, 131072 + 32768 + 8192));

    // The full chain stops at 4x4, as the game's textures do.
    let stdout = ok(&["ytd", "encode", s(&png), "-o", s(&tmp.path().join("full.dds")), "--format", "bc3"]);
    assert!(stdout.contains("DXT5 7 mip(s)"), "{stdout}");

    // A directory input converts everything inside; a bad size is refused.
    let odd = write_png(tmp.path(), "odd", 6, 6, false);
    let err = fails(&["ytd", "encode", s(&odd)]);
    assert!(err.contains("multiples of 4"), "{err}");
    let err = fails(&["ytd", "encode", s(&png), s(&odd), "-o", s(&out)]);
    assert!(err.contains("give a directory"), "{err}");
}

#[test]
fn build_makes_a_dictionary_that_reads_back_and_merges_with_from() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir(&src).unwrap();
    write_png(&src, "civic_body", 32, 32, false);
    write_png(&src, "civic_lights", 16, 8, true);
    ok(&["ytd", "encode", s(&src.join("civic_body.png")), "-o", s(&src.join("civic_body.dds")), "--format", "bc7"]);
    std::fs::remove_file(src.join("civic_body.png")).unwrap();

    let ytd = tmp.path().join("civic.ytd");
    let stdout = ok(&["ytd", "build", s(&src), "-o", s(&ytd)]);
    assert!(stdout.contains("civic_body — 32x32x1 BC7 4 mip(s)"), "{stdout}");
    assert!(stdout.contains("civic_lights — 16x8x1 DXT5 2 mip(s)"), "{stdout}");
    assert!(stdout.contains("with 2 texture(s)") && stdout.contains("2 added, 0 replaced, 0 kept"), "{stdout}");

    let textures = parse_ytd(&std::fs::read(&ytd).unwrap()).unwrap();
    let names: Vec<&str> = textures.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(textures.len(), 2);
    assert!(names.contains(&"civic_body") && names.contains(&"civic_lights"), "{names:?}");
    let body = textures.iter().find(|t| t.name == "civic_body").unwrap();
    assert_eq!((body.format, body.levels), (TextureFormat::BC7, 4));
    assert_eq!(body.name_hash, rage_formats::rage_joaat("civic_body"));

    // The loose dictionary exports on its own, and `--dds` round-trips the
    // DDS that went in.
    let back = tmp.path().join("back");
    let stdout = ok(&["ytd", s(&ytd), "-o", s(&back), "--dds"]);
    assert!(stdout.contains("Exported 2 texture(s)"), "{stdout}");
    assert_eq!(std::fs::read(back.join("civic_body.dds")).unwrap(), std::fs::read(src.join("civic_body.dds")).unwrap());

    // --from: replace one, add one, keep one.
    let more = tmp.path().join("more");
    std::fs::create_dir(&more).unwrap();
    write_png(&more, "civic_lights", 64, 64, false);
    write_png(&more, "civic_wheel", 8, 8, false);
    let merged = tmp.path().join("civic2.ytd");
    let stdout = ok(&["ytd", "build", s(&more), "--from", s(&ytd), "-o", s(&merged), "--format", "bc1"]);
    assert!(stdout.contains("1 added, 1 replaced, 1 kept"), "{stdout}");
    let textures = parse_ytd(&std::fs::read(&merged).unwrap()).unwrap();
    assert_eq!(textures.len(), 3);
    let lights = textures.iter().find(|t| t.name == "civic_lights").unwrap();
    assert_eq!((lights.width, lights.format), (64, TextureFormat::DXT1));
    assert!(textures.iter().any(|t| t.name == "civic_wheel"));
    assert!(textures.iter().any(|t| t.name == "civic_body" && t.format == TextureFormat::BC7));

    // resource info understands the result too.
    let info = ok(&["resource", "info", s(&merged)]);
    assert!(info.contains("version 13") && info.contains("Textures:  3"), "{info}");
}

#[test]
fn build_refuses_duplicate_names_and_non_textures() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    write_png(&a, "same", 8, 8, false);
    write_png(&b, "Same", 8, 8, false);
    let out = tmp.path().join("x.ytd");
    // Same name from two files: the later one replaces the earlier, no error.
    let stdout = ok(&["ytd", "build", s(&a), s(&b), "-o", s(&out)]);
    assert!(stdout.contains("1 added, 1 replaced"), "{stdout}");

    std::fs::write(tmp.path().join("notes.txt"), "hi").unwrap();
    let err = fails(&["ytd", "build", s(&tmp.path().join("notes.txt")), "-o", s(&out)]);
    assert!(err.contains("not a DDS or image file"), "{err}");
    let err = fails(&["ytd", "build", s(&tmp.path().join("missing")), "-o", s(&out)]);
    assert!(err.contains("no such file"), "{err}");
}

#[test]
fn a_non_resource_positional_is_explained() {
    let tmp = tempfile::tempdir().unwrap();
    let txt = tmp.path().join("readme.txt");
    std::fs::write(&txt, "x").unwrap();
    let err = fails(&["textures", s(&txt)]);
    assert!(err.contains("not a loose .ytd/.ydr/.ydd/.yft"), "{err}");
}

#[test]
fn textures_whose_names_sanitise_alike_get_distinct_files() {
    use rage_formats::{serialize_ytd, YtdTexture};
    let tex = |name: &str| YtdTexture {
        name: name.into(), name_hash: 0, width: 4, height: 4, depth: 1, format: TextureFormat::DXT1, levels: 1, stride: 2,
        pixel_data: vec![0; 8],
    };
    let tmp = tempfile::tempdir().unwrap();
    let ytd = tmp.path().join("car.ytd");
    std::fs::write(&ytd, serialize_ytd(&[tex("mesh_spec"), tex("mesh spec")]).unwrap()).unwrap();
    let out = tmp.path().join("out");
    let output = rage(&["ytd", s(&ytd), "--dds", "-o", s(&out)]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is written as mesh_spec~2"), "{stderr}");
    let mut files: Vec<String> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    files.sort();
    assert_eq!(files, ["mesh_spec.dds", "mesh_spec~2.dds"]);
}
