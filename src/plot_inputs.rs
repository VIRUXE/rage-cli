//! Turning `rage plot`'s free-form inputs — loose files, a FiveM resource
//! folder, or an archetype name — into the parsed pieces a plan is drawn
//! from. Nothing here parses a format itself: every byte goes to rage-formats.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rage_formats::{
    is_fxap, parse_ybn, parse_ydd, parse_ydr, parse_ymap_entities, parse_ymap_mlo_instances, parse_ynv, parse_ytyp,
    rage_joaat, Drawable, MloInstance, Vec3, Ybn, YmapEntity, Ynv, Ytyp,
};

use crate::index::GameIndex;
use crate::rpf::GtaKeys;
use crate::utils::walkdir;

/// A folder is drawn whole only while it holds fewer drawables than this;
/// at this many it is a whole resource's art, and the caller is asked to name
/// the ones worth drawing instead of waiting for all of them to be parsed.
const MAX_FOLDER_DRAWABLES: usize = 200;

/// Whether a folder's drawable count is past the limit. The rule is "fewer
/// than `MAX_FOLDER_DRAWABLES` are drawn", so the limit itself is already too
/// many.
fn too_many_drawables(drawables: usize) -> bool {
    drawables >= MAX_FOLDER_DRAWABLES
}

/// Files named with a `--ymap`/`--ytyp`/`--ybn`/`--ydr` flag. The extension
/// still decides what a file is; what the flag adds is the caller's word
/// that it belongs to the interior, so `--ybn`/`--ydr` files are placed by
/// the .ymap whatever they are called.
#[derive(Default)]
pub struct Explicit {
    pub ymap: Vec<PathBuf>,
    pub ytyp: Vec<PathBuf>,
    pub ybn: Vec<PathBuf>,
    pub ydr: Vec<PathBuf>,
}

/// Everything the inputs turned out to hold, in the order it was read.
#[derive(Default)]
pub struct PlotSources {
    /// What the plan is of: the first input's file or folder name.
    pub label: String,
    pub ynvs: Vec<(String, Ynv)>,
    pub ybns: Vec<Mesh<Ybn>>,
    /// A `.ydr` contributes one drawable, a `.ydd` its whole dictionary.
    pub drawables: Vec<Mesh<Vec<Drawable>>>,
    pub ytyps: Vec<(String, Ytyp)>,
    pub ymaps: Vec<(String, Vec<YmapEntity>, Vec<MloInstance>)>,
    /// joaat(lowercase stem) -> stem, for every file seen: how archetype
    /// hashes in an MLO get readable names.
    pub names: HashMap<u32, String>,
    /// Files that could not contribute geometry, with why; already reported
    /// on stderr, one line per folder and reason rather than per file.
    pub skipped: Vec<Skipped>,
    /// Anything the reader had to decide for the caller — an interior placed
    /// in several .ymaps, say. `plot` puts these on the page's caption.
    pub notes: Vec<String>,
}

/// A mesh file that was read: collision or a drawable dictionary.
pub struct Mesh<T> {
    /// What to call it on stderr — a file name, or the inner path of an entry
    /// inside a game archive.
    pub name: String,
    /// True when the caller named this very file with `--ybn`/`--ydr`, which
    /// says it belongs to the interior whatever it is called. Carried per
    /// entry rather than looked up by name, so a same-named file picked up by
    /// a folder walk is not mistaken for it.
    pub explicit: bool,
    pub data: T,
}

/// Why a file contributed nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Wrapped in FiveM's escrow container: the geometry is not readable.
    Escrow,
    /// Could not be read or parsed.
    Unreadable,
}

/// One file that was left out of the plan. Only the counts reach the page,
/// so what is kept is what the grouping needs.
pub struct Skipped {
    /// The folder it was found in, when it came from a folder walk; the
    /// report groups by this so a resource with 140 escrowed props costs one
    /// line, not 140.
    pub folder: Option<String>,
    pub reason: SkipReason,
}

impl Skipped {
    /// A file named directly (not found by walking), which was already
    /// reported on its own line.
    pub fn unreadable() -> Self {
        Self { folder: None, reason: SkipReason::Unreadable }
    }
}

