// `rage paths`: inspect, fetch, export and rewrite path node cells (.ynd).

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rage_formats::{
    parse_ynd, serialize_ynd, ynd_area_id, ynd_area_id_from_file_name, ynd_cell_bounds, ynd_cell_file_name,
    ynd_cell_for_position, ynd_cell_of_area, ynd_is_island_area, ynd_same_cell, NameTable, PathNode, Ynd,
    YND_ISLAND_FLAG,
};

use crate::resources::{find_in_game, load_resource_bytes};
use crate::rpf::GtaKeys;
use crate::utils::parse_pair;

#[derive(clap::Args)]
pub struct PathsArgs {
    #[command(subcommand)]
    pub command: PathsCommand,
}

#[derive(clap::Subcommand)]
pub enum PathsCommand {
    /// Summarise a .ynd: cell, node/link/junction counts, flags, streets, adjacent cells
    Info(InfoArgs),
    /// Pull a grid cell's path nodes out of the game, from the archive that loads last
    Cell(CellArgs),
    /// Write a .ynd's nodes and links as a Wavefront OBJ (points, lines and junction heightmaps)
    Export(ExportArgs),
    /// Parse a .ynd and write it back out unchanged (checks the writer against the game)
    Rewrite(ExportArgs),
}

#[derive(clap::Args)]
pub struct InfoArgs {
    /// A loose .ynd on disk, or (with --archive) a name inside the archive
    pub file: String,
    /// Look FILE up inside this RPF archive instead of on disk
    #[arg(short, long, value_name = "RPF")]
    pub archive: Option<PathBuf>,
    /// Extra name lists (one name per line) for printing street names; repeatable
    #[arg(long, value_name = "FILE")]
    pub names: Vec<PathBuf>,
}

