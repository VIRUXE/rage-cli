//! `rage plot`: a readable top-down plan of an interior — rooms, portals,
//! props, collision, drawable shells and navmesh — drawn from whatever files
//! it is given. Parsing lives in rage-formats, drawing in rage-render; this
//! command only decides what belongs on the page.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rage_formats::{encode_image, rage_joaat, ImageFormat, MloDef, MloInstance, MloRoom, Vec3, YmapEntity};
use rage_render::{
    plan_png, plan_svg, quad_footprint, rooms_stacked, scene_bounds, EntityMark, Layer, Marker, NavClass, NavShape,
    PlanOptions, PlanReport, PortalShape, RoomShape, Scene, Tri, FLOOR_BAND,
};

use crate::plot_inputs::{self, Explicit, Mesh, PlotSources, SkipReason};
use crate::props::{PropResolver, PropShape};
use crate::rpf::GtaKeys;
use crate::utils::{parse_marker, parse_pair, parse_quad};

/// The drawable layers, as `--layers` spells them. Mirrors
/// [`rage_render::Layer`] so clap can list and validate them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LayerArg {
    /// Room boxes from the .ytyp, labelled with their names
    Rooms,
    /// The doorways and openings joining rooms
    Portals,
    /// The props the interior places, one dot each
    Entities,
    /// Triangles from the .ybn: floors shaded by height, walls outlined
    Collision,
    /// Triangles from the .ydr/.ydd shell
    Drawable,
    /// Navmesh polygons: where the game lets pedestrians walk
    Navmesh,
}

impl From<LayerArg> for Layer {
    fn from(l: LayerArg) -> Layer {
        match l {
            LayerArg::Rooms => Layer::Rooms,
            LayerArg::Portals => Layer::Portals,
            LayerArg::Entities => Layer::Entities,
            LayerArg::Collision => Layer::Collision,
            LayerArg::Drawable => Layer::Drawable,
            LayerArg::Navmesh => Layer::Navmesh,
        }
    }
}

#[derive(clap::Args)]
pub struct PlotArgs {
    /// Files (.ynv .ybn .ymap .ytyp .ydr .ydd), FiveM resource folders
    /// (scanned recursively), or vanilla MLO archetype names / 0x hashes
    /// (resolved through the game index)
    #[arg(required = true, num_args = 1..)]
    pub inputs: Vec<String>,

    /// A .ymap whose MLO instance places the interior in the world; without
    /// one the plan stays in the interior's own coordinates
    #[arg(long, value_name = "FILE")]
    pub ymap: Vec<PathBuf>,

    /// A .ytyp declaring the interior: its rooms, portals, entity sets and props
    #[arg(long, value_name = "FILE")]
    pub ytyp: Vec<PathBuf>,

    /// A .ybn to draw the collision from: the floors and walls the game
    /// actually stops you at. Named here it counts as the interior's own and
    /// is placed by the .ymap, whatever it is called; inside a scanned folder
    /// only a file named after the archetype is placed, the rest being
    /// vanilla map chunks that are already in world coordinates
    #[arg(long, value_name = "FILE")]
    pub ybn: Vec<PathBuf>,

    /// A .ydr/.ydd to draw the visible shell from; named here it is taken as
    /// the interior's own and placed by the .ymap, as with --ybn
    #[arg(long, value_name = "FILE")]
    pub ydr: Vec<PathBuf>,

    /// Layers to draw (comma separated)
    #[arg(long, value_delimiter = ',', default_value = "rooms,portals,entities,collision,drawable,navmesh")]
    pub layers: Vec<LayerArg>,

    /// Draw one storey: geometry from Z-0.3 to Z+2.0 m
    #[arg(long, value_name = "Z", conflicts_with = "z_range")]
    pub floor_z: Option<f32>,

    /// Draw only geometry between these heights
    #[arg(long, value_name = "LO,HI")]
    pub z_range: Option<String>,

    /// World XY box to draw: x0,y0,x1,y1 (default: extent of what is drawn plus 2 m)
    #[arg(long, value_name = "X0,Y0,X1,Y1")]
    pub region: Option<String>,

    /// Pixels per metre (default 30, lowered to fit the page when the
    /// area drawn is large)
    #[arg(long, value_name = "PX")]
    pub scale: Option<f32>,

    /// Markers to draw: "x,y,label"; repeatable
    #[arg(long, value_name = "X,Y,LABEL")]
    pub marker: Vec<String>,

    /// Label portals and props (rooms and markers are always labelled)
    #[arg(long)]
    pub labels: bool,

    /// How many distinct prop models an exterior map may draw; the rest are
    /// boxes or marks
    #[arg(long, default_value = "500", value_name = "N")]
    pub props: usize,

    /// Draw exterior map entities as marks only, no model or box
    #[arg(long)]
    pub no_props: bool,

    /// Heading for the page (default: the input, region and height band)
    #[arg(long)]
    pub title: Option<String>,

    /// JPEG/WebP quality
    #[arg(long, default_value = "90")]
    pub quality: u8,

