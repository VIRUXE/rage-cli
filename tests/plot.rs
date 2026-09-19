//! `rage plot` against hand-built inputs; needs no game install.

use std::path::Path;
use std::process::{Command, Output};

use rage_formats::{serialize_ynv, NavPoly, Vec3, Ynv};

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
        args.join(" "), output.status,
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_str().unwrap().to_string()
}

/// A cell 3236 with one interior polygon where the plots frame, and one
/// street polygon away from it.
fn cell() -> Ynv {
    let mut ynv = Ynv::new_cell(3236, Vec3::new(-600.0, -1200.0, 10.0), Vec3::new(-450.0, -1050.0, 70.0));
    let square = |x: f32, y: f32, z: f32| vec![Vec3::new(x, y, z), Vec3::new(x + 2.0, y, z), Vec3::new(x + 2.0, y + 2.0, z), Vec3::new(x, y + 2.0, z)];
    let mut room = NavPoly::new(square(-580.0, -1064.0, 21.0));
    room.set_interior(true);
    ynv.polys.push(room);
    let mut road = NavPoly::new(square(-500.0, -1100.0, 30.0));
    road.set_flat_ground(true);
    ynv.polys.push(road);
    ynv
}

/// The `WxH` pixel size out of a `Wrote out.png (620x540 px, region ...)` line.
fn reported_size(line: &str) -> (u32, u32) {
    let open = line.find('(').expect("no size in the stdout line");
    let size = line[open + 1..].split(" px").next().expect("no ' px' in the stdout line");
    let (w, h) = size.split_once('x').expect("size is not WxH");
    (w.parse().expect("width"), h.parse().expect("height"))
}

#[test]
fn plots_a_cell_to_png() {
    let dir = tempfile::tempdir().unwrap();
    let ynv = write(dir.path(), "cell.ynv", &serialize_ynv(&cell()).unwrap());
    let png = dir.path().join("out.png");
    let out = ok(&["plot", &ynv, "--region=-585,-1070,-570,-1055", "--scale", "20", "-o", png.to_str().unwrap()]);
    assert!(out.starts_with("Wrote"), "{out}");

    let (w, h) = reported_size(&out);
    let img = rage_formats::image::open(&png).expect("the plot should be a readable image");
    assert_eq!((img.width(), img.height()), (w, h), "{out}");
    assert!(w > 15 * 20, "the 15 m region at 20 px/m should be at least 300 px wide, got {w}");
}

#[test]
fn plots_a_cell_to_svg() {
    let dir = tempfile::tempdir().unwrap();
    let ynv = write(dir.path(), "cell.ynv", &serialize_ynv(&cell()).unwrap());
    let svg = dir.path().join("out.svg");
    ok(&["plot", &ynv, "--region=-585,-1070,-570,-1055", "--scale", "20", "-o", svg.to_str().unwrap()]);
    let text = std::fs::read_to_string(&svg).unwrap();
    assert!(text.contains("<svg"), "{text}");
    assert!(text.contains("<polygon"), "no polygons in the SVG");
    assert!(text.contains("<text"), "no labels in the SVG");
    assert!(text.contains("navmesh"), "no navmesh legend row in the SVG");
    assert!(!text.contains("<image"), "a navmesh-only plot needs no raster underlay");
}

#[test]
fn plots_a_ytyp_in_local_space() {
    let dir = tempfile::tempdir().unwrap();
    let ytyp = write(dir.path(), "int.ytyp", &rage_formats::ytyp::tests::minimal_mlo_ytyp());
    let svg = dir.path().join("out.svg");
    ok(&["plot", &ytyp, "-o", svg.to_str().unwrap()]);
    let text = std::fs::read_to_string(&svg).unwrap();
    assert!(text.contains("kitchen"), "the room name should be on the plan");
    assert!(text.contains("MLO-local"), "without a .ymap the plan is in MLO-local coordinates");
}

#[test]
fn folder_input_skips_escrowed_files() {
    let dir = tempfile::tempdir().unwrap();
    let stream = dir.path().join("res").join("stream");
    std::fs::create_dir_all(&stream).unwrap();
    let mut escrowed = b"FXAP".to_vec();
    escrowed.extend(std::iter::repeat(0u8).take(60));
    for name in ["prop_a.ydr", "prop_b.ydr", "prop_c.ydd"] {
        write(&stream, name, &escrowed);
    }
    write(&stream, "cell.ynv", &serialize_ynv(&cell()).unwrap());

    let png = dir.path().join("out.png");
    let res = dir.path().join("res");
    let out = rage(&["plot", res.to_str().unwrap(), "-o", png.to_str().unwrap()]);
    assert!(out.status.success(), "stderr:\n{}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("escrow"), "{err}");
    // One line for the folder, not one per file.
    assert_eq!(err.matches("escrow").count(), 1, "{err}");
    assert!(err.contains("skipping 3 escrow-encrypted files"), "{err}");
}

#[test]
fn rejects_unknown_layer() {
    let dir = tempfile::tempdir().unwrap();
    let ynv = write(dir.path(), "cell.ynv", &serialize_ynv(&cell()).unwrap());
    let out = rage(&["plot", &ynv, "--layers", "walls", "-o", dir.path().join("x.png").to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("rooms"), "the error should list the real layers: {err}");
}

#[test]
fn refuses_a_huge_image() {
    let dir = tempfile::tempdir().unwrap();
    let ynv = write(dir.path(), "cell.ynv", &serialize_ynv(&cell()).unwrap());
    let out = rage(&["plot", &ynv, "--scale", "5000", "--region", "0,0,100,100", "-o", dir.path().join("x.png").to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("too large"), "{err}");
}

#[test]
fn no_inputs_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = rage(&["plot", "-o", dir.path().join("x.png").to_str().unwrap()]);
    assert!(!out.status.success());
}

#[test]
fn archetype_input_without_index_explains_itself() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(["plot", "v_int_3", "-o", dir.path().join("out.png").to_str().unwrap()])
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .env_remove("GTAV_PATH")
        .output()
        .expect("failed to run the rage binary");
    assert!(!out.status.success(), "an archetype name with no game index should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("GTAV_PATH"), "the error should say how to point `rage` at the game: {err}");
    assert!(err.contains("v_int_3"), "the error should name the input: {err}");
}

#[test]
fn navmesh_plot_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let ynv = write(dir.path(), "cell.ynv", &serialize_ynv(&cell()).unwrap());
    let out = rage(&["navmesh", "plot", &ynv, "-o", dir.path().join("y.png").to_str().unwrap()]);
    assert!(!out.status.success(), "`navmesh plot` should be gone; plotting lives in `rage plot`");
}