#[derive(clap::Args)]
pub struct CellArgs {
    /// World position "X,Y" inside the cell
    #[arg(long, value_name = "X,Y", conflicts_with_all = ["index", "area"])]
    pub at: Option<String>,
    /// Cell index "CX,CY" (0..31 each)
    #[arg(long, value_name = "CX,CY", conflicts_with = "area")]
    pub index: Option<String>,
    /// Area id, as in the file name (nodes<AREA>.ynd)
    #[arg(long, value_name = "AREA")]
    pub area: Option<u32>,
    /// Output file (defaults to the cell's own file name in the current directory)
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct ExportArgs {
    pub file: PathBuf,
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,
}

pub fn run(args: &PathsArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    match &args.command {
        PathsCommand::Info(a) => run_info(a, keys),
        PathsCommand::Cell(a) => run_cell(a, keys, exe),
        PathsCommand::Export(a) => run_export(a),
        PathsCommand::Rewrite(a) => run_rewrite(a),
    }
}

// ─── info ────────────────────────────────────────────────────────────────────

fn run_info(args: &InfoArgs, keys: Option<&GtaKeys>) -> Result<()> {
    let data = load_resource_bytes(&args.file, args.archive.as_deref(), keys)?;
    let ynd = parse_ynd(&data).with_context(|| format!("parsing '{}'", args.file))?;
    let names = crate::names::load(&args.names, None).unwrap_or_else(|_| NameTable::core());
    print!("{}", describe(&ynd, ynd_area_id_from_file_name(&args.file), &names));
    Ok(())
}

/// The lines `paths info` and `resource info` print for a cell. `area` is
/// what the file name says; the nodes say otherwise only in a broken file.
pub fn describe(ynd: &Ynd, area: Option<u32>, names: &NameTable) -> String {
    let mut out = String::new();
    let area = area.or(ynd.area_id());
    match area {
        Some(area) => {
            let (cx, cy) = ynd_cell_of_area(area);
            let (x0, y0, x1, y1) = ynd_cell_bounds(cx, cy);
            let which = if ynd_is_island_area(area) {
                format!("area {area}: Cayo Perico over cell {cx},{cy}, area {} + {YND_ISLAND_FLAG}", area & (YND_ISLAND_FLAG - 1))
            } else {
                format!("area {area}, cell {cx},{cy}")
            };
            writeln!(out, "Cell:      nodes{area}.ynd ({which}), x {x0}..{x1}, y {y0}..{y1}").unwrap();
        }
        None => writeln!(out, "Cell:      empty (no nodes to say which)").unwrap(),
    }
    let n = &ynd.nodes;
    let count = |f: fn(&PathNode) -> bool| n.iter().filter(|n| f(n)).count();
    writeln!(out, "Nodes:     {} ({} vehicle, {} ped; {} junctions, {} disabled, {} highway, {} tunnel, {} off-road, {} no GPS)",
        n.len(), ynd.vehicle_node_count, ynd.ped_node_count,
        count(PathNode::is_junction), count(PathNode::is_disabled), count(PathNode::highway),
        count(PathNode::tunnel), count(PathNode::off_road), count(PathNode::no_gps)).unwrap();
    let links: Vec<_> = n.iter().flat_map(|n| &n.links).collect();
    let inside = |a: u16| area.is_some_and(|area| ynd_same_cell(a as u32, area));
    let foreign = links.iter().filter(|l| !inside(l.area_id)).count();
    writeln!(out, "Links:     {} ({} inside the cell, {} into other cells; {} shortcuts, {} not for navigation, {} two-way)",
        links.len(), links.len() - foreign, foreign,
        links.iter().filter(|l| l.shortcut()).count(),
        links.iter().filter(|l| l.dont_use_for_navigation()).count(),
        links.iter().filter(|l| l.is_two_way()).count()).unwrap();
    let heightmap: usize = ynd.junctions.iter().map(|j| j.heightmap.values.len()).sum();
    writeln!(out, "Junctions: {} ({} refs, {heightmap} heightmap bytes)", ynd.junctions.len(), ynd.junction_refs.len()).unwrap();

    let mut special: BTreeMap<String, usize> = BTreeMap::new();
    for node in n {
        let s = node.special();
        if s != rage_formats::NodeSpecial::None {
            *special.entry(s.name()).or_default() += 1;
        }
    }
    if !special.is_empty() {
        let list: Vec<String> = special.iter().map(|(k, v)| format!("{k} {v}")).collect();
        writeln!(out, "Special:   {}", list.join(", ")).unwrap();
    }

    let mut streets: BTreeMap<u32, usize> = BTreeMap::new();
    for node in n.iter().filter(|n| n.street_name != 0) {
        *streets.entry(node.street_name).or_default() += 1;
    }
    if !streets.is_empty() {
        let mut list: Vec<(u32, usize)> = streets.into_iter().collect();
        list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let shown: Vec<String> = list.iter().take(8).map(|(h, c)| format!("{} ({c})", names.resolve(*h))).collect();
        let more = if list.len() > 8 { format!(", {} more", list.len() - 8) } else { String::new() };
        writeln!(out, "Streets:   {}{more}", shown.join(", ")).unwrap();
    }

    let mut areas: Vec<u32> = links.iter().map(|l| l.area_id).filter(|a| !inside(*a)).map(u32::from).collect();
    areas.sort_unstable();
    areas.dedup();
    if !areas.is_empty() {
        writeln!(out, "Adjacent:  {}", areas.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")).unwrap();
    }
    if let Some(first) = n.first() {
        let (mut lo, mut hi) = (first.position, first.position);
        for node in n {
            lo = lo.min(node.position);
            hi = hi.max(node.position);
        }
        writeln!(out, "Extent:    x {:.2}..{:.2}, y {:.2}..{:.2}, z {:.2}..{:.2}", lo.x, hi.x, lo.y, hi.y, lo.z, hi.z).unwrap();
    }
    out
}

/// The `resource info --json` object for a cell.
pub fn json_summary(ynd: &Ynd, area: Option<u32>) -> json::JsonValue {
    let links: usize = ynd.nodes.iter().map(|n| n.links.len()).sum();
    json::object! {
        area: area.or(ynd.area_id()),
        nodes: ynd.nodes.len(),
        vehicleNodes: ynd.vehicle_node_count,
        pedNodes: ynd.ped_node_count,
        links: links,
        junctions: ynd.junctions.len(),
    }
}

// ─── cell ────────────────────────────────────────────────────────────────────

fn run_cell(args: &CellArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    let area = match (&args.at, &args.index, args.area) {
        (Some(at), _, _) => {
            let (x, y) = parse_pair(at).context("--at expects X,Y")?;
            let (cx, cy) = ynd_cell_for_position(x, y);
            ynd_area_id(cx, cy)
        }
        (_, Some(index), _) => {
            let (x, y) = parse_pair(index).context("--index expects CX,CY")?;
            if !(0.0..32.0).contains(&x) || !(0.0..32.0).contains(&y) {
                bail!("--index expects cell numbers 0..31");
            }
            ynd_area_id(x as u32, y as u32)
        }
        (_, _, Some(area)) => area,
        _ => bail!("give --at X,Y, --index CX,CY or --area N"),
    };
    let (cx, cy) = ynd_cell_of_area(area);
    let name = ynd_cell_file_name(cx, cy);
    let exe = exe.context("--exe or GTAV_PATH is required to read the game archives")?;
    let exe_path = crate::keys::resolve_exe(exe)?;
    let game_root = exe_path.parent().context("--exe has no parent directory")?;

    let (from, data) = find_in_game(game_root, &name, "paths", keys)?
        .with_context(|| format!("{name} not found in any archive under {}", game_root.display()))?;
    let output = args.output.clone().unwrap_or_else(|| PathBuf::from(&name));
    std::fs::write(&output, &data).with_context(|| format!("writing {}", output.display()))?;
    println!("{name}: {} bytes from {from}", data.len());
    println!("Wrote {}", output.display());
    let ynd = parse_ynd(&data)?;
    print!("{}", describe(&ynd, Some(area), &NameTable::core()));
    Ok(())
}

// ─── export ──────────────────────────────────────────────────────────────────

fn run_export(args: &ExportArgs) -> Result<()> {
    let data = std::fs::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let ynd = parse_ynd(&data)?;
    let area = ynd_area_id_from_file_name(&args.file.to_string_lossy()).or(ynd.area_id());
    let (obj, report) = ynd_to_obj(&ynd, area);
    std::fs::write(&args.output, obj).with_context(|| format!("writing {}", args.output.display()))?;
    println!("Wrote {} nodes, {} links ({} into other cells left out) and {} junction heightmaps to {}",
        report.nodes, report.links, report.foreign_links, report.junctions, args.output.display());
    Ok(())
}

struct ObjReport {
    nodes: usize,
    links: usize,
    foreign_links: usize,
    junctions: usize,
}

/// OBJ text for a cell: one vertex per node, the nodes as points grouped
/// `nodes_vehicle`/`nodes_ped`/`nodes_disabled`, the links as lines grouped
/// by kind (a link into a cell not in the file has no far end and is left
/// out), and each junction heightmap as a mesh in `junctions`, laid out as
/// CodeWalker's `UpdateJunctionTriangleVertices` does.
fn ynd_to_obj(ynd: &Ynd, area: Option<u32>) -> (String, ObjReport) {
    let mut out = String::from("# rage paths export\n");
    let index_of = |node_id: u16| ynd.nodes.iter().position(|n| n.node_id == node_id);
    for n in &ynd.nodes {
        writeln!(out, "v {} {} {}", n.position.x, n.position.y, n.position.z).unwrap();
    }
    let mut points: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut lines: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let (mut links, mut foreign) = (0usize, 0usize);
    for (i, n) in ynd.nodes.iter().enumerate() {
        let group = if n.is_disabled() { "nodes_disabled" } else if n.is_ped_node() { "nodes_ped" } else { "nodes_vehicle" };
        points.entry(group).or_default().push(format!("p {}", i + 1));
        for l in &n.links {
            if !area.is_some_and(|area| ynd_same_cell(l.area_id as u32, area)) {
                foreign += 1;
                continue;
            }
            let Some(j) = index_of(l.node_id) else { foreign += 1; continue };
            let to = &ynd.nodes[j];
            let group = if l.shortcut() { "links_shortcut" }
                else if n.is_disabled() || to.is_disabled() { "links_disabled" }
                else if n.is_ped_node() || to.is_ped_node() { "links_ped" }
                else if n.off_road() || to.off_road() { "links_offroad" }
                else if l.dont_use_for_navigation() { "links_no_navigation" }
                else { "links_road" };
            lines.entry(group).or_default().push(format!("l {} {}", i + 1, j + 1));
            links += 1;
        }
    }
    for (group, items) in points.iter().chain(lines.iter()) {
        writeln!(out, "g {group}").unwrap();
        for item in items { writeln!(out, "{item}").unwrap(); }
    }
    let mut next = ynd.nodes.len() + 1;
    let mut junctions = 0usize;
    for j in &ynd.junctions {
        let h = &j.heightmap;
        if h.width < 2 || h.height < 2 { continue; }
        if junctions == 0 { writeln!(out, "g junctions").unwrap(); }
        junctions += 1;
        let range = j.max_z - j.min_z;
        for y in 0..h.height as usize {
            for x in 0..h.width as usize {
                let z = j.min_z + h.values[y * h.width as usize + x] as f32 / 255.0 * range;
                writeln!(out, "v {} {} {}", j.position.x + x as f32 * 2.0, j.position.y + y as f32 * 2.0, z).unwrap();
            }
        }
        let w = h.width as usize;
        for y in 1..h.height as usize {
            for x in 1..w {
                let (a, b, c, d) = (next + (y - 1) * w + (x - 1), next + (y - 1) * w + x, next + y * w + (x - 1), next + y * w + x);
                writeln!(out, "f {a} {b} {c}").unwrap();
                writeln!(out, "f {c} {b} {d}").unwrap();
            }
        }
        next += w * h.height as usize;
    }
    (out, ObjReport { nodes: ynd.nodes.len(), links, foreign_links: foreign, junctions })
}

fn run_rewrite(args: &ExportArgs) -> Result<()> {
    let data = std::fs::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let ynd = parse_ynd(&data)?;
    let bytes = serialize_ynd(&ynd)?;
    std::fs::write(&args.output, &bytes).with_context(|| format!("writing {}", args.output.display()))?;
    println!("Rewrote {} nodes: {} -> {} bytes, {}", ynd.nodes.len(), data.len(), bytes.len(), args.output.display());
    Ok(())
}