    /// Where to write the plan; the extension picks the format (.png, .jpg, .webp, .svg)
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,
}

/// What the output extension asks for.
enum Output {
    Svg,
    Image(ImageFormat),
}

fn output_format(path: &Path) -> Result<Output> {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "svg" => Ok(Output::Svg),
        ext @ ("png" | "jpg" | "jpeg" | "webp") => Ok(Output::Image(ext.parse()?)),
        other => bail!("unsupported output extension '.{other}'; use .png, .jpg, .webp or .svg"),
    }
}

pub fn run(args: &PlotArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    // Everything the command line says is settled before a byte is read: a
    // typo in --region should not cost a walk over a whole resource folder.
    let region = args.region.as_deref().map(parse_quad).transpose().context("--region")?;
    let z_band = z_band(args)?;
    let markers: Vec<Marker> = args
        .marker
        .iter()
        .map(|m| {
            let (x, y, label) = parse_marker(m).context("--marker expects x,y,label")?;
            Ok(Marker { x, y, label })
        })
        .collect::<Result<_>>()?;
    let output = output_format(&args.output)?;

    let explicit = Explicit {
        ymap: args.ymap.clone(),
        ytyp: args.ytyp.clone(),
        ybn: args.ybn.clone(),
        ydr: args.ydr.clone(),
    };
    let sources = plot_inputs::resolve(&args.inputs, &explicit, keys, exe)?;

    let placement = find_placement(&sources);
    let mut props = PropResolver::new(&sources, keys, exe, !args.no_props, args.props);
    let scene = build_scene(args, &sources, &placement, z_band, region, markers, &mut props)?;

    if z_band.is_none() {
        let stacked = rooms_stacked(&scene.rooms);
        if let Some(warning) = stacked_rooms_warning(&scene.rooms, &stacked) {
            eprintln!("{warning}");
        }
    }

    let layers: Vec<Layer> = args.layers.iter().map(|l| Layer::from(*l)).collect();
    let defaults = PlanOptions::default();
    let scale = match args.scale {
        Some(scale) => scale,
        None => {
            let framed = region.or_else(|| scene_bounds(&scene, &layers, z_band));
            let scale = fit_scale(framed, DEFAULT_SCALE, defaults.max_pixels);
            if scale < DEFAULT_SCALE {
                eprintln!("scale lowered to {scale} px/m to fit the page; pass --scale or --region for more detail");
            }
            scale
        }
    };
    let opts = PlanOptions { region, scale, z_band, layers, labels: args.labels, ..defaults };

    let report = match output {
        Output::Svg => {
            let (svg, report) = plan_svg(&scene, &opts)?;
            std::fs::write(&args.output, svg).with_context(|| format!("writing {}", args.output.display()))?;
            report
        }
        Output::Image(format) => {
            let (img, report) = plan_png(&scene, &opts)?;
            let bytes = encode_image(&img, format, args.quality)?;
            std::fs::write(&args.output, bytes).with_context(|| format!("writing {}", args.output.display()))?;
            report
        }
    };

    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    println!("Wrote {} ({})", args.output.display(), summary(&report));
    Ok(())
}

/// The usual pixels per metre, when nothing asks for another.
const DEFAULT_SCALE: f32 = 30.0;

/// `wanted` px/m, or as much as keeps a page of `framed` (with the 2 m pad
/// the planner adds each side) within `max_pixels`, rounded down to a half.
fn fit_scale(framed: Option<[f32; 4]>, wanted: f32, max_pixels: u64) -> f32 {
    let Some([x0, y0, x1, y1]) = framed else { return wanted };
    let (w, h) = ((x1 - x0).abs() as f64 + 4.0, (y1 - y0).abs() as f64 + 4.0);
    if !(w.is_finite() && h.is_finite()) || w * h <= 0.0 {
        return wanted;
    }
    // The page also carries a header and legend; leave a fifth for them.
    let fits = (max_pixels as f64 * 0.8 / (w * h)).sqrt();
    if fits >= wanted as f64 {
        return wanted;
    }
    ((fits * 2.0).floor() / 2.0).max(0.5) as f32
}

/// `620x540 px, region -585.0,-1070.0..-570.0,-1055.0; 5 rooms, 42 navmesh polys`.
fn summary(report: &PlanReport) -> String {
    let [x0, y0, x1, y1] = report.region;
    let counts: Vec<String> = report
        .drawn
        .iter()
        .map(|(layer, n)| {
            let noun = match layer {
                Layer::Rooms => "rooms",
                Layer::Portals => "portals",
                Layer::Entities => "entities",
                Layer::Collision => "collision tris",
                Layer::Drawable => "drawable tris",
                Layer::Navmesh => "navmesh polys",
            };
            format!("{n} {noun}")
        })
        .collect();
    format!(
        "{}x{} px, region {x0:.1},{y0:.1}..{x1:.1},{y1:.1}; {}",
        report.width,
        report.height,
        counts.join(", ")
    )
}

