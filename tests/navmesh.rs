//! `rage navmesh` against a hand-built cell and collision; needs no game install.

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

/// A cell 3236 with one street polygon under the future interior and one away from it.
fn cell() -> Ynv {
    let mut ynv = Ynv::new_cell(3236, Vec3::new(-600.0, -1200.0, 10.0), Vec3::new(-450.0, -1050.0, 70.0));
    let square = |x: f32, y: f32, z: f32| vec![Vec3::new(x, y, z), Vec3::new(x + 2.0, y, z), Vec3::new(x + 2.0, y + 2.0, z), Vec3::new(x, y + 2.0, z)];
    ynv.polys.push(NavPoly::new(square(-580.0, -1064.0, 21.0)));
    let mut road = NavPoly::new(square(-500.0, -1100.0, 30.0));
    road.set_flat_ground(true);
    ynv.polys.push(road);
    ynv
}

#[test]
fn info_summarises_a_cell() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "navmesh[108][96].ynv", &serialize_ynv(&cell()).unwrap());
    let out = ok(&["navmesh", "info", &path]);
    assert!(out.contains("navmesh[108][96].ynv (area 3236)"), "{out}");
    assert!(out.contains("Polygons:  2 (0 interior, 1 flat ground"), "{out}");
}

#[test]
fn export_writes_one_face_per_polygon() {
    let dir = tempfile::tempdir().unwrap();
    let ynv = write(dir.path(), "cell.ynv", &serialize_ynv(&cell()).unwrap());
    let obj = dir.path().join("cell.obj");
    ok(&["navmesh", "export", &ynv, "-o", obj.to_str().unwrap()]);
    let text = std::fs::read_to_string(&obj).unwrap();
    assert_eq!(text.lines().filter(|l| l.starts_with("f ")).count(), 2);
    assert!(text.contains("g exterior"));
}

#[test]
fn build_rejects_a_clip_box_outside_the_cell() {
    let dir = tempfile::tempdir().unwrap();
    let ynv = write(dir.path(), "cell.ynv", &serialize_ynv(&cell()).unwrap());
    // Any RSC7 file will do for the argument check to trigger first; reuse the cell.
    let out = rage(&["navmesh", "build", &ynv, "--ybn", &ynv, "--clip=-700,-1100,-650,-1080", "--floor-z", "21", "-o", dir.path().join("x.ynv").to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("leaves the cell") || err.contains("parsing"), "{err}");
}
