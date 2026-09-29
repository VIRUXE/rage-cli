//! `rage paths` (and `resource info|dump|build` on a `.ynd`) against a
//! hand-built cell; needs no game install.

use std::path::Path;
use std::process::{Command, Output};

use rage_formats::{parse_ynd, serialize_ynd, Heightmap, NodeJunctionRef, NodeSpecial, PathJunction, PathLink, Vec2, Vec3, Ynd};

fn rage(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .output()
        .expect("failed to run the rage binary")
}

fn ok(args: &[&str]) -> (String, String) {
    let output = rage(args);
    assert!(
        output.status.success(),
        "`rage {}` failed with {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "), output.status,
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr),
    );
    (String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned())
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_str().unwrap().to_string()
}

/// Cell 489 (9,15): two road nodes joined both ways, a ped crossing node
/// off one of them, a link into cell 490, and a junction on node 0.
fn cell() -> Ynd {
    let mut ynd = Ynd::new();
    let a = ynd.add_node(489, Vec3::new(-3500.0, -400.0, 20.0));
    a.street_name = 0x9B01_C923;
    a.set_is_junction(true);
    let mut ab = PathLink::to(489, 1);
    ab.set_lane_count_forward(2);
    ab.set_lane_count_backward(2);
    ab.length = 40;
    a.links.push(ab);
    let b = ynd.add_node(489, Vec3::new(-3460.0, -400.0, 20.0));
    b.street_name = 0x9B01_C923;
    let mut ba = PathLink::to(489, 0);
    ba.set_lane_count_forward(2);
    ba.set_lane_count_backward(2);
    ba.length = 40;
    b.links.push(ba);
    b.links.push(PathLink::to(489, 2));
    b.links.push(PathLink::to(490, 3));
    let c = ynd.add_node(489, Vec3::new(-3460.0, -390.0, 20.0));
    c.set_special(NodeSpecial::PedNodeRoadCrossing);
    c.links.push(PathLink::to(489, 1));
    ynd.junctions.push(PathJunction {
        position: Vec2::new(-3504.0, -404.0),
        min_z: 19.0,
        max_z: 21.0,
        heightmap: Heightmap { width: 4, height: 4, values: (0..16).map(|i| i * 16).collect() },
    });
    ynd.junction_refs.push(NodeJunctionRef { area_id: 489, node_id: 0, junction_id: 0, unk0: 0 });
    ynd.vehicle_node_count = 2;
    ynd.ped_node_count = 1;
    ynd
}

#[test]
fn info_summarises_a_cell() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "nodes489.ynd", &serialize_ynd(&cell()).unwrap());
    let (out, _) = ok(&["paths", "info", &path]);
    assert!(out.contains("Cell:      nodes489.ynd (area 489, cell 9,15), x -3584..-3072, y -512..0"), "{out}");

    // An island cell: its links name the cell without the flag, so they count as inside.
    let mut island = cell();
    for n in &mut island.nodes {
        n.area_id = 489 + 1024;
    }
    let island_path = write(dir.path(), "nodes1513.ynd", &serialize_ynd(&island).unwrap());
    let (out, _) = ok(&["paths", "info", &island_path]);
    assert!(out.contains("Cell:      nodes1513.ynd (area 1513: Cayo Perico over cell 9,15, area 489 + 1024), x -3584..-3072"), "{out}");
    assert!(out.contains("Links:     5 (4 inside the cell, 1 into other cells;"), "{out}");
    assert!(out.contains("Nodes:     3 (2 vehicle, 1 ped; 1 junctions"), "{out}");
    assert!(out.contains("Links:     5 (4 inside the cell, 1 into other cells;"), "{out}");
    assert!(out.contains("2 two-way)"), "{out}");
    assert!(out.contains("Junctions: 1 (1 refs, 16 heightmap bytes)"), "{out}");
    assert!(out.contains("Special:   PedNodeRoadCrossing 1"), "{out}");
    assert!(out.contains("Streets:   hash_9B01C923 (2)"), "{out}");
    assert!(out.contains("Adjacent:  490"), "{out}");

    // `resource info` gives the same summary under the RSC7 header.
    let (out, _) = ok(&["resource", "info", &path]);
    assert!(out.contains("Format:    RSC7  version 1"), "{out}");
    assert!(out.contains("Nodes:     3 (2 vehicle, 1 ped;"), "{out}");
    let (out, _) = ok(&["resource", "info", "--json", &path]);
    assert!(out.contains("\"paths\":{\"area\":489,\"nodes\":3,\"vehicleNodes\":2,\"pedNodes\":1,\"links\":5,\"junctions\":1}"), "{out}");
}