/// Where the interior sits in the world, and which file said so.
struct Placement {
    entity: Option<YmapEntity>,
    /// The .ymap the placement came from, for the caption.
    source: Option<String>,
    /// The instance that placed it, which lists the entity sets that are on
    /// by default.
    instance: Option<MloInstance>,
    /// The MLO definition the plan is of.
    mlo: Option<MloDef>,
    /// How many interiors the .ytyp files between them declare: more than
    /// one and only a .ymap can say which is meant.
    mlo_count: usize,
}

impl Placement {
    /// Interior coordinates to world coordinates, or identity when nothing
    /// places the interior.
    fn to_world(&self, v: Vec3) -> Vec3 {
        match &self.entity {
            Some(e) => e.to_world(v),
            None => v,
        }
    }
}

/// The MLO instance that places a loaded interior; failing that the first MLO
/// instance, then the first entity of the first .ymap — the same order the
/// navmesh tools have always used.
fn find_placement(sources: &PlotSources) -> Placement {
    let mlos: Vec<&MloDef> = sources.ytyps.iter().flat_map(|(_, y)| &y.mlos).collect();

    for (name, _, instances) in &sources.ymaps {
        for instance in instances {
            if let Some(mlo) = mlos.iter().find(|m| m.name_hash == instance.entity.archetype_hash) {
                return Placement {
                    entity: Some(instance.entity),
                    source: Some(name.clone()),
                    instance: Some(instance.clone()),
                    mlo: Some((*mlo).clone()),
                    mlo_count: mlos.len(),
                };
            }
        }
    }
    let mlo = mlos.first().map(|m| (*m).clone());
    let mlo_count = mlos.len();
    for (name, _, instances) in &sources.ymaps {
        if let Some(instance) = instances.first() {
            return Placement {
                entity: Some(instance.entity),
                source: Some(name.clone()),
                instance: Some(instance.clone()),
                mlo,
                mlo_count,
            };
        }
    }
    // A plain entity places an interior only when there is an interior to
    // place; the entities of an exterior map are drawn where they are.
    if mlo.is_some() {
        for (name, entities, _) in &sources.ymaps {
            if let Some(entity) = entities.first() {
                return Placement { entity: Some(*entity), source: Some(name.clone()), instance: None, mlo, mlo_count };
            }
        }
    }
    Placement { entity: None, source: None, instance: None, mlo, mlo_count }
}

/// Whether a mesh file is the interior's own — and so has to go through the
/// placement — or a world-space map chunk shipped in the same folder, which
/// is already where it belongs. Reports every world-space file it decides on,
/// but only when a placement is in play: with nothing to transform, the
/// distinction makes no difference to the page.
fn is_interior_mesh<T>(mesh: &Mesh<T>, placement: &Placement, what: &str) -> bool {
    if mesh.explicit {
        return true;
    }
    let interior = placement.mlo.as_ref().is_some_and(|mlo| mesh_belongs_to(&mesh.name, mlo.name_hash));
    if !interior && placement.entity.is_some() {
        eprintln!("{}: not the interior's own {what}; drawn in world space", mesh.name);
    }
    interior
}

/// True when `file_name` names the archetype `mlo_name_hash`. The name may
/// arrive as a path — a file inside the game's archives is known by its inner
/// path — so only the last component counts; and a resource may ship the same
/// mesh at several detail levels (`hi@name.ybn`), so the LOD prefix comes off
/// before the name is hashed.
fn mesh_belongs_to(file_name: &str, mlo_name_hash: u32) -> bool {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = base.rsplit_once('.').map_or(base, |(s, _)| s).to_lowercase();
    let stem = ["hi@", "ma@", "lo@"].iter().find_map(|p| stem.strip_prefix(p)).unwrap_or(stem.as_str());
    rage_joaat(stem) == mlo_name_hash
}

/// The one-line note about rooms sitting on top of each other, or `None` when
/// none do. `rooms_stacked` reports `RoomShape::index` values, which are the
/// MLO's own room numbers — not positions in `rooms`, since a room whose
/// footprint could not be estimated is left out.
fn stacked_rooms_warning(rooms: &[RoomShape], stacked: &[(usize, usize)]) -> Option<String> {
    let &(a, b) = stacked.first()?;
    let find = |index: usize| rooms.iter().find(|r| r.index == index);
    let (a, b) = (find(a)?, find(b)?);
    let (u, l) = if a.z_lo >= b.z_lo { (a, b) } else { (b, a) };
    let more = match stacked.len() {
        1 => String::new(),
        n => format!(" and {} more", n - 1),
    };
    Some(format!(
        "warning: rooms stack vertically (r{} '{}' {:.1}..{:.1} over r{} '{}' {:.1}..{:.1}{more}); use --floor-z Z or --z-range LO,HI to draw one storey",
        u.index, u.name, u.z_lo, u.z_hi, l.index, l.name, l.z_lo, l.z_hi
    ))
}

