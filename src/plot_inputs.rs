//! Turning `rage plot`'s free-form inputs — loose files, a FiveM resource
//! folder, or an archetype name — into the parsed pieces a plan is drawn
//! from. Nothing here parses a format itself: every byte goes to rage-formats.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rage_formats::{
    is_fxap, parse_ybn, parse_ydd, parse_ydr, parse_ymap_entities, parse_ymap_mlo_instances, parse_ynv, parse_ytyp,
    rage_joaat, Drawable, MloInstance, Ybn, YmapEntity, Ynv, Ytyp,
};

use crate::rpf::GtaKeys;
use crate::utils::walkdir;

/// A folder holding more drawables than this is a whole resource's art: the
/// caller is asked to name the ones worth drawing instead of waiting for all
/// of them to be parsed.
const MAX_FOLDER_DRAWABLES: usize = 200;

/// Files named on the command line with a `--ymap`/`--ytyp`/`--ybn`/`--ydr`
/// flag, which say what a file is instead of leaving it to its extension.
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
    pub ybns: Vec<(String, Ybn)>,
    /// A `.ydr` contributes one drawable, a `.ydd` its whole dictionary.
    pub drawables: Vec<(String, Vec<Drawable>)>,
    pub ytyps: Vec<(String, Ytyp)>,
    pub ymaps: Vec<(String, Vec<YmapEntity>, Vec<MloInstance>)>,
    /// joaat(lowercase stem) -> stem, for every file seen: how archetype
    /// hashes in an MLO get readable names.
    pub names: HashMap<u32, String>,
    /// Files that could not contribute geometry (escrow-encrypted or
    /// unparseable); each was already reported on stderr.
    pub skipped: Vec<String>,
}

/// What an input string turned out to be.
enum InputKind {
    Folder(PathBuf),
    File(PathBuf),
    Archetype(String),
}

fn classify(input: &str) -> InputKind {
    let path = PathBuf::from(input);
    if path.is_dir() {
        InputKind::Folder(path)
    } else if path.is_file() {
        InputKind::File(path)
    } else {
        InputKind::Archetype(input.to_string())
    }
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
            InputKind::Archetype(name) => name,
        })
        .unwrap_or_default();

    for input in inputs {
        match classify(input) {
            InputKind::Folder(dir) => add_folder(&dir, &mut src)?,
            InputKind::File(path) => add_file(&path, true, &mut src)?,
            InputKind::Archetype(name) => add_archetype(&name, keys, exe, &mut src)?,
        }
    }
    for path in &explicit.ymap { add_file(path, true, &mut src)?; }
    for path in &explicit.ytyp { add_file(path, true, &mut src)?; }
    for path in &explicit.ybn { add_file(path, true, &mut src)?; }
    for path in &explicit.ydr { add_file(path, true, &mut src)?; }
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
fn add_file(path: &Path, strict: bool, src: &mut PlotSources) -> Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if !matches!(ext.as_str(), "ynv" | "ybn" | "ymap" | "ytyp" | "ydr" | "ydd") {
        if strict {
            eprintln!("{}: not a file `plot` can draw (.ynv .ybn .ymap .ytyp .ydr .ydd); ignored", name_of(path));
        }
        return Ok(());
    }
    remember_name(path, src);

    let name = name_of(path);
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if !strict => {
            eprintln!("skipping {name}: {e}");
            src.skipped.push(name);
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    // FiveM escrow wraps the resource in an encrypted container; the geometry
    // inside it is not there to be read, by this tool or any other.
    if is_fxap(&data) {
        eprintln!("skipping {name}: FiveM escrow-encrypted (FXAP); its geometry cannot be drawn");
        src.skipped.push(name);
        return Ok(());
    }

    let parsed = (|| -> Result<()> {
        match ext.as_str() {
            "ynv" => src.ynvs.push((name.clone(), parse_ynv(&data)?)),
            "ybn" => src.ybns.push((name.clone(), parse_ybn(&data)?)),
            "ytyp" => src.ytyps.push((name.clone(), parse_ytyp(&data)?)),
            "ymap" => {
                let entities = parse_ymap_entities(&data)?;
                let instances = parse_ymap_mlo_instances(&data)?;
                src.ymaps.push((name.clone(), entities, instances));
            }
            "ydr" => src.drawables.push((name.clone(), vec![parse_ydr(&data)?])),
            "ydd" => {
                let entries = parse_ydd(&data)?;
                src.drawables.push((name.clone(), entries.into_iter().map(|e| e.drawable).collect()));
            }
            _ => unreachable!("extension already filtered"),
        }
        Ok(())
    })();

    if let Err(e) = parsed {
        if strict {
            return Err(e).with_context(|| format!("parsing {}", path.display()));
        }
        eprintln!("skipping {name}: {e:#}");
        src.skipped.push(name);
    }
    Ok(())
}

/// Walks a FiveM resource folder: every file's stem names an archetype, and
/// every map, type and geometry file it holds joins the plan.
fn add_folder(dir: &Path, src: &mut PlotSources) -> Result<()> {
    let files = walkdir(dir)?;
    for path in &files {
        remember_name(path, src);
    }
    let is_drawable = |p: &PathBuf| {
        matches!(p.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref(), Some("ydr") | Some("ydd"))
    };
    let drawables = files.iter().filter(|p| is_drawable(p)).count();
    let skip_drawables = drawables > MAX_FOLDER_DRAWABLES;
    if skip_drawables {
        eprintln!("{drawables} drawables in folder; pass the ones to draw with --ydr");
    }
    for path in &files {
        if skip_drawables && is_drawable(path) {
            continue;
        }
        add_file(path, false, src)?;
    }
    Ok(())
}

/// A vanilla interior named on the command line, e.g. `v_bahama` or
/// `0x8ae4f2c2`. Resolving one needs the game index, which this command does
/// not read yet.
fn add_archetype(name_or_hash: &str, _keys: Option<&GtaKeys>, _exe: Option<&Path>, _src: &mut PlotSources) -> Result<()> {
    bail!("'{name_or_hash}' is not a file or folder; resolving a vanilla interior by name needs the game index (Task 4 adds it)")
}
