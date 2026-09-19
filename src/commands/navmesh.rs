// `rage navmesh`: inspect, fetch, export and build navmesh cells (.ynv).

use anyhow::{bail, Context, Result};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rage_formats::{
    cell_bounds, cell_file_name, cell_for_position, parse_ybn, parse_ymap_entities, parse_ynv, rage_joaat,
    serialize_ynv, Triangle, Ynv, YmapEntity,
};

use crate::navmesh::{build, BuildOptions};
use crate::resources::load_resource_bytes;
use crate::rpf::{Archive, GtaKeys};

#[derive(clap::Args)]
pub struct NavmeshArgs {
    #[command(subcommand)]
    pub command: NavmeshCommand,
}

#[derive(clap::Subcommand)]
pub enum NavmeshCommand {
    /// Summarise a .ynv: cell, bounds, polygon/edge/portal/point counts
    Info(InfoArgs),
    /// Pull a grid cell's navmesh out of the game, from the archive that loads last
    Cell(CellArgs),
    /// Write a .ynv's polygons as a Wavefront OBJ (one group per polygon class)
    Export(ExportArgs),
    /// Write a .ybn's triangles as a Wavefront OBJ, optionally placed by a .ymap entity
    YbnObj(YbnObjArgs),
    /// Generate interior polygons from collision and append them to a cell
    Build(BuildArgs),
    /// Parse a .ynv and write it back out unchanged (checks the writer against the game)
    Rewrite(ExportArgs),
}