/// True when every room but limbo stores its box as half-extents about the
/// MLO origin rather than where the room is, which means the .ytyp never
/// recorded the positions and drawing the boxes would stack every room
/// concentrically at the interior's centre.
///
/// Most such rooms are exactly symmetric (`min == -max`); a few are a little
/// off, so a room counts as centred when its centre is well inside its own
/// half-extent. Requiring it of every room keeps a real layout — whose rooms
/// are metres apart — from ever matching.
///
/// Some vanilla interiors (v_bahama) store the same kind of half-extents
/// around a point that is not the origin. Those show up another way: every
/// room's centre lies inside the other rooms' boxes, whereas the rooms of a
/// real layout sit side by side. When most rooms nest like that, the boxes
/// are not positions either.
fn rooms_unpositioned(rooms: &[MloRoom]) -> bool {
    let live: Vec<&MloRoom> = rooms.iter().skip(1).collect();
    if live.len() < 2 {
        return false;
    }
    let centred = live.iter().all(|r| centred_on_origin(r.bb_min.x, r.bb_max.x) && centred_on_origin(r.bb_min.y, r.bb_max.y));
    let nested = live
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            let cx = (r.bb_min.x + r.bb_max.x) / 2.0;
            let cy = (r.bb_min.y + r.bb_max.y) / 2.0;
            live.iter().enumerate().any(|(j, other)| {
                j != *i && cx > other.bb_min.x && cx < other.bb_max.x && cy > other.bb_min.y && cy < other.bb_max.y
            })
        })
        .count();
    centred || nested * 2 > live.len()
}

fn centred_on_origin(lo: f32, hi: f32) -> bool {
    let centre = (lo + hi) / 2.0;
    let half = (hi - lo) / 2.0;
    centre.abs() <= 0.025f32.max(half / 2.0)
}

/// Where a room really is, taken from what is attached to it: the props it
/// owns and the corners of the portals opening into it. `None` when fewer
/// than three points vouch for it, which is too little to call a footprint.
fn estimate_room_box(mlo: &MloDef, room: usize) -> Option<(Vec3, Vec3)> {
    let mut points: Vec<Vec3> = mlo.rooms[room]
        .attached_objects
        .iter()
        .filter_map(|i| mlo.entities.get(*i as usize))
        .map(|e| e.position)
        .collect();
    for portal in &mlo.portals {
        if portal.room_from as usize == room || portal.room_to as usize == room {
            points.extend(portal.corners.iter().copied());
        }
    }
    if points.len() < 3 {
        return None;
    }
    let fold = |pick: fn(f32, f32) -> f32, get: fn(&Vec3) -> f32| points.iter().map(get).fold(f32::NAN, pick);
    Some((
        Vec3::new(fold(f32::min, |v| v.x), fold(f32::min, |v| v.y), fold(f32::min, |v| v.z)),
        Vec3::new(fold(f32::max, |v| v.x), fold(f32::max, |v| v.y), fold(f32::max, |v| v.z)),
    ))
}

/// The height band to draw, from `--floor-z` or `--z-range`.
fn z_band(args: &PlotArgs) -> Result<Option<(f32, f32)>> {
    if let Some(z) = args.floor_z {
        return Ok(Some((z + FLOOR_BAND.0, z + FLOOR_BAND.1)));
    }
    match &args.z_range {
        Some(range) => {
            let (lo, hi) = parse_pair(range).context("--z-range expects LO,HI")?;
            Ok(Some((lo.min(hi), lo.max(hi))))
        }
        None => Ok(None),
    }
}