#[test]
fn export_writes_points_lines_and_junction_meshes() {
    let dir = tempfile::tempdir().unwrap();
    let ynd = write(dir.path(), "nodes489.ynd", &serialize_ynd(&cell()).unwrap());
    let obj = dir.path().join("cell.obj");
    let (out, _) = ok(&["paths", "export", &ynd, "-o", obj.to_str().unwrap()]);
    assert!(out.contains("3 nodes, 4 links (1 into other cells left out) and 1 junction heightmaps"), "{out}");
    let text = std::fs::read_to_string(&obj).unwrap();
    assert_eq!(text.lines().filter(|l| l.starts_with("p ")).count(), 3);
    assert_eq!(text.lines().filter(|l| l.starts_with("l ")).count(), 4);
    assert!(text.contains("g nodes_ped\np 3\n"), "{text}");
    assert!(text.contains("g links_road\nl 1 2\nl 2 1\n"), "{text}");
    assert!(text.contains("g links_ped\nl 2 3\nl 3 2\n"), "{text}");
    // A 4x4 heightmap is 16 vertices and 9 cells of two triangles.
    assert!(text.contains("g junctions\n"), "{text}");
    assert_eq!(text.lines().filter(|l| l.starts_with("f ")).count(), 18);
    assert_eq!(text.lines().filter(|l| l.starts_with("v ")).count(), 3 + 16);
}

#[test]
fn rewrite_and_the_xml_round_trip_keep_the_cell() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = serialize_ynd(&cell()).unwrap();
    let ynd = write(dir.path(), "nodes489.ynd", &bytes);
    let again = dir.path().join("again.ynd");
    let (out, _) = ok(&["paths", "rewrite", &ynd, "-o", again.to_str().unwrap()]);
    assert!(out.starts_with("Rewrote 3 nodes:"), "{out}");
    assert_eq!(std::fs::read(&again).unwrap(), bytes, "a rewrite is byte-identical");

    let xml = dir.path().join("nodes489.ynd.xml");
    ok(&["resource", "dump", &ynd, "-o", xml.to_str().unwrap()]);
    let text = std::fs::read_to_string(&xml).unwrap();
    assert!(text.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<NodeDictionary>\n  <VehicleNodeCount value=\"2\" />"), "{text}");
    assert!(text.contains("<StreetName>hash_9B01C923</StreetName>"), "{text}");
    assert!(text.contains("<Heightmap>\n        00 10 20 30\n"), "{text}");

    let built = dir.path().join("built.ynd");
    let (_, err) = ok(&["resource", "build", xml.to_str().unwrap(), "-o", built.to_str().unwrap()]);
    assert!(err.contains("RSC7 path nodes from"), "{err}");
    assert!(err.contains("3 nodes, 5 links, 1 junctions"), "{err}");
    assert_eq!(parse_ynd(&std::fs::read(&built).unwrap()).unwrap(), cell());
}

#[test]
fn cell_needs_the_game_and_a_position() {
    let out = rage(&["paths", "cell"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--at X,Y, --index CX,CY or --area N"), "{err}");
    let out = rage(&["paths", "cell", "--area", "489"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("GTAV_PATH"), "{err}");
}