impl PlotSources {
    /// Files the caller named are reported one by one; files found by
    /// walking are counted now and reported per folder afterwards.
    fn skip(&mut self, path: &Path, reason: SkipReason, strict: bool, why: &str) {
        let name = name_of(path);
        if strict {
            eprintln!("skipping {name}: {why}");
        }
        let folder = if strict {
            None
        } else {
            Some(path.parent().map(|p| p.display().to_string()).unwrap_or_default())
        };
        self.skipped.push(Skipped { folder, reason });
    }
}

/// What an input string turned out to be.
enum InputKind {
    Folder(PathBuf),
    File(PathBuf),
    /// Meant as a path, but nothing is there.
    Missing(String),
    Archetype(String),
}

fn classify(input: &str) -> InputKind {
    let path = PathBuf::from(input);
    if path.is_dir() {
        InputKind::Folder(path)
    } else if path.is_file() {
        InputKind::File(path)
    } else if looks_like_a_path(input) {
        InputKind::Missing(input.to_string())
    } else {
        InputKind::Archetype(input.to_string())
    }
}

/// True when an input reads as a path rather than an archetype name. A
/// mistyped file name would otherwise be taken for a vanilla interior and
/// cost a full index build before failing with the wrong complaint.
fn looks_like_a_path(input: &str) -> bool {
    if input.contains(['/', '\\']) {
        return true;
    }
    let ext = input.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    matches!(ext.as_deref(), Some("ynv" | "ybn" | "ymap" | "ytyp" | "ydr" | "ydd"))
}

/// The path as the filesystem knows it, so the same file reached two ways is
/// recognised as one. A path that cannot be resolved stands for itself.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Reads every input and the explicitly-typed files into one `PlotSources`.
/// A file named on the command line is an error when it cannot be read; a
/// file found by walking a folder is only reported and skipped.
pub fn resolve(inputs: &[String], explicit: &Explicit, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<PlotSources> {
    let mut src = PlotSources::default();
    src.label = inputs
        .first()
        .map(|first| match classify(first) {
            InputKind::Folder(p) | InputKind::File(p) => {
                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| first.clone())
            }
            InputKind::Missing(name) | InputKind::Archetype(name) => name,
        })
        .unwrap_or_default();

    // The flagged files are read first so that each claims its path: the
    // same file reached again through a folder walk or a positional input is
    // then left alone, rather than drawn twice — once placed and once in
    // world space, which shows up as ghost geometry and doubled counts.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for path in &explicit.ybn { add_file(path, true, true, &mut seen, &mut src)?; }
    for path in &explicit.ydr { add_file(path, true, true, &mut seen, &mut src)?; }
    for path in &explicit.ymap { add_file(path, true, false, &mut seen, &mut src)?; }
    for path in &explicit.ytyp { add_file(path, true, false, &mut seen, &mut src)?; }

    for input in inputs {
        match classify(input) {
            InputKind::Folder(dir) => add_folder(&dir, &mut seen, &mut src)?,
            InputKind::File(path) => add_file(&path, true, false, &mut seen, &mut src)?,
            InputKind::Missing(name) => bail!("no such file or folder: {name}"),
            InputKind::Archetype(name) => add_archetype(&name, keys, exe, &mut src)?,
        }
    }
    Ok(src)
}

fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

fn remember_name(path: &Path, src: &mut PlotSources) {
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let stem = stem.to_lowercase();
        src.names.insert(rage_joaat(&stem), stem);
    }
}