#[derive(clap::Args)]
pub struct InfoArgs {
    /// A loose .ynv on disk, or (with --archive) a name inside the archive
    pub file: String,
    /// Look FILE up inside this RPF archive instead of on disk
    #[arg(short, long, value_name = "RPF")]
    pub archive: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct CellArgs {
    /// World position "X,Y" inside the cell
    #[arg(long, value_name = "X,Y", conflicts_with = "index")]
    pub at: Option<String>,
    /// Cell index "CX,CY" (0..99 each)
    #[arg(long, value_name = "CX,CY")]
    pub index: Option<String>,
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

#[derive(clap::Args)]
pub struct YbnObjArgs {
    pub file: PathBuf,
    /// Place the collision by this .ymap's MLO instance (or first entity)
    #[arg(long, value_name = "FILE")]
    pub ymap: Option<PathBuf>,
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    /// The cell to extend (from `rage navmesh cell`)
    pub cell: PathBuf,
    /// Collision file(s) to read the floor and obstacles from
    #[arg(long, value_name = "FILE", required = true)]
    pub ybn: Vec<PathBuf>,
    /// Place the collision by this .ymap's MLO instance entity
    #[arg(long, value_name = "FILE")]
    pub ymap: Option<PathBuf>,
    /// World XY box to work inside: x0,y0,x1,y1
    #[arg(long, value_name = "X0,Y0,X1,Y1")]
    pub clip: String,
    /// Floor height the interior sits at
    #[arg(long, value_name = "Z")]
    pub floor_z: f32,
    /// Extra blocked XY boxes (x0,y0,x1,y1); repeatable
    #[arg(long, value_name = "X0,Y0,X1,Y1")]
    pub block: Vec<String>,
    /// Grid step in metres
    #[arg(long, default_value = "0.25")]
    pub grid: f32,
    /// Longest polygon side in metres
    #[arg(long, default_value = "3.0")]
    pub max_side: f32,
    /// Body slab above the floor that must be clear: low,high
    #[arg(long, default_value = "0.15,0.45", value_name = "LOW,HIGH")]
    pub body: String,
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,
    /// Also write the new polygons (and the sunk ones) as an OBJ for checking
    #[arg(long, value_name = "FILE")]
    pub obj: Option<PathBuf>,
}

pub fn run(args: &NavmeshArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    match &args.command {
        NavmeshCommand::Info(a) => run_info(a, keys),
        NavmeshCommand::Cell(a) => run_cell(a, keys, exe),
        NavmeshCommand::Export(a) => run_export(a),
        NavmeshCommand::YbnObj(a) => run_ybn_obj(a),
        NavmeshCommand::Build(a) => run_build(a),
        NavmeshCommand::Rewrite(a) => run_rewrite(a),
    }
}

// ─── info ────────────────────────────────────────────────────────────────────

fn run_info(args: &InfoArgs, keys: Option<&GtaKeys>) -> Result<()> {
    let data = load_resource_bytes(&args.file, args.archive.as_deref(), keys)?;
    let ynv = parse_ynv(&data).with_context(|| format!("parsing '{}'", args.file))?;
    print!("{}", describe(&ynv));
    Ok(())
}

pub fn describe(ynv: &Ynv) -> String {
    let mut out = String::new();
    match ynv.cell() {
        Some((cx, cy)) => {
            let (x0, y0, x1, y1) = cell_bounds(cx, cy);
            writeln!(out, "Cell:      {} (area {}), x {x0}..{x1}, y {y0}..{y1}", cell_file_name(cx, cy), ynv.area_id).unwrap();
        }
        None => writeln!(out, "Area:      {} (standalone)", ynv.area_id).unwrap(),
    }
    writeln!(out, "Flags:     {} ({})", ynv.content_flags, content_flag_names(ynv.content_flags)).unwrap();
    writeln!(out, "Bounds:    ({:.2}, {:.2}, {:.2}) .. ({:.2}, {:.2}, {:.2})",
        ynv.bb_min.x, ynv.bb_min.y, ynv.bb_min.z, ynv.bb_max.x, ynv.bb_max.y, ynv.bb_max.z).unwrap();
    let interior = ynv.polys.iter().filter(|p| p.is_interior()).count();
    let edges: usize = ynv.polys.iter().map(|p| p.edges.len()).sum();
    let linked = ynv.polys.iter().flat_map(|p| &p.edges).filter(|e| !e.a.is_none()).count();
    let foreign = ynv.polys.iter().flat_map(|p| &p.edges).filter(|e| !e.a.is_none() && e.a.area_id != ynv.area_id).count();
    writeln!(out, "Polygons:  {} ({} interior, {} flat ground, {} footpath, {} road, {} water)",
        ynv.polys.len(), interior,
        ynv.polys.iter().filter(|p| p.is_flat_ground()).count(),
        ynv.polys.iter().filter(|p| p.is_footpath()).count(),
        ynv.polys.iter().filter(|p| p.is_road()).count(),
        ynv.polys.iter().filter(|p| p.is_water()).count()).unwrap();
    writeln!(out, "Edges:     {edges} ({linked} linked, {foreign} into other cells)").unwrap();
    writeln!(out, "Portals:   {}", ynv.portals.len()).unwrap();
    writeln!(out, "Points:    {}", ynv.points.len()).unwrap();
    let mut areas: Vec<u32> = ynv.polys.iter().flat_map(|p| &p.edges).filter(|e| !e.a.is_none()).map(|e| e.a.area_id).collect();
    areas.sort_unstable();
    areas.dedup();
    writeln!(out, "Adjacent:  {}", areas.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")).unwrap();
    out
}

fn content_flag_names(flags: u32) -> String {
    let mut names = Vec::new();
    if flags & 1 != 0 { names.push("Polygons"); }
    if flags & 2 != 0 { names.push("Portals"); }
    if flags & 4 != 0 { names.push("Vehicle"); }
    if flags & 8 != 0 { names.push("Unknown8"); }
    if flags & 16 != 0 { names.push("Unknown16"); }
    if names.is_empty() { "none".to_string() } else { names.join(", ") }
}

// ─── cell ────────────────────────────────────────────────────────────────────

fn run_cell(args: &CellArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    let (cx, cy) = match (&args.at, &args.index) {
        (Some(at), _) => {
            let (x, y) = parse_pair(at).context("--at expects X,Y")?;
            cell_for_position(x, y)
        }
        (_, Some(index)) => {
            let (x, y) = parse_pair(index).context("--index expects CX,CY")?;
            (x as u32, y as u32)
        }
        _ => bail!("give --at X,Y or --index CX,CY"),
    };
    let name = cell_file_name(cx, cy);
    let exe = exe.context("--exe or GTAV_PATH is required to read the game archives")?;
    let exe_path = crate::keys::resolve_exe(exe)?;
    let game_root = exe_path.parent().context("--exe has no parent directory")?;

    let (from, data) = find_in_game(game_root, &name, keys)?
        .with_context(|| format!("{name} not found in any archive under {}", game_root.display()))?;
    let output = args.output.clone().unwrap_or_else(|| PathBuf::from(&name));
    std::fs::write(&output, &data).with_context(|| format!("writing {}", output.display()))?;
    println!("{name}: {} bytes from {from}", data.len());
    println!("Wrote {}", output.display());
    let ynv = parse_ynv(&data)?;
    print!("{}", describe(&ynv));
    Ok(())
}

/// Finds `name` (by stem hash) across the game's archives in load order —
/// base, update.rpf, then DLC packs — descending only into nested archives
/// that look like navmesh packs; the last hit wins, as in the game.
fn find_in_game(game_root: &Path, name: &str, keys: Option<&GtaKeys>) -> Result<Option<(String, Vec<u8>)>> {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s).to_lowercase();
    let hash = rage_joaat(&stem);
    let mut found = None;
    for archive_path in crate::index::ranked_archives(game_root, keys)? {
        let archive = match Archive::open(&archive_path, keys) {
            Ok(a) => a,
            Err(_) => continue,
        };
        if archive.require_keys(keys).is_err() { continue; }
        let label = archive_path.display().to_string();
        find_in_archive(&archive, &label, hash, keys, 0, &mut |hit| found = Some(hit))?;
    }
    Ok(found)
}

fn find_in_archive(
    archive: &Archive, label: &str, hash: u32, keys: Option<&GtaKeys>, depth: usize,
    on_hit: &mut dyn FnMut((String, Vec<u8>)),
) -> Result<()> {
    if depth > 4 { return Ok(()); }
    let files: Vec<_> = archive.list_files().into_iter().cloned().collect();
    for file in files {
        let lower = file.name.to_lowercase();
        if lower.ends_with(".rpf") {
            if !lower.contains("nav") { continue; }
            if let Ok(nested) = archive.extract(&file, keys).and_then(|d| Archive::from_bytes(d, &file.name, keys)) {
                find_in_archive(&nested, &format!("{label}:{}", file.path), hash, keys, depth + 1, on_hit)?;
            }
            continue;
        }
        if !lower.ends_with(".ynv") { continue; }
        let stem = lower.rsplit_once('.').map_or(lower.as_str(), |(s, _)| s);
        if rage_joaat(stem) == hash {
            let data = archive.extract(&file, keys)?;
            on_hit((format!("{label}:{}", file.path), data));
        }
    }
    Ok(())
}

fn parse_pair(s: &str) -> Result<(f32, f32)> {
    let mut it = s.split(',').map(|p| p.trim().parse::<f32>());
    match (it.next(), it.next(), it.next()) {
        (Some(Ok(a)), Some(Ok(b)), None) => Ok((a, b)),
        _ => bail!("expected two comma-separated numbers, got '{s}'"),
    }
}

fn parse_quad(s: &str) -> Result<[f32; 4]> {
    let vals: Result<Vec<f32>, _> = s.split(',').map(|p| p.trim().parse::<f32>()).collect();
    match vals {
        Ok(v) if v.len() == 4 => Ok([v[0], v[1], v[2], v[3]]),
        _ => bail!("expected four comma-separated numbers, got '{s}'"),
    }
}

// ─── export ──────────────────────────────────────────────────────────────────

fn run_export(args: &ExportArgs) -> Result<()> {
    let data = std::fs::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let ynv = parse_ynv(&data)?;
    let obj = ynv_to_obj(&ynv, None);
    std::fs::write(&args.output, obj).with_context(|| format!("writing {}", args.output.display()))?;
    println!("Wrote {} polygons to {}", ynv.polys.len(), args.output.display());
    Ok(())
}

/// OBJ text for a navmesh: `interior`, `exterior` and `sunk` groups, plus a
/// `new` group for polygons at index >= `first_new` when given.
fn ynv_to_obj(ynv: &Ynv, first_new: Option<usize>) -> String {
    let mut out = String::from("# rage navmesh export\n");
    let mut faces: Vec<(&'static str, String)> = Vec::new();
    let mut next = 1usize;
    for (i, p) in ynv.polys.iter().enumerate() {
        for v in &p.vertices {
            writeln!(out, "v {} {} {}", v.x, v.y, v.z).unwrap();
        }
        let group = if first_new.is_some_and(|f| i >= f) { "new" }
            else if p.vertices.iter().all(|v| (v.z - ynv.bb_min.z).abs() < 1e-3) { "sunk" }
            else if p.is_interior() { "interior" } else { "exterior" };
        let idx: Vec<String> = (0..p.vertices.len()).map(|k| (next + k).to_string()).collect();
        faces.push((group, format!("f {}", idx.join(" "))));
        next += p.vertices.len();
    }
    for group in ["exterior", "interior", "sunk", "new"] {
        let lines: Vec<&String> = faces.iter().filter(|(g, _)| *g == group).map(|(_, f)| f).collect();
        if lines.is_empty() { continue; }
        writeln!(out, "g {group}").unwrap();
        for f in lines { writeln!(out, "{f}").unwrap(); }
    }
    out
}

fn run_rewrite(args: &ExportArgs) -> Result<()> {
    let data = std::fs::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let ynv = parse_ynv(&data)?;
    let bytes = serialize_ynv(&ynv)?;
    std::fs::write(&args.output, &bytes).with_context(|| format!("writing {}", args.output.display()))?;
    println!("Rewrote {} polygons: {} -> {} bytes, {}", ynv.polys.len(), data.len(), bytes.len(), args.output.display());
    Ok(())
}

// ─── ybn-obj ─────────────────────────────────────────────────────────────────

fn run_ybn_obj(args: &YbnObjArgs) -> Result<()> {
    let placement = match &args.ymap { Some(p) => Some(mlo_placement(p)?), None => None };
    let tris = load_triangles(std::slice::from_ref(&args.file), placement.as_ref())?;
    let mut out = String::from("# rage navmesh ybn-obj\n");
    for t in &tris {
        for v in t.vertices { writeln!(out, "v {} {} {}", v.x, v.y, v.z).unwrap(); }
    }
    let mut by_material: std::collections::BTreeMap<u8, Vec<usize>> = Default::default();
    for (i, t) in tris.iter().enumerate() { by_material.entry(t.material).or_default().push(i); }
    for (m, idx) in by_material {
        writeln!(out, "g material_{m}").unwrap();
        for i in idx { writeln!(out, "f {} {} {}", i * 3 + 1, i * 3 + 2, i * 3 + 3).unwrap(); }
    }
    std::fs::write(&args.output, out).with_context(|| format!("writing {}", args.output.display()))?;
    println!("Wrote {} triangles to {}", tris.len(), args.output.display());
    Ok(())
}

/// The MLO instance entity of a .ymap (or its first entity).
fn mlo_placement(path: &Path) -> Result<YmapEntity> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let entities = parse_ymap_entities(&data)?;
    entities.iter().find(|e| e.is_mlo_instance).or(entities.first()).copied()
        .with_context(|| format!("{} places no entities", path.display()))
}

fn load_triangles(files: &[PathBuf], placement: Option<&YmapEntity>) -> Result<Vec<Triangle>> {
    let mut all = Vec::new();
    for file in files {
        let data = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        let ybn = parse_ybn(&data).with_context(|| format!("parsing {}", file.display()))?;
        let mut tris = ybn.triangles();
        if let Some(e) = placement {
            for t in &mut tris {
                for v in &mut t.vertices { *v = e.to_world(*v); }
            }
        }
        eprintln!("{}: {} triangles", file.display(), tris.len());
        all.extend(tris);
    }
    Ok(all)
}

// ─── build ───────────────────────────────────────────────────────────────────

fn run_build(args: &BuildArgs) -> Result<()> {
    let data = std::fs::read(&args.cell).with_context(|| format!("reading {}", args.cell.display()))?;
    let mut cell = parse_ynv(&data)?;
    let before = cell.polys.len();
    let placement = match &args.ymap { Some(p) => Some(mlo_placement(p)?), None => None };
    if let Some(e) = &placement {
        eprintln!("placing collision at ({:.3}, {:.3}, {:.3})", e.position.x, e.position.y, e.position.z);
    }
    let tris = load_triangles(&args.ybn, placement.as_ref())?;

    let (body_low, body_high) = parse_pair(&args.body).context("--body expects LOW,HIGH")?;
    let mut opts = BuildOptions {
        clip: parse_quad(&args.clip).context("--clip")?,
        floor_z: args.floor_z,
        grid: args.grid,
        max_side: args.max_side,
        body_low,
        body_high,
        ..Default::default()
    };
    for b in &args.block { opts.blocks.push(parse_quad(b).context("--block")?); }

    let (x0, y0, x1, y1) = match cell.cell() {
        Some((cx, cy)) => cell_bounds(cx, cy),
        None => (cell.bb_min.x, cell.bb_min.y, cell.bb_max.x, cell.bb_max.y),
    };
    let [cx0, cy0, cx1, cy1] = opts.clip;
    if cx0 < x0 || cy0 < y0 || cx1 > x1 || cy1 > y1 {
        bail!("clip box {cx0},{cy0}..{cx1},{cy1} leaves the cell ({x0},{y0}..{x1},{y1}); build each cell separately");
    }
    if args.floor_z < cell.bb_min.z || args.floor_z > cell.bb_max.z {
        bail!("floor z {} is outside the cell's vertical range {}..{}", args.floor_z, cell.bb_min.z, cell.bb_max.z);
    }

    let report = build(&mut cell, &tris, &opts)?;
    let bytes = serialize_ynv(&cell)?;
    std::fs::write(&args.output, &bytes).with_context(|| format!("writing {}", args.output.display()))?;

    println!("Grid:       {} cells of {} m ({} floor, {} blocked, {} free)",
        report.grid_cells, opts.grid, report.floor_cells, report.blocked_cells, report.free_cells);
    println!("Rectangles: {}", report.rectangles);
    println!("Polygons:   {} before, {} sunk, {} added ({} internal edges linked), {} after",
        before, report.sunk_polys, report.new_polys, report.internal_edges, cell.polys.len());
    println!("Wrote {} ({} bytes)", args.output.display(), bytes.len());

    if let Some(obj) = &args.obj {
        std::fs::write(obj, ynv_to_obj(&cell, Some(before))).with_context(|| format!("writing {}", obj.display()))?;
        println!("Wrote {}", obj.display());
    }
    Ok(())
}