fn build_scene(
    args: &PlotArgs, sources: &PlotSources, placement: &Placement, z_band: Option<(f32, f32)>,
    region: Option<[f32; 4]>, markers: Vec<Marker>, props: &mut PropResolver<'_>,
) -> Result<Scene> {
    let mut scene = Scene::default();
    let names = crate::names::load(&[], None).unwrap_or_else(|_| rage_formats::NameTable::core());
    let name_of = |hash: u32| sources.names.get(&hash).cloned().unwrap_or_else(|| names.resolve(hash).into_owned());
    let mut estimated_rooms = false;
    // How much of the page comes from the interior itself (and so needs
    // the .ymap to place it), as against exterior entities already in
    // world space.
    let mut interior_marks = 0usize;

    if let Some(mlo) = &placement.mlo {
        // Rooms and portals are stored axis-aligned in the interior's own
        // space; MLO instances are placed with a yaw-only rotation in
        // practice, so a transformed box stays a box on the page.
        estimated_rooms = rooms_unpositioned(&mlo.rooms);
        for (i, room) in mlo.rooms.iter().enumerate() {
            // Room 0 is limbo, the world outside: it has no props of its own
            // to estimate from, and its box is meant to swallow the interior.
            let box_ = match estimated_rooms && i != 0 {
                true => match estimate_room_box(mlo, i) {
                    Some(b) => b,
                    None => continue,
                },
                false => (room.bb_min, room.bb_max),
            };
            let (bb_min, bb_max) = box_;
            let footprint = quad_footprint(bb_min, bb_max, |v| placement.to_world(v));
            // A yaw-only placement turns about z, so a point's world z
            // depends on its own z alone: the box's two z extremes bound it.
            let z_a = placement.to_world(Vec3::new(bb_min.x, bb_min.y, bb_min.z)).z;
            let z_b = placement.to_world(Vec3::new(bb_min.x, bb_min.y, bb_max.z)).z;
            let (z_lo, z_hi) = (z_a.min(z_b), z_a.max(z_b));
            scene.rooms.push(RoomShape { index: i, name: room.name.clone(), footprint, z_lo, z_hi });
        }
        for (i, portal) in mlo.portals.iter().enumerate() {
            scene.portals.push(PortalShape {
                index: i,
                room_from: portal.room_from as usize,
                room_to: portal.room_to as usize,
                corners: portal.corners.iter().map(|c| placement.to_world(*c)).collect(),
            });
        }
        for entity in &mlo.entities {
            scene.entities.push(EntityMark {
                position: placement.to_world(entity.position),
                label: name_of(entity.archetype_hash),
                set: None,
                faded: false,
            });
            interior_marks += 1;
        }
        // An instance that names its default entity sets tells us which of
        // the interior's optional prop sets are actually switched on; the
        // rest are drawn faded rather than dropped.
        let defaults = placement.instance.as_ref().map(|i| i.default_entity_sets.clone()).unwrap_or_default();
        for (k, set) in mlo.entity_sets.iter().enumerate() {
            scene.entity_set_names.push(name_of(set.name_hash));
            let faded = !defaults.is_empty() && !defaults.contains(&set.name_hash);
            for entity in &set.entities {
                scene.entities.push(EntityMark {
                    position: placement.to_world(entity.position),
                    label: name_of(entity.archetype_hash),
                    set: Some(k),
                    faded,
                });
                interior_marks += 1;
            }
        }
    }

    // The entities of exterior maps: each one marked where it stands, with
    // its model placed there when one can be found, else its box. On a plot
    // of an interior only its surroundings are of interest — the vanilla
    // chunks a resource ships place hundreds of entities over a kilometre,
    // which would frame the page around the whole block.
    let neighbourhood = placement.mlo.as_ref().map(|_| interior_neighbourhood(&scene, placement));
    let mut exterior_entities = 0usize;
    let mut exterior_maps = 0usize;
    let mut exterior_skipped = 0usize;
    for (_, entities, _) in &sources.ymaps {
        let mut any = false;
        for entity in entities.iter().filter(|e| !e.is_mlo_instance) {
            if let Some([x0, y0, x1, y1]) = neighbourhood
                && !(entity.position.x >= x0 && entity.position.x <= x1 && entity.position.y >= y0 && entity.position.y <= y1)
            {
                exterior_skipped += 1;
                continue;
            }
            any = true;
            exterior_entities += 1;
            scene.entities.push(EntityMark { position: entity.position, label: name_of(entity.archetype_hash), set: None, faded: false });
            let put = |v: Vec3| entity.to_world(v);
            match props.resolve(entity.archetype_hash) {
                PropShape::Folder { entry, member } => {
                    let data = &sources.drawables[entry].data;
                    let chosen: Vec<&rage_formats::Drawable> = match member {
                        Some(m) => data.get(m).into_iter().collect(),
                        None => data.iter().collect(),
                    };
                    for drawable in chosen {
                        push_drawable_triangles(&mut scene.drawable, drawable, put);
                    }
                }
                PropShape::Game { drawables, member } => {
                    let chosen: Vec<&rage_formats::Drawable> = match member {
                        Some(m) => drawables.get(m).into_iter().collect(),
                        None => drawables.iter().collect(),
                    };
                    for drawable in chosen {
                        push_drawable_triangles(&mut scene.drawable, drawable, put);
                    }
                }
                PropShape::Box(lo, hi) => push_box_footprint(&mut scene.drawable, lo, hi, put),
                PropShape::None => {}
            }
        }
        exterior_maps += usize::from(any);
    }

    let mut placed_meshes = 0usize;
    for ybn in &sources.ybns {
        let placed = is_interior_mesh(ybn, placement, "collision");
        placed_meshes += usize::from(placed);
        for tri in ybn.data.triangles() {
            scene.collision.push(Tri { v: tri.vertices.map(|v| if placed { placement.to_world(v) } else { v }) });
        }
    }

    // A drawable that belongs to the interior is its shell, stored in the
    // same local space as the rooms. A drawable that stands for an exterior
    // map's archetype was drawn above, once per placement, and would only
    // ghost at the origin here.
    for (i, entry) in sources.drawables.iter().enumerate() {
        if props.matched_folder.contains(&i) {
            continue;
        }
        let placed = is_interior_mesh(entry, placement, "shell");
        placed_meshes += usize::from(placed);
        let put = |v: Vec3| if placed { placement.to_world(v) } else { v };
        for drawable in &entry.data {
            push_drawable_triangles(&mut scene.drawable, drawable, put);
        }
    }

    for (_, ynv) in &sources.ynvs {
        for poly in &ynv.polys {
            // A polygon pinned to the cell's floor is a sunk one: the game
            // never walks it, and it usually marks a build gone wrong.
            let class = if poly.vertices.iter().all(|v| (v.z - ynv.bb_min.z).abs() < 1e-3) {
                NavClass::Sunk
            } else if poly.is_interior() {
                NavClass::Interior
            } else {
                NavClass::Exterior
            };
            scene.navmesh.push(NavShape { vertices: poly.vertices.clone(), class });
        }
    }

    scene.markers = markers;

    // Only geometry stored in the interior's own space needs a .ymap to say
    // where it is; a navmesh cell on its own is already in world coordinates.
    let needs_placing = !scene.rooms.is_empty() || !scene.portals.is_empty() || interior_marks > 0 || placed_meshes > 0;
    if !sources.ynvs.is_empty() && placement.entity.is_none() && needs_placing {
        eprintln!("warning: navmesh polygons are in world coordinates but the rest of the plan is in the interior's own; pass the .ymap that places it");
    }

    let band = match z_band {
        Some((lo, hi)) => format!("z {lo:.1}..{hi:.1} m"),
        None => "all heights".to_string(),
    };
    scene.title = match &args.title {
        Some(title) => title.clone(),
        None => match region {
            Some([x0, y0, x1, y1]) => {
                format!("{} — {x0:.1},{y0:.1}..{x1:.1},{y1:.1} — {band}", sources.label)
            }
            None => format!("{} — {band}", sources.label),
        },
    };
    match &placement.source {
        Some(ymap) => scene.caption.push(format!("placement: {ymap}")),
        None if needs_placing => scene.caption.push("MLO-local coordinates (no ymap given)".to_string()),
        None => {}
    }
    // With no .ymap, nothing says which of several declared interiors was
    // meant, so the first is drawn and the page says so.
    if placement.source.is_none() && placement.mlo_count > 1 {
        if let Some(mlo) = &placement.mlo {
            let line = format!(
                "{} interiors loaded, drawing {} (give the .ymap to pick one)",
                placement.mlo_count,
                name_of(mlo.name_hash)
            );
            eprintln!("warning: {line}");
            scene.caption.push(line);
        }
    }
    scene.caption.push(band);
    if exterior_entities > 0 {
        let maps = if exterior_maps == 1 { "ymap" } else { "ymaps" };
        let around = if neighbourhood.is_some() { " around the interior" } else { "" };
        scene.caption.push(format!("{exterior_entities} entities from {exterior_maps} {maps}{around}"));
        if props.stats.archetypes > 0 && !args.no_props {
            scene.caption.push(props.stats.line());
        }
        if props.stats.over_budget > 0 {
            eprintln!("warning: {} archetypes past --props {}; drawn as boxes or marks", props.stats.over_budget, args.props);
        }
    }
    if exterior_skipped > 0 {
        eprintln!("{exterior_skipped} entities of the folder's exterior maps lie beyond the interior's surroundings; not drawn");
    }
    if scene.drawable.len() > 2_000_000 {
        eprintln!("warning: {} drawable triangles; --region, --no-props or --layers can keep the page tractable", scene.drawable.len());
    }
    if estimated_rooms {
        scene.caption.push("room boxes estimated from props and portals (ytyp boxes are unpositioned)".to_string());
    }
    let escrow = sources.skipped.iter().filter(|s| s.reason == SkipReason::Escrow).count();
    let unreadable = sources.skipped.len() - escrow;
    if escrow + unreadable > 0 {
        let mut parts = Vec::new();
        if escrow > 0 {
            parts.push(format!("{escrow} escrow-encrypted {}", if escrow == 1 { "file" } else { "files" }));
        }
        if unreadable > 0 {
            parts.push(format!("{unreadable} unreadable"));
        }
        scene.caption.push(format!("not drawn: {}", parts.join(", ")));
    }
    for note in &sources.notes {
        scene.caption.push(note.clone());
    }
    Ok(scene)
}