/// Reads one file and files its contents under the right kind. `strict` is
/// on for files the caller named: their problems are errors, not notes.
/// `explicit` is on for the mesh files `--ybn`/`--ydr` named, which the
/// caller has declared part of the interior. `seen` holds the paths already
/// read, so no file joins the plan twice.
fn add_file(path: &Path, strict: bool, explicit: bool, seen: &mut HashSet<PathBuf>, src: &mut PlotSources) -> Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if !matches!(ext.as_str(), "ynv" | "ybn" | "ymap" | "ytyp" | "ydr" | "ydd") {
        if strict {
            eprintln!("{}: not a file `plot` can draw (.ynv .ybn .ymap .ytyp .ydr .ydd); ignored", name_of(path));
        }
        return Ok(());
    }
    if !seen.insert(canonical(path)) {
        return Ok(());
    }
    remember_name(path, src);

    let name = name_of(path);
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if !strict => {
            src.skip(path, SkipReason::Unreadable, strict, &format!("{e}"));
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    // FiveM escrow wraps the resource in an encrypted container; the geometry
    // inside it is not there to be read, by this tool or any other.
    if is_fxap(&data) {
        src.skip(path, SkipReason::Escrow, strict, "FiveM escrow-encrypted (FXAP); its geometry cannot be drawn");
        return Ok(());
    }

    let parsed = (|| -> Result<()> {
        match ext.as_str() {
            "ynv" => src.ynvs.push((name.clone(), parse_ynv(&data)?)),
            "ybn" => src.ybns.push(Mesh { name: name.clone(), explicit, data: parse_ybn(&data)? }),
            "ytyp" => src.ytyps.push((name.clone(), parse_ytyp(&data)?)),
            "ymap" => {
                let entities = parse_ymap_entities(&data)?;
                let instances = parse_ymap_mlo_instances(&data)?;
                src.ymaps.push((name.clone(), entities, instances));
            }
            "ydr" => {
                src.drawables.push(Mesh { name: name.clone(), explicit, data: vec![parse_ydr(&data)?] })
            }
            "ydd" => {
                let entries = parse_ydd(&data)?;
                let data = entries.into_iter().map(|e| e.drawable).collect();
                src.drawables.push(Mesh { name: name.clone(), explicit, data });
            }
            _ => unreachable!("extension already filtered"),
        }
        Ok(())
    })();

    if let Err(e) = parsed {
        if strict {
            return Err(e).with_context(|| format!("parsing {}", path.display()));
        }
        src.skip(path, SkipReason::Unreadable, strict, &format!("{e:#}"));
    }
    Ok(())
}

/// Walks a FiveM resource folder: every file's stem names an archetype, and
/// every map, type and geometry file it holds joins the plan.
fn add_folder(dir: &Path, seen: &mut HashSet<PathBuf>, src: &mut PlotSources) -> Result<()> {
    let files = walkdir(dir)?;
    for path in &files {
        remember_name(path, src);
    }
    let is_drawable = |p: &PathBuf| {
        matches!(p.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref(), Some("ydr") | Some("ydd"))
    };
    let drawables = files.iter().filter(|p| is_drawable(p)).count();
    let skip_drawables = too_many_drawables(drawables);
    if skip_drawables {
        eprintln!(
            "{drawables} drawables in folder ({MAX_FOLDER_DRAWABLES} or more are left out);              pass the ones to draw with --ydr"
        );
    }
    let before = src.skipped.len();
    for path in &files {
        if skip_drawables && is_drawable(path) {
            continue;
        }
        add_file(path, false, false, seen, src)?;
    }
    report_skips(&src.skipped[before..]);
    Ok(())
}

/// One stderr line per folder and reason: a resource whose whole prop
/// library is escrowed should cost one line, not one per file.
fn report_skips(skipped: &[Skipped]) {
    let mut groups: Vec<(String, SkipReason, usize)> = Vec::new();
    for s in skipped {
        let folder = s.folder.clone().unwrap_or_default();
        match groups.iter_mut().find(|(f, r, _)| *f == folder && *r == s.reason) {
            Some((_, _, n)) => *n += 1,
            None => groups.push((folder, s.reason, 1)),
        }
    }
    for (folder, reason, n) in groups {
        let files = if n == 1 { "file" } else { "files" };
        match reason {
            SkipReason::Escrow => {
                eprintln!("skipping {n} escrow-encrypted {files} (FXAP) under {folder}; their geometry cannot be drawn")
            }
            SkipReason::Unreadable => eprintln!("skipping {n} unreadable {files} under {folder}"),
        }
    }
}

/// Two interior placements this close together are the same placement seen
/// twice: a DLC pack shipping its own copy of a base-game map re-states the
/// position, but rarely to the last float.
const SAME_PLACEMENT_METRES: f32 = 0.5;

