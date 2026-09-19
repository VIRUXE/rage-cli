// `rage navmesh`: inspect, fetch, export and build navmesh cells (.ynv).

use anyhow::{bail, Context, Result};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rage_formats::{
    cell_bounds, cell_file_name, cell_for_position, parse_ybn, parse_ymap_entities, parse_ynv, parse_ytyp, rage_joaat,
    serialize_ynv, Triangle, Vec3, Ynv, YmapEntity,
};

use crate::navmesh::{build, BuildOptions, Footprint};
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
    /// Draw a top-down PNG of a cell: polygons over the collision floor and walls
    Plot(PlotArgs),
}

#[derive(clap::Args)]
pub struct PlotArgs {
    /// The .ynv to draw (interior polygons green, sunk red, the rest grey)
    pub file: PathBuf,
    /// World XY box to draw: x0,y0,x1,y1 (default: the interior polygons' extent plus 2 m)
    #[arg(long, value_name = "X0,Y0,X1,Y1")]
    pub region: Option<String>,
    /// Collision file(s) to draw underneath (floor light grey, walls at body height blue)
    #[arg(long, value_name = "FILE")]
    pub ybn: Vec<PathBuf>,
    /// Place the collision by this .ymap's MLO instance
    #[arg(long, value_name = "FILE")]
    pub ymap: Option<PathBuf>,
    /// Floor height used to classify collision triangles
    #[arg(long)]
    pub floor_z: Option<f32>,
    /// Markers to draw: "x,y,label"; repeatable
    #[arg(long, value_name = "X,Y,LABEL")]
    pub marker: Vec<String>,
    /// Pixels per metre
    #[arg(long, default_value = "30")]
    pub scale: f32,
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,
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
    /// .ytyp file(s) declaring the MLO and its props; every prop the MLO
    /// places whose archetype box is furniture-height gets its footprint
    /// blocked (collision inside escrowed .ydr files is invisible otherwise)
    #[arg(long, value_name = "FILE")]
    pub ytyp: Vec<PathBuf>,
    /// Directory whose file names (e.g. stream/ydr) name the archetype hashes in the report
    #[arg(long, value_name = "DIR")]
    pub names_from: Option<PathBuf>,
    /// Also look unknown archetypes up in the game's texture index (needs
    /// --exe and a built `rage index`), so vanilla props placed by the MLO
    /// get their boxes too
    #[arg(long)]
    pub game_props: bool,
    /// A prop whose top is lower than this above the floor is stepped over, not blocked
    #[arg(long, default_value = "0.3")]
    pub step_height: f32,
    /// A prop with a bigger footprint (m²) is a room shell or decor set, not furniture: skipped
    #[arg(long, default_value = "12")]
    pub max_prop_area: f32,
    /// Prop names (as in --names-from) to block regardless of the rules; repeatable
    #[arg(long, value_name = "NAME")]
    pub block_entity: Vec<String>,
    /// Name fragments never to block, on top of the defaults (lproxy,
    /// _details, shell, door, window, carpet); repeatable
    #[arg(long, value_name = "FRAGMENT")]
    pub ignore_entity: Vec<String>,
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
        NavmeshCommand::Build(a) => run_build(a, exe),
        NavmeshCommand::Rewrite(a) => run_rewrite(a),
        NavmeshCommand::Plot(a) => run_plot(a),
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

/// Footprints of the props the placed MLO contains, per the height rule and
/// the --block-entity/--ignore-entity overrides; prints one line per prop.
fn prop_footprints(
    args: &BuildArgs, placement: &YmapEntity, opts: &BuildOptions, game_boxes: &std::collections::HashMap<u32, (Vec3, Vec3)>,
) -> Result<Vec<Footprint>> {
    let mut archetypes: std::collections::HashMap<u32, (Vec3, Vec3)> = game_boxes.clone();
    let mut mlos = Vec::new();
    for path in &args.ytyp {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let ytyp = parse_ytyp(&data).with_context(|| format!("parsing {}", path.display()))?;
        for a in ytyp.archetypes { archetypes.insert(a.name_hash, (a.bb_min, a.bb_max)); }
        mlos.extend(ytyp.mlos);
    }
    let mlo = mlos.iter().find(|m| m.name_hash == placement.archetype_hash)
        .with_context(|| format!("no CMloArchetypeDef with hash {:#010x} (the .ymap's MLO) in the given .ytyp files", placement.archetype_hash))?;

    let mut names: std::collections::HashMap<u32, String> = Default::default();
    if let Some(dir) = &args.names_from {
        for entry in walkdir(dir)? {
            if let Some(stem) = entry.file_stem().and_then(|s| s.to_str()) {
                let stem = stem.to_lowercase();
                names.insert(rage_joaat(&stem), stem);
            }
        }
    }
    let name_of = |hash: u32| names.get(&hash).cloned().unwrap_or_else(|| format!("{hash:#010x}"));
    let forced: std::collections::HashSet<String> = args.block_entity.iter().map(|s| s.to_lowercase()).collect();
    let mut ignored: Vec<String> = ["lproxy", "_details", "shell", "door", "window", "carpet"].iter().map(|s| s.to_string()).collect();
    ignored.extend(args.ignore_entity.iter().map(|s| s.to_lowercase()));

    // A prop counts as furniture when it stands on the floor (bottom within
    // the floor tolerance) and rises above step height; big footprints are
    // room shells and decor sets; name fragments catch doors, light proxies
    // and the like.
    let floor_lo = opts.floor_z - opts.floor_below - 0.1;
    let floor_hi = opts.floor_z + opts.floor_above;
    let mut out = Vec::new();
    let mut counts = [0usize; 5]; // blocked, ignored by name, too big, off the floor, unknown
    eprintln!("{:<42} {:>6} {:>7} {:>13}  decision", "prop", "height", "area", "z (world)");
    for e in &mlo.entities {
        let name = name_of(e.archetype_hash);
        let Some(&(bb_min, bb_max)) = archetypes.get(&e.archetype_hash) else { counts[4] += 1; eprintln!("{name:<42} {:>6} {:>7} {:>13}  unknown archetype (try --game-props)", "?", "?", "?"); continue };
        let corners = [
            (bb_min.x, bb_min.y, bb_min.z), (bb_max.x, bb_min.y, bb_min.z), (bb_max.x, bb_max.y, bb_min.z), (bb_min.x, bb_max.y, bb_min.z),
            (bb_min.x, bb_min.y, bb_max.z), (bb_max.x, bb_min.y, bb_max.z), (bb_max.x, bb_max.y, bb_max.z), (bb_min.x, bb_max.y, bb_max.z),
        ];
        let world: Vec<Vec3> = corners.iter().map(|&(x, y, z)| placement.to_world(e.to_world(Vec3::new(x, y, z)))).collect();
        let z_lo = world.iter().map(|v| v.z).fold(f32::MAX, f32::min);
        let z_hi = world.iter().map(|v| v.z).fold(f32::MIN, f32::max);
        let fp = Footprint::from_points(&world.iter().map(|v| (v.x, v.y)).collect::<Vec<_>>(), z_lo, z_hi);
        let height = bb_max.z - bb_min.z;
        let (slot, decision) = if forced.contains(&name) { (0, "blocked (--block-entity)") }
            else if ignored.iter().any(|frag| name.contains(frag.as_str())) { (1, "skipped: name") }
            else if z_lo > floor_hi || z_lo < floor_lo { (3, "skipped: not standing on the floor") }
            else if z_hi < opts.floor_z + args.step_height { (3, "skipped: below step height") }
            else if fp.area() > args.max_prop_area { (2, "skipped: bigger than --max-prop-area") }
            else if fp.area() < 0.02 { (3, "skipped: tiny") }
            else { (0, "blocked") };
        eprintln!("{name:<42} {height:>6.2} {:>7.2} {:>6.2}..{:<6.2}  {decision}", fp.area(), z_lo, z_hi);
        counts[slot] += 1;
        if slot == 0 {
            out.push(fp);
        }
    }
    eprintln!("props: {} blocked, {} skipped by name, {} too big, {} not furniture height, {} without a known archetype",
        counts[0], counts[1], counts[2], counts[3], counts[4]);
    Ok(out)
}

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() { out.extend(walkdir(&path)?); } else { out.push(path); }
    }
    Ok(out)
}