/// How far around the interior an exterior entity still belongs on its
/// page: the world-space box of its rooms and portals (or, when those are
/// unpositioned, the placement itself) grown by this much.
const NEIGHBOURHOOD_METRES: f32 = 25.0;

/// `x0,y0,x1,y1` around what the interior occupies in the world.
fn interior_neighbourhood(scene: &Scene, placement: &Placement) -> [f32; 4] {
    fn grow(bb: &mut Option<[f32; 4]>, x: f32, y: f32) {
        *bb = Some(match *bb {
            None => [x, y, x, y],
            Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x), y1.max(y)],
        });
    }
    let mut bb: Option<[f32; 4]> = None;
    for room in scene.rooms.iter().skip(1) {
        for v in &room.footprint {
            grow(&mut bb, v.x, v.y);
        }
    }
    for portal in &scene.portals {
        for c in &portal.corners {
            grow(&mut bb, c.x, c.y);
        }
    }
    if bb.is_none() {
        let p = placement.to_world(Vec3::new(0.0, 0.0, 0.0));
        grow(&mut bb, p.x, p.y);
    }
    let [x0, y0, x1, y1] = bb.unwrap_or([0.0; 4]);
    let m = NEIGHBOURHOOD_METRES;
    [x0 - m, y0 - m, x1 + m, y1 + m]
}