/// Collapses placements that the game itself would only load once. A `.ymap`
/// overridden by a later archive is read twice — once from the base game,
/// once from the pack that replaces it — and both copies place the interior
/// in the same spot. The last in rank order wins, which is the DLC copy, so
/// what is drawn (and counted in the note) is what the game would load.
///
/// Instances further apart than `SAME_PLACEMENT_METRES` are separate
/// placements of a repeated interior — a garage, an apartment — and all of
/// them are kept. Returns how many survived.
fn dedupe_placements(ymaps: &mut [(String, Vec<YmapEntity>, Vec<MloInstance>)]) -> usize {
    let all: Vec<Vec3> =
        ymaps.iter().flat_map(|(_, _, instances)| instances.iter().map(|i| i.entity.position)).collect();
    let same = |a: Vec3, b: Vec3| {
        (a.x - b.x).abs() <= SAME_PLACEMENT_METRES
            && (a.y - b.y).abs() <= SAME_PLACEMENT_METRES
            && (a.z - b.z).abs() <= SAME_PLACEMENT_METRES
    };
    let keep: Vec<bool> = (0..all.len()).map(|i| !all[i + 1..].iter().any(|&later| same(all[i], later))).collect();

    let mut next = 0;
    for (_, _, instances) in ymaps.iter_mut() {
        instances.retain(|_| {
            next += 1;
            keep[next - 1]
        });
    }
    keep.iter().filter(|kept| **kept).count()
}

