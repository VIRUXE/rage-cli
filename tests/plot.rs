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

/// Two `.ytyp`s each declaring an interior, and no `.ymap` to say which one
/// was meant: the plan is of the first, and both stderr and the page's
/// caption say so. The count is of `MloDef`s loaded, not of distinct
/// archetype hashes — two copies of the same interior are still two
/// declarations, and the tool cannot tell which the caller meant either.
#[test]
fn several_interiors_and_no_ymap_warns_which_one_is_drawn() {
    let dir = tempfile::tempdir().unwrap();
    let ytyp = rage_formats::ytyp::tests::minimal_mlo_ytyp();
    let first = write(dir.path(), "int_a.ytyp", &ytyp);
    let second = write(dir.path(), "int_b.ytyp", &ytyp);

    let svg = dir.path().join("out.svg");
    let out = rage(&["plot", &first, &second, "-o", svg.to_str().unwrap()]);
    assert!(out.status.success(), "stderr:
{}", String::from_utf8_lossy(&out.stderr));

    let err = String::from_utf8_lossy(&out.stderr);
    let expected = "2 interiors loaded, drawing";
    assert!(err.contains("warning: "), "the multi-interior note belongs on stderr: {err}");
    assert!(err.contains(expected), "{err}");
    assert!(err.contains("give the .ymap to pick one"), "{err}");

    let text = std::fs::read_to_string(&svg).unwrap();
    assert!(text.contains(expected), "the caption should carry the same note: {text}");
}

/// One interior and no `.ymap` needs no such warning: nothing was chosen.
#[test]
fn a_single_interior_is_not_warned_about() {
    let dir = tempfile::tempdir().unwrap();
    let ytyp = write(dir.path(), "int.ytyp", &rage_formats::ytyp::tests::minimal_mlo_ytyp());
    let svg = dir.path().join("out.svg");
    let out = rage(&["plot", &ytyp, "-o", svg.to_str().unwrap()]);
    assert!(out.status.success(), "stderr:
{}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("interiors loaded"), "{err}");
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
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("`navmesh plot` was replaced by `rage plot`"), "an old command line deserves a pointer: {err}");
    assert!(err.contains("rage plot --help"), "{err}");
}

#[test]
fn a_mistyped_path_is_not_looked_up_as_an_archetype() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("stream").join("ybn").join("interor.ybn");
    let out = rage(&["plot", missing.to_str().unwrap(), "-o", dir.path().join("x.png").to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no such file or folder"), "{err}");
    assert!(err.contains("interor.ybn"), "the error should name the input: {err}");
    assert!(!err.contains("index"), "a path typo must not send `plot` off to build the game index: {err}");

    // A bare name with no separator and no resource extension is still an
    // archetype, and still says how to resolve one.
    let out = rage(&["plot", "v_int_3", "-o", dir.path().join("x.png").to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("GTAV_PATH"));
}

#[test]
fn a_file_reached_twice_is_loaded_once() {
    let dir = tempfile::tempdir().unwrap();
    let stream = dir.path().join("res").join("stream");
    std::fs::create_dir_all(&stream).unwrap();
    let ynv = write(&stream, "cell.ynv", &serialize_ynv(&cell()).unwrap());
    let res = dir.path().join("res");
    let png = dir.path().join("out.png");

    let alone = ok(&["plot", &ynv, "-o", png.to_str().unwrap()]);
    let polys = alone.split_once("; ").expect("counts in the stdout line").1.to_string();

    // The folder walk finds the same file the flag names; it must not be
    // drawn twice.
    let both = ok(&["plot", res.to_str().unwrap(), "--ybn", &ynv, "-o", png.to_str().unwrap()]);
    assert_eq!(both.split_once("; ").unwrap().1, polys, "the cell was counted twice\n{both}");

    // Same file given positionally as well as inside the folder.
    let twice = ok(&["plot", res.to_str().unwrap(), &ynv, "-o", png.to_str().unwrap()]);
    assert_eq!(twice.split_once("; ").unwrap().1, polys, "the cell was counted twice\n{twice}");
}

/// An exterior map (plain entities, no interior) draws its entities where
/// they stand, with the model of any archetype found in the folder placed
/// at each of them — and never once more at the origin.
#[test]
fn plots_an_exterior_map_with_folder_props() {
    use rage_formats::{build_rsc7, rage_joaat, ydd::tests::minimal_ydr_sections, ymap::tests::sample_exterior_ymap};
    let dir = tempfile::tempdir().unwrap();
    let stream = dir.path().join("stream");
    std::fs::create_dir(&stream).unwrap();
    let ymap = sample_exterior_ymap("paleto_props", &[("test_drawable", Vec3::new(1000.0, 2000.0, 30.0), 0.0), ("test_drawable", Vec3::new(1010.0, 2000.0, 30.0), 90.0), ("prop_nowhere", Vec3::new(1005.0, 2005.0, 30.0), 0.0)]);
    write(&stream, "paleto_props.ymap", &ymap);
    let (system, graphics) = minimal_ydr_sections(false);
    write(&stream, "test_drawable.ydr", &build_rsc7(165, &system, &graphics));
    let _ = rage_joaat("test_drawable");

    let svg = dir.path().join("out.svg");
    let out = ok(&["plot", dir.path().to_str().unwrap(), "--labels", "-o", svg.to_str().unwrap()]);
    assert!(out.contains("3 entities"), "{out}");
    assert!(out.contains("drawable tris"), "the folder model should be placed at its entities: {out}");
    let text = std::fs::read_to_string(&svg).unwrap();
    assert!(text.contains("3 entities from 1 ymap"), "{text}");
    assert!(text.contains("props: 1 from folder, 1 unresolved"), "{text}");
    assert!(text.contains("test_drawable"), "entity labels should name the archetype: {text}");
    assert!(!text.contains("MLO-local"), "an exterior map is in world space already: {text}");

    // The region framed is around the entities, not the model's own
    // (1..9) coordinates near the origin.
    let region_line = out.lines().find(|l| l.contains("region")).unwrap();
    assert!(region_line.contains("99") || region_line.contains("100"), "{region_line}");
}

/// Without a model to hand, an entity is still a mark; `--no-props` keeps
/// it that way even when a model is available.
#[test]
fn exterior_entities_are_marks_without_models() {
    use rage_formats::ymap::tests::sample_exterior_ymap;
    let dir = tempfile::tempdir().unwrap();
    let ymap = write(dir.path(), "lonely.ymap", &sample_exterior_ymap("lonely", &[("prop_a", Vec3::new(10.0, 20.0, 30.0), 0.0)]));
    let png = dir.path().join("out.png");
    let out = ok(&["plot", &ymap, "--no-props", "-o", png.to_str().unwrap()]);
    assert!(out.contains("1 entities"), "{out}");
    assert!(out.contains("0 drawable tris"), "{out}");
    assert!(png.is_file());
}