// ─── plot ────────────────────────────────────────────────────────────────────

fn run_plot(args: &PlotArgs) -> Result<()> {
    use rage_formats::image::{Rgba, RgbaImage};

    let data = std::fs::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let ynv = parse_ynv(&data)?;
    let interior: Vec<&rage_formats::NavPoly> = ynv.polys.iter().filter(|p| p.is_interior()).collect();
    let region = match &args.region {
        Some(r) => parse_quad(r).context("--region")?,
        None => {
            if interior.is_empty() { bail!("no interior polygons to frame; give --region"); }
            let mut r = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
            for p in &interior {
                let (lo, hi) = p.bounds();
                r[0] = r[0].min(lo.x); r[1] = r[1].min(lo.y); r[2] = r[2].max(hi.x); r[3] = r[3].max(hi.y);
            }
            [r[0] - 2.0, r[1] - 2.0, r[2] + 2.0, r[3] + 2.0]
        }
    };
    let [x0, y0, x1, y1] = region;
    let scale = args.scale.max(1.0);
    let w = ((x1 - x0) * scale).ceil().max(1.0) as u32;
    let h = ((y1 - y0) * scale).ceil().max(1.0) as u32;
    if w * h > 40_000_000 { bail!("{w}x{h} px is too large; lower --scale or shrink --region"); }
    let mut img = RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]));
    // y up in the world, down in the image
    let to_px = |x: f32, y: f32| ((x - x0) * scale, (y1 - y) * scale);

    let floor_z = args.floor_z.or_else(|| interior.first().map(|p| p.centroid().z));
    if !args.ybn.is_empty() {
        let placement = match &args.ymap { Some(p) => Some(mlo_placement(p)?), None => None };
        let tris = load_triangles(&args.ybn, placement.as_ref())?;
        let fz = floor_z.context("--floor-z is needed to classify collision when the cell has no interior polygons")?;
        for t in &tris {
            let n = t.normal();
            let zc = (t.vertices[0].z + t.vertices[1].z + t.vertices[2].z) / 3.0;
            let zlo = t.vertices.iter().map(|v| v.z).fold(f32::MAX, f32::min);
            let zhi = t.vertices.iter().map(|v| v.z).fold(f32::MIN, f32::max);
            let pts: Vec<(f32, f32)> = t.vertices.iter().map(|v| to_px(v.x, v.y)).collect();
            if n.z > 0.7 && (zc - fz).abs() < 0.6 {
                fill_polygon(&mut img, &pts, Rgba([225, 225, 225, 255]));
            } else if n.z.abs() < 0.7 && zlo < fz + 0.45 && zhi > fz + 0.15 {
                stroke_polygon(&mut img, &pts, Rgba([120, 120, 255, 255]));
            }
        }
    }
    for p in &ynv.polys {
        let pts: Vec<(f32, f32)> = p.vertices.iter().map(|v| to_px(v.x, v.y)).collect();
        if pts.iter().all(|&(x, y)| x < 0.0 || y < 0.0 || x > w as f32 || y > h as f32) { continue; }
        let sunk = p.vertices.iter().all(|v| (v.z - ynv.bb_min.z).abs() < 1e-3);
        if sunk {
            stroke_polygon(&mut img, &pts, Rgba([220, 40, 40, 255]));
        } else if p.is_interior() {
            fill_polygon(&mut img, &pts, Rgba([120, 210, 120, 140]));
            stroke_polygon(&mut img, &pts, Rgba([0, 120, 0, 255]));
        } else {
            stroke_polygon(&mut img, &pts, Rgba([170, 170, 170, 255]));
        }
    }
    for m in &args.marker {
        let mut it = m.splitn(3, ',');
        let (Some(x), Some(y)) = (it.next().and_then(|v| v.trim().parse::<f32>().ok()), it.next().and_then(|v| v.trim().parse::<f32>().ok())) else {
            bail!("--marker expects x,y,label; got '{m}'");
        };
        let label = it.next().unwrap_or("");
        let (px, py) = to_px(x, y);
        let r = 4.0;
        fill_polygon(&mut img, &[(px - r, py - r), (px + r, py - r), (px + r, py + r), (px - r, py + r)], Rgba([0, 0, 0, 255]));
        rage_render::draw_text(&mut img, (px + 6.0) as i32, (py - 4.0) as i32, label, 1, [0, 0, 0, 255]);
    }
    img.save(&args.output).with_context(|| format!("writing {}", args.output.display()))?;
    println!("Wrote {} ({w}x{h}, {} interior polygons, region {x0},{y0}..{x1},{y1})", args.output.display(), interior.len());
    Ok(())
}

