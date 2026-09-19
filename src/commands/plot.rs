//! `rage plot`: a readable top-down plan of an interior — rooms, portals,
//! props, collision, drawable shells and navmesh — drawn from whatever files
//! it is given. Parsing lives in rage-formats, drawing in rage-render; this
//! command only decides what belongs on the page.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rage_formats::{encode_image, ImageFormat, MloDef, MloInstance, Vec3, YmapEntity};
use rage_render::{
    plan_png, plan_svg, quad_footprint, rooms_stacked, EntityMark, Layer, Marker, NavClass, NavShape, PlanOptions,
    PlanReport, PortalShape, RoomShape, Scene, Tri, FLOOR_BAND,
};

use crate::plot_inputs::{self, Explicit, PlotSources};
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
    /// actually stops you at
    #[arg(long, value_name = "FILE")]
    pub ybn: Vec<PathBuf>,

    /// A .ydr/.ydd to draw the visible shell from (in the interior's own
    /// coordinates, as the MLO stores it)
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

pub fn run(args: &PlotArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    let explicit = Explicit {
        ymap: args.ymap.clone(),
        ytyp: args.ytyp.clone(),
        ybn: args.ybn.clone(),
        ydr: args.ydr.clone(),
    };
    let sources = plot_inputs::resolve(&args.inputs, &explicit, keys, exe)?;

    let placement = find_placement(&sources);
    let z_band = z_band(args)?;
    let scene = build_scene(args, &sources, &placement, z_band)?;

    if z_band.is_none() {
        for (a, b) in rooms_stacked(&scene.rooms) {
            let (upper, lower) = if scene.rooms[a].z_lo >= scene.rooms[b].z_lo { (a, b) } else { (b, a) };
            let (u, l) = (&scene.rooms[upper], &scene.rooms[lower]);
            eprintln!(
                "warning: rooms stack vertically (r{} '{}' {:.1}..{:.1} over r{} '{}' {:.1}..{:.1}); use --floor-z Z or --z-range LO,HI to draw one storey",
                u.index, u.name, u.z_lo, u.z_hi, l.index, l.name, l.z_lo, l.z_hi
            );
        }
    }

    let opts = PlanOptions {
        region: args.region.as_deref().map(parse_quad).transpose().context("--region")?,
        scale: args.scale,
        z_band,
        layers: args.layers.iter().map(|l| Layer::from(*l)).collect(),
        labels: args.labels,
        ..Default::default()
    };

    let ext = args.output.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let report = match ext.as_str() {
        "svg" => {
            let (svg, report) = plan_svg(&scene, &opts)?;
            std::fs::write(&args.output, svg).with_context(|| format!("writing {}", args.output.display()))?;
            report
        }
        "png" | "jpg" | "jpeg" | "webp" => {
            let (img, report) = plan_png(&scene, &opts)?;
            let format: ImageFormat = ext.parse()?;
            let bytes = encode_image(&img, format, args.quality)?;
            std::fs::write(&args.output, bytes).with_context(|| format!("writing {}", args.output.display()))?;
            report
        }
        other => bail!("unsupported output extension '.{other}'; use .png, .jpg, .webp or .svg"),
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
) -> Result<Scene> {
    let mut scene = Scene::default();
    let name_of = |hash: u32| sources.names.get(&hash).cloned().unwrap_or_else(|| format!("{hash:#010x}"));

    if let Some(mlo) = &placement.mlo {
        // Rooms and portals are stored axis-aligned in the interior's own
        // space; MLO instances are placed with a yaw-only rotation in
        // practice, so a transformed box stays a box on the page.
        for (i, room) in mlo.rooms.iter().enumerate() {
            let footprint = quad_footprint(room.bb_min, room.bb_max, |v| placement.to_world(v));
            let (mut z_lo, mut z_hi) = (f32::MAX, f32::MIN);
            for z in [room.bb_min.z, room.bb_max.z] {
                for (x, y) in [(room.bb_min.x, room.bb_min.y), (room.bb_max.x, room.bb_max.y)] {
                    let w = placement.to_world(Vec3::new(x, y, z));
                    z_lo = z_lo.min(w.z);
                    z_hi = z_hi.max(w.z);
                }
            }
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

    for (_, ybn) in &sources.ybns {
        for tri in ybn.triangles() {
            scene.collision.push(Tri { v: tri.vertices.map(|v| placement.to_world(v)) });
        }
    }

    // A drawable given as a file is the interior's own shell, stored in the
    // same local space as the rooms; props are not placed per entity yet.
    for (_, drawables) in &sources.drawables {
        for drawable in drawables {
            let Some(lod) = drawable.best_lod() else { continue };
            for model in &lod.models {
                for geometry in &model.geometries {
                    let (Some(vertices), Some(indices)) = (&geometry.vertex_buffer, &geometry.index_buffer) else {
                        continue;
                    };
                    let Ok(unified) = vertices.to_unified_vertices() else { continue };
                    for face in indices.indices.chunks_exact(3) {
                        let corner = |i: u32| unified.get(i as usize).map(|v| placement.to_world(v.position));
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

    for marker in &args.marker {
        let (x, y, label) = parse_marker(marker).context("--marker expects x,y,label")?;
        scene.markers.push(Marker { x, y, label });
    }

    if !sources.ynvs.is_empty()
        && placement.entity.is_none()
        && !(scene.rooms.is_empty() && scene.collision.is_empty())
    {
        eprintln!("warning: navmesh polygons are in world coordinates but the rest of the plan is in the interior's own; pass the .ymap that places it");
    }

    let band = match z_band {
        Some((lo, hi)) => format!("z {lo:.1}..{hi:.1} m"),
        None => "all heights".to_string(),
    };
    scene.title = match &args.title {
        Some(title) => title.clone(),
        None => match &args.region {
            Some(region) => format!("{} — {region} — {band}", sources.label),
            None => format!("{} — {band}", sources.label),
        },
    };
    scene.caption.push(match &placement.source {
        Some(ymap) => format!("placement: {ymap}"),
        None => "MLO-local coordinates (no ymap given)".to_string(),
    });
    scene.caption.push(band);
    if !sources.skipped.is_empty() {
        scene.caption.push(format!("escrow-encrypted, not drawn: {}", sources.skipped.join(", ")));
    }
    for note in &sources.notes {
        scene.caption.push(note.clone());
    }
    Ok(scene)
}