/// A vanilla interior named on the command line, e.g. `v_bahama` or
/// `0x8ae4f2c2`: the game index says which `.ytyp` declares it, which
/// `.ymap`s place it and whether a `.ybn` shares its name, and each of those
/// is read straight out of the archives it lives in.
fn add_archetype(name_or_hash: &str, keys: Option<&GtaKeys>, exe: Option<&Path>, src: &mut PlotSources) -> Result<()> {
    let hash = match name_or_hash.strip_prefix("0x").or_else(|| name_or_hash.strip_prefix("0X")) {
        Some(digits) => u32::from_str_radix(digits, 16)
            .with_context(|| format!("'{name_or_hash}' is not a 32-bit hex hash"))?,
        None => rage_joaat(&name_or_hash.to_lowercase()),
    };

    let Some(index) = GameIndex::load_or_build(exe, keys) else {
        bail!(
            "'{name_or_hash}' is not a file or folder; resolving a vanilla interior by name needs \
             --exe or GTAV_PATH so the game index can be used"
        );
    };

    let Some(loc) = index.mlo_ytyp.get(&hash) else {
        bail!("no interior archetype named '{name_or_hash}' in the game index");
    };
    let data = index.load_bytes(loc, keys).with_context(|| format!("reading {}", loc.inner_path))?;
    let mut ytyp = parse_ytyp(&data).with_context(|| format!("parsing {}", loc.inner_path))?;
    // One .ytyp declares many interiors; only the one that was asked for
    // should be drawn, but every archetype stays for the prop boxes.
    ytyp.mlos.retain(|mlo| mlo.name_hash == hash);
    src.ytyps.push((loc.inner_path.clone(), ytyp));

    let placements = index.mlo_instances.get(&hash).map(Vec::as_slice).unwrap_or(&[]);
    // Where this archetype's maps start, so only they are deduped below:
    // an earlier input may have put its own .ymaps in the same list.
    let first_ymap = src.ymaps.len();
    for loc in placements {
        let data = match index.load_bytes(loc, keys) {
            Ok(data) => data,
            Err(err) => {
                eprintln!("skipping {}: {err:#}", loc.inner_path);
                src.skipped.push(Skipped::unreadable());
                continue;
            }
        };
        let parsed = parse_ymap_entities(&data).and_then(|entities| {
            let instances = parse_ymap_mlo_instances(&data)?;
            Ok((entities, instances))
        });
        match parsed {
            Ok((mut entities, mut instances)) => {
                entities.retain(|e| e.archetype_hash == hash);
                instances.retain(|i| i.entity.archetype_hash == hash);
                src.ymaps.push((loc.inner_path.clone(), entities, instances));
            }
            Err(err) => {
                eprintln!("skipping {}: {err:#}", loc.inner_path);
                src.skipped.push(Skipped::unreadable());
            }
        }
    }
    // Counted in placements, not files: one .ymap can place the same
    // interior several times, and the note is about which one is drawn.
    let placements_found = dedupe_placements(&mut src.ymaps[first_ymap..]);
    if placements_found > 1 {
        src.notes.push(format!("{placements_found} placements in the game, drawing the first"));
    }

    // The collision of an interior almost always shares its archetype name;
    // when nothing does, the plan simply has no collision layer.
    if let Some(loc) = index.ybn_by_name.get(&hash) {
        match index.load_bytes(loc, keys).and_then(|data| parse_ybn(&data).map_err(Into::into)) {
            Ok(ybn) => src.ybns.push(Mesh { name: loc.inner_path.clone(), explicit: false, data: ybn }),
            Err(err) => {
                eprintln!("skipping {}: {err:#}", loc.inner_path);
                src.skipped.push(Skipped::unreadable());
            }
        }
    }

    src.names.insert(hash, name_or_hash.to_lowercase());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{dedupe_placements, too_many_drawables, MAX_FOLDER_DRAWABLES};
    use rage_formats::{MloInstance, Vec3, YmapEntity};

    fn instance(x: f32, y: f32, z: f32) -> MloInstance {
        MloInstance {
            entity: YmapEntity {
                archetype_hash: 7,
                flags: 0,
                guid: 0,
                position: Vec3::new(x, y, z),
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale_xy: 1.0,
                scale_z: 1.0,
                parent_index: -1,
                lod_dist: 100.0,
                is_mlo_instance: true,
            },
            group_id: 0,
            floor_id: 0,
            default_entity_sets: Vec::new(),
            num_exit_portals: 0,
        }
    }

    fn ymap(name: &str, instances: Vec<MloInstance>) -> (String, Vec<YmapEntity>, Vec<MloInstance>) {
        (name.to_string(), Vec::new(), instances)
    }

    fn positions(ymaps: &[(String, Vec<YmapEntity>, Vec<MloInstance>)]) -> Vec<(String, Vec3)> {
        ymaps
            .iter()
            .flat_map(|(name, _, instances)| instances.iter().map(|i| (name.clone(), i.entity.position)))
            .collect()
    }

    /// A DLC pack ships its own copy of a base-game map, so the same interior
    /// is placed twice in the same spot. The later (DLC) copy is the one the
    /// game loads, so it is the one kept.
    #[test]
    fn placements_in_the_same_spot_collapse_to_the_last_one() {
        let mut ymaps = vec![
            ymap("base.ymap", vec![instance(100.0, 200.0, 30.0)]),
            ymap("dlc.ymap", vec![instance(100.2, 200.1, 30.0)]),
        ];
        assert_eq!(dedupe_placements(&mut ymaps), 1);
        assert_eq!(positions(&ymaps), vec![("dlc.ymap".to_string(), Vec3::new(100.2, 200.1, 30.0))]);
    }

    /// A repeated interior (a garage, an apartment) is placed many times over
    /// the map; those are separate placements and all of them are kept.
    #[test]
    fn placements_further_apart_than_half_a_metre_are_all_kept() {
        let mut ymaps = vec![
            ymap("a.ymap", vec![instance(0.0, 0.0, 0.0), instance(0.0, 0.6, 0.0)]),
            ymap("b.ymap", vec![instance(500.0, 0.0, 0.0)]),
        ];
        assert_eq!(dedupe_placements(&mut ymaps), 3);
        assert_eq!(positions(&ymaps).len(), 3);
    }

    /// Two maps that merely share a file name can place the interior in two
    /// different spots; the index now keeps both, and both are drawn.
    #[test]
    fn same_named_maps_placing_the_interior_elsewhere_both_survive() {
        let mut ymaps = vec![
            ymap("x64a.rpf/int.ymap", vec![instance(0.0, 0.0, 0.0)]),
            ymap("dlc.rpf/int.ymap", vec![instance(-1200.0, 450.0, 60.0)]),
        ];
        assert_eq!(dedupe_placements(&mut ymaps), 2);
        assert_eq!(positions(&ymaps).len(), 2);
    }

    /// Nothing placed is still nothing placed.
    #[test]
    fn no_placements_dedupe_to_none() {
        let mut ymaps = vec![ymap("empty.ymap", Vec::new())];
        assert_eq!(dedupe_placements(&mut ymaps), 0);
    }

    /// The documented rule is "fewer than 200 drawables are drawn", so 200
    /// itself is already too many.
    #[test]
    fn the_drawable_limit_is_exclusive() {
        assert!(!too_many_drawables(MAX_FOLDER_DRAWABLES - 1));
        assert!(too_many_drawables(MAX_FOLDER_DRAWABLES));
        assert!(too_many_drawables(MAX_FOLDER_DRAWABLES + 1));
    }
}