/// Scanline fill of a simple polygon with alpha blending.
fn fill_polygon(img: &mut rage_formats::image::RgbaImage, pts: &[(f32, f32)], colour: rage_formats::image::Rgba<u8>) {
    if pts.len() < 3 { return; }
    let (w, h) = (img.width() as i32, img.height() as i32);
    let y_min = pts.iter().map(|p| p.1).fold(f32::MAX, f32::min).floor().max(0.0) as i32;
    let y_max = pts.iter().map(|p| p.1).fold(f32::MIN, f32::max).ceil().min(h as f32 - 1.0) as i32;
    let mut xs: Vec<f32> = Vec::new();
    for y in y_min..=y_max {
        let sy = y as f32 + 0.5;
        xs.clear();
        for i in 0..pts.len() {
            let (ax, ay) = pts[i];
            let (bx, by) = pts[(i + 1) % pts.len()];
            if (ay <= sy && by > sy) || (by <= sy && ay > sy) {
                xs.push(ax + (sy - ay) / (by - ay) * (bx - ax));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for pair in xs.chunks(2) {
            if pair.len() < 2 { break; }
            let xa = pair[0].round().max(0.0) as i32;
            let xb = pair[1].round().min(w as f32 - 1.0) as i32;
            for x in xa..=xb {
                blend(img, x, y, colour);
            }
        }
    }
}

fn stroke_polygon(img: &mut rage_formats::image::RgbaImage, pts: &[(f32, f32)], colour: rage_formats::image::Rgba<u8>) {
    for i in 0..pts.len() {
        let (ax, ay) = pts[i];
        let (bx, by) = pts[(i + 1) % pts.len()];
        let steps = ((bx - ax).abs().max((by - ay).abs())).ceil().max(1.0) as i32;
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            blend(img, (ax + (bx - ax) * t).round() as i32, (ay + (by - ay) * t).round() as i32, colour);
        }
    }
}

fn blend(img: &mut rage_formats::image::RgbaImage, x: i32, y: i32, c: rage_formats::image::Rgba<u8>) {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 { return; }
    let p = img.get_pixel_mut(x as u32, y as u32);
    let a = c.0[3] as u32;
    for k in 0..3 {
        p.0[k] = ((c.0[k] as u32 * a + p.0[k] as u32 * (255 - a)) / 255) as u8;
    }
    p.0[3] = 255;
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

fn run_build(args: &BuildArgs, exe: Option<&Path>) -> Result<()> {
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
    if !args.ytyp.is_empty() {
        let placement = placement.as_ref().context("--ytyp needs --ymap to place the MLO's props")?;
        let game_boxes = if args.game_props {
            let exe = exe.context("--game-props needs --exe or GTAV_PATH")?;
            let exe_path = crate::keys::resolve_exe(exe)?;
            let path = crate::index::GameIndex::cache_path(&exe_path).context("no cache directory (no HOME/USERPROFILE?)")?;
            let index = crate::index::GameIndex::load_cached(&path)
                .with_context(|| format!("--game-props needs the texture index at {}; run `rage index build` first", path.display()))?;
            index.archetype_box
        } else {
            Default::default()
        };
        opts.footprints = prop_footprints(args, placement, &opts, &game_boxes)?;
    }

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