/// The best LOD's triangles of `drawable`, through `put`.
fn push_drawable_triangles(out: &mut Vec<Tri>, drawable: &rage_formats::Drawable, put: impl Fn(Vec3) -> Vec3) {
    let Some(lod) = drawable.best_lod() else { return };
    for model in &lod.models {
        for geometry in &model.geometries {
            let (Some(vertices), Some(indices)) = (&geometry.vertex_buffer, &geometry.index_buffer) else {
                continue;
            };
            let Ok(unified) = vertices.to_unified_vertices() else { continue };
            for face in indices.indices.chunks_exact(3) {
                let corner = |i: u32| unified.get(i as usize).map(|v| put(v.position));
                if let (Some(a), Some(b), Some(c)) = (corner(face[0]), corner(face[1]), corner(face[2])) {
                    out.push(Tri { v: [a, b, c] });
                }
            }
        }
    }
}

/// An archetype's box as two triangles on its floor, through `put`: what a
/// prop occupies when its model is not to hand.
fn push_box_footprint(out: &mut Vec<Tri>, lo: Vec3, hi: Vec3, put: impl Fn(Vec3) -> Vec3) {
    let a = put(Vec3::new(lo.x, lo.y, lo.z));
    let b = put(Vec3::new(hi.x, lo.y, lo.z));
    let c = put(Vec3::new(hi.x, hi.y, lo.z));
    let d = put(Vec3::new(lo.x, hi.y, lo.z));
    out.push(Tri { v: [a, b, c] });
    out.push(Tri { v: [a, c, d] });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rage_formats::{MloPortal, MloRoom};
    use rage_formats::Vec2;

    fn room(name: &str, bb_min: Vec3, bb_max: Vec3, attached: &[u32]) -> MloRoom {
        MloRoom {
            name: name.to_string(),
            bb_min,
            bb_max,
            flags: 0,
            floor_id: 0,
            attached_objects: attached.to_vec(),
        }
    }

    fn entity(position: Vec3) -> YmapEntity {
        YmapEntity {
            archetype_hash: 0,
            flags: 0,
            guid: 0,
            position,
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale_xy: 1.0,
            scale_z: 1.0,
            parent_index: -1,
            lod_dist: 0.0,
            is_mlo_instance: false,
        }
    }

    fn portal(room_from: u32, room_to: u32, corners: Vec<Vec3>) -> MloPortal {
        MloPortal {
            room_from,
            room_to,
            flags: 0,
            mirror_priority: 0,
            opacity: 0,
            audio_occlusion: 0,
            corners,
            attached_objects: Vec::new(),
        }
    }

    #[test]
    fn the_scale_fits_large_areas_and_leaves_small_ones_alone() {
        assert_eq!(fit_scale(None, 30.0, 40_000_000), 30.0);
        assert_eq!(fit_scale(Some([0.0, 0.0, 20.0, 10.0]), 30.0, 40_000_000), 30.0);
        // A kilometre square at 30 px/m would be 900 Mpx; it comes down.
        let s = fit_scale(Some([0.0, 0.0, 1000.0, 1000.0]), 30.0, 40_000_000);
        assert!(s < 6.0 && s >= 0.5, "{s}");
        assert_eq!(s * 2.0, (s * 2.0).floor(), "rounded to a half");
        assert!((1004.0 * s as f64).powi(2) <= 40_000_000.0 * 0.8 + 1.0);
    }

    #[test]
    fn a_mesh_named_after_the_archetype_is_the_interiors_own() {
        let hash = rage_joaat("denis3d_catcafe");
        assert!(mesh_belongs_to("denis3d_catcafe.ybn", hash));
        assert!(mesh_belongs_to("hi@denis3d_catcafe.ybn", hash), "a LOD prefix is still the same mesh");
        assert!(mesh_belongs_to("MA@Denis3d_CatCafe.ydr", hash), "the name is hashed lowercase");
        assert!(!mesh_belongs_to("kt1_15_0.ybn", hash), "a vanilla map chunk is world-space");
        assert!(!mesh_belongs_to("hei_kt1_rd_4.ybn", hash));

        // A file inside the game's archives is known by its inner path.
        let bahama = rage_joaat("v_bahama");
        assert!(mesh_belongs_to("levels/gta5/_citye/beverly_01/bh1_08.rpf/v_bahama.ybn", bahama));
        assert!(mesh_belongs_to("levels\\gta5\\props.rpf\\hi@v_bahama.ydr", bahama));
        assert!(!mesh_belongs_to("levels/gta5/_citye/bh1_08.rpf/bh1_08_details.ybn", bahama));
    }

    #[test]
    fn the_stacked_rooms_warning_names_rooms_by_their_mlo_index() {
        let shape = |index: usize, name: &str, z_lo: f32, z_hi: f32| RoomShape {
            index,
            name: name.to_string(),
            footprint: [Vec2::new(0.0, 0.0), Vec2::new(4.0, 0.0), Vec2::new(4.0, 4.0), Vec2::new(0.0, 4.0)],
            z_lo,
            z_hi,
        };
        // Room 2 was dropped for want of points to estimate it from, so the
        // indices `rooms_stacked` reports are past the end of the list.
        let rooms = [shape(0, "limbo", 0.0, 9.0), shape(1, "basement", 17.0, 20.5), shape(3, "kitchen", 21.0, 24.0)];

        let warning = stacked_rooms_warning(&rooms, &[(1, 3), (3, 1)]).expect("a pair was given");
        assert!(warning.contains("r3 'kitchen' 21.0..24.0 over r1 'basement' 17.0..20.5"), "{warning}");
        assert!(warning.contains("and 1 more"), "{warning}");
        assert!(warning.contains("--floor-z"), "{warning}");

        assert!(stacked_rooms_warning(&rooms, &[]).is_none());
        assert!(stacked_rooms_warning(&rooms, &[(1, 2)]).is_none(), "a dropped room cannot be named");
    }

    #[test]
    fn rooms_are_unpositioned_when_every_box_is_centred_on_the_origin() {
        let limbo = room("limbo", Vec3::new(-100.0, -100.0, -50.0), Vec3::new(100.0, 100.0, 50.0), &[]);
        let half = |x: f32, y: f32| room("r", Vec3::new(-x, -y, 0.0), Vec3::new(x, y, 3.0), &[]);
        assert!(rooms_unpositioned(&[limbo.clone(), half(4.0, 3.0), half(9.0, 2.0)]));

        // As the cat cafe stores them: mostly exact half-extents, a few rooms
        // a little off centre but nowhere near their real position.
        let nearly = room("r10", Vec3::new(-2.60, -6.25, -2.21), Vec3::new(3.62, 6.68, 1.84), &[]);
        assert!(rooms_unpositioned(&[limbo.clone(), half(7.48, 7.45), nearly]));

        let placed = room("kitchen", Vec3::new(2.0, 1.0, 0.0), Vec3::new(6.0, 4.0, 3.0), &[]);
        assert!(!rooms_unpositioned(&[limbo.clone(), half(4.0, 3.0), placed]));
        assert!(!rooms_unpositioned(&[limbo, half(4.0, 3.0)]), "one room is not a pattern");
        assert!(!rooms_unpositioned(&[]));
    }

    #[test]
    fn rooms_are_unpositioned_when_they_nest_inside_each_other() {
        // As v_bahama stores them: half-extents around a point that is not
        // the origin, so every room's centre sits inside the others' boxes.
        // Real rooms are side by side, never one inside the next.
        let limbo = room("limbo", Vec3::new(9.0, 6.0, 0.0), Vec3::new(11.0, 8.0, 3.0), &[]);
        let dance = room("dancefloor", Vec3::new(2.0, 1.0, 0.0), Vec3::new(18.0, 13.0, 3.0), &[]);
        let club = room("clubroom", Vec3::new(6.0, 4.0, 0.0), Vec3::new(14.0, 10.0, 3.0), &[]);
        let entry = room("entry", Vec3::new(8.5, 5.5, 0.0), Vec3::new(12.0, 9.0, 3.0), &[]);
        assert!(rooms_unpositioned(&[limbo.clone(), dance.clone(), club.clone(), entry.clone()]));

        // The same three rooms laid out next to each other stay as stored,
        // even though the dance floor's box overlaps the club's a little.
        let club_beside = room("clubroom", Vec3::new(17.0, 1.0, 0.0), Vec3::new(25.0, 7.0, 3.0), &[]);
        let entry_beside = room("entry", Vec3::new(2.0, 14.0, 0.0), Vec3::new(5.5, 17.5, 3.0), &[]);
        assert!(!rooms_unpositioned(&[limbo, dance, club_beside, entry_beside]));
    }

    #[test]
    fn a_rooms_box_is_estimated_from_its_props_and_portals() {
        let mlo = MloDef {
            name_hash: 0,
            entities: vec![entity(Vec3::new(1.0, 1.0, 0.0)), entity(Vec3::new(3.0, 2.0, 1.0))],
            rooms: vec![
                room("limbo", Vec3::new(-9.0, -9.0, -9.0), Vec3::new(9.0, 9.0, 9.0), &[]),
                room("kitchen", Vec3::new(-4.0, -3.0, 0.0), Vec3::new(4.0, 3.0, 2.5), &[0, 1]),
                room("empty", Vec3::new(-4.0, -3.0, 0.0), Vec3::new(4.0, 3.0, 2.5), &[]),
            ],
            portals: vec![portal(1, 2, vec![Vec3::new(5.0, 0.0, 0.0), Vec3::new(5.0, 4.0, 2.5)])],
            entity_sets: Vec::new(),
        };

        let (min, max) = estimate_room_box(&mlo, 1).expect("two props and a portal are enough");
        assert_eq!((min.x, min.y, min.z), (1.0, 0.0, 0.0));
        assert_eq!((max.x, max.y, max.z), (5.0, 4.0, 2.5));

        // Room 2 is vouched for by the portal's two corners alone.
        assert!(estimate_room_box(&mlo, 2).is_none(), "two points are not a footprint");
    }
}
