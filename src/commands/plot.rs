//! `rage plot`: a readable top-down plan of an interior — rooms, portals,
//! props, collision, drawable shells and navmesh — drawn from whatever files
//! it is given. Parsing lives in rage-formats, drawing in rage-render; this
//! command only decides what belongs on the page.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rage_formats::{encode_image, rage_joaat, ImageFormat, MloDef, MloInstance, MloRoom, Vec3, YmapEntity};
use rage_render::{
    plan_png, plan_svg, quad_footprint, rooms_stacked, EntityMark, Layer, Marker, NavClass, NavShape, PlanOptions,
    PlanReport, PortalShape, RoomShape, Scene, Tri, FLOOR_BAND,
};

use crate::plot_inputs::{self, Explicit, Mesh, PlotSources, SkipReason};
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

    /// Pixels per metre
    #[arg(long, default_value = "30")]
    pub scale: f32,

    /// Markers to draw: "x,y,label"; repeatable
    #[arg(long, value_name = "X,Y,LABEL")]
    pub marker: Vec<String>,

    /// Label portals and props (rooms and markers are always labelled)
    #[arg(long)]
    pub labels: bool,

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
    let scene = build_scene(args, &sources, &placement, z_band, region, markers)?;

    if z_band.is_none() {
        let stacked = rooms_stacked(&scene.rooms);
        if let Some(warning) = stacked_rooms_warning(&scene.rooms, &stacked) {
            eprintln!("{warning}");
        }
    }

    let opts = PlanOptions {
        region,
        scale: args.scale,
        z_band,
        layers: args.layers.iter().map(|l| Layer::from(*l)).collect(),
        labels: args.labels,
        ..Default::default()
    };

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
                };
            }
        }
    }
    let mlo = mlos.first().map(|m| (*m).clone());
    for (name, _, instances) in &sources.ymaps {
        if let Some(instance) = instances.first() {
            return Placement {
                entity: Some(instance.entity),
                source: Some(name.clone()),
                instance: Some(instance.clone()),
                mlo,
            };
        }
    }
    for (name, entities, _) in &sources.ymaps {
        if let Some(entity) = entities.first() {
            return Placement { entity: Some(*entity), source: Some(name.clone()), instance: None, mlo };
        }
    }
    Placement { entity: None, source: None, instance: None, mlo }
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
fn rooms_unpositioned(rooms: &[MloRoom]) -> bool {
    let live: Vec<&MloRoom> = rooms.iter().skip(1).collect();
    live.len() >= 2 && live.iter().all(|r| centred_on_origin(r.bb_min.x, r.bb_max.x) && centred_on_origin(r.bb_min.y, r.bb_max.y))
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
    region: Option<[f32; 4]>, markers: Vec<Marker>,
) -> Result<Scene> {
    let mut scene = Scene::default();
    let name_of = |hash: u32| sources.names.get(&hash).cloned().unwrap_or_else(|| format!("{hash:#010x}"));
    let mut estimated_rooms = false;

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
            }
        }
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
    // same local space as the rooms; props are not placed per entity yet.
    for entry in &sources.drawables {
        let placed = is_interior_mesh(entry, placement, "shell");
        placed_meshes += usize::from(placed);
        let put = |v: Vec3| if placed { placement.to_world(v) } else { v };
        for drawable in &entry.data {
            let Some(lod) = drawable.best_lod() else { continue };
            for model in &lod.models {
                for geometry in &model.geometries {
                    let (Some(vertices), Some(indices)) = (&geometry.vertex_buffer, &geometry.index_buffer) else {
                        continue;
                    };
                    let Ok(unified) = vertices.to_unified_vertices() else { continue };
                    for face in indices.indices.chunks_exact(3) {
                        let corner = |i: u32| unified.get(i as usize).map(|v| put(v.position));
                        if let (Some(a), Some(b), Some(c)) = (corner(face[0]), corner(face[1]), corner(face[2])) {
                            scene.drawable.push(Tri { v: [a, b, c] });
                        }
                    }
                }
            }
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
    let needs_placing =
        !scene.rooms.is_empty() || !scene.portals.is_empty() || !scene.entities.is_empty() || placed_meshes > 0;
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
    scene.caption.push(band);
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
