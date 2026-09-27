//! `rage ytyp from-drawables`: a type file declaring an archetype for each
//! model, the way CodeWalker's "New Archetype from YDR" fills one in — the
//! name, the drawable's bounding box and sphere, the draw distances, and the
//! texture and physics dictionaries it finds next to the model. A `.ydr`
//! gives one archetype, a `.ydd` one per drawable it holds, a `.yft` one
//! for the fragment.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rage_formats::{
    build_meta, dump_meta, from_xml, parse_drawables, parse_yft, parse_ytyp, prepare_rsc7, rage_joaat, to_xml, Drawable, DrawableBounds,
    DrawableKind, MetaValue, NameTable, Schema, Vec3,
};

#[derive(clap::Args)]
pub struct YtypArgs {
    #[command(subcommand)]
    pub command: YtypCommand,
}

#[derive(clap::Subcommand)]
pub enum YtypCommand {
    /// Declare an archetype for every .ydr, .ydd entry and .yft given
    FromDrawables(FromDrawablesArgs),
}

#[derive(clap::Args)]
pub struct FromDrawablesArgs {
    /// .ydr/.ydd/.yft files, or folders searched for them
    #[arg(required = true, value_name = "PATH")]
    pub inputs: Vec<PathBuf>,

    /// The .ytyp to write
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,

    /// Texture dictionary for every archetype; default: a .ytd named like
    /// the model beside it, else the model's own name when it embeds its
    /// textures (as CodeWalker does), else none
    #[arg(long, value_name = "NAME")]
    pub txd: Option<String>,

    /// lodDist of every archetype
    #[arg(long, default_value = "60", value_name = "N")]
    pub lod_dist: f32,

    /// hdTextureDist of every archetype (capped at the lodDist)
    #[arg(long, default_value = "60", value_name = "N")]
    pub hd_dist: f32,

    /// Archetype flags
    #[arg(long, default_value = "32", value_name = "N")]
    pub flags: u32,

    /// Add to this .ytyp instead of starting empty: its archetypes are
    /// kept, those with a name given again are replaced
    #[arg(long, value_name = "FILE")]
    pub merge: Option<PathBuf>,
}

pub fn run(args: &YtypArgs) -> Result<()> {
    match &args.command {
        YtypCommand::FromDrawables(a) => run_from_drawables(a),
    }
}

/// One `CBaseArchetypeDef` to write.
#[derive(Debug, Clone)]
struct ArchDef {
    name: String,
    asset_type: &'static str,
    drawable_dictionary: String,
    texture_dictionary: String,
    physics_dictionary: String,
    bounds: DrawableBounds,
}

fn run_from_drawables(args: &FromDrawablesArgs) -> Result<()> {
    let files = collect_inputs(&args.inputs)?;
    if files.is_empty() {
        bail!("no .ydr, .ydd or .yft among the inputs");
    }
    let names = crate::names::load(&[], files.first().map(PathBuf::as_path))?;

    let mut defs: Vec<ArchDef> = Vec::new();
    let mut seen = HashSet::new();
    for file in &files {
        if std::fs::read(file).is_ok_and(|d| rage_formats::is_fxap(&d)) {
            eprintln!("warning: skipping {}: FiveM escrow-encrypted, only the server that bought it can decrypt it", file.display());
            continue;
        }
        let made = archetypes_of(file, args.txd.as_deref(), &names).with_context(|| format!("'{}'", file.display()))?;
        for def in made {
            if seen.insert(rage_joaat(&def.name.to_lowercase())) {
                defs.push(def);
            } else {
                eprintln!("warning: {} is declared twice; keeping the first ({})", def.name, file.display());
            }
        }
    }

    if defs.is_empty() {
        bail!("no archetypes to write");
    }

    let mut root = match &args.merge {
        Some(path) => {
            let data = std::fs::read(path).with_context(|| format!("failed to read '{}'", path.display()))?;
            let dump = dump_meta(&data).with_context(|| format!("'{}' is not a Meta .ytyp", path.display()))?;
            // Through the XML text, so the kept archetypes and the new ones
            // reach the writer in the same form.
            from_xml(&to_xml(&dump.root, &names))?
        }
        None => from_xml(&empty_types(&stem_of(&args.output)))?,
    };
    let (added, replaced, kept) = splice(&mut root, &defs, args)?;

    let written = build_meta(&root, Schema::builtin())?;
    for warning in &written.warnings {
        eprintln!("warning: {warning}");
    }
    let check = parse_ytyp(&written.bytes).context("the written file does not read back")?;
    if let Some(parent) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&args.output, &written.bytes).with_context(|| format!("writing {}", args.output.display()))?;

    for d in &defs {
        let b = &d.bounds;
        let txd = if d.texture_dictionary.is_empty() { "-" } else { &d.texture_dictionary };
        eprintln!(
            "  {:<32} {:<14} radius {:>7.2}  size {:.2} x {:.2} x {:.2}  txd {txd}",
            d.name,
            short_type(d.asset_type),
            b.sphere_radius,
            b.box_max.x - b.box_min.x,
            b.box_max.y - b.box_min.y,
            b.box_max.z - b.box_min.z,
        );
    }
    let mut summary = format!("Wrote {} ({} archetypes", args.output.display(), check.archetypes.len());
    if args.merge.is_some() {
        write!(summary, ": {added} added, {replaced} replaced, {kept} kept").unwrap();
    }
    eprintln!("{summary})");
    Ok(())
}

/// The model files among `inputs`, folders walked, in a stable order. A
/// vehicle's `_hi.yft` is left out when the plain `.yft` is there: it is
/// the same archetype's high-detail model, not an archetype of its own.
fn collect_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let mut found: Vec<PathBuf> = crate::utils::walkdir(input)?.into_iter().filter(|p| kind_of(p).is_some()).collect();
            found.sort();
            files.extend(found);
        } else if input.is_file() {
            if kind_of(input).is_none() {
                bail!("'{}' is not a .ydr, .ydd or .yft", input.display());
            }
            files.push(input.clone());
        } else {
            bail!("'{}': no such file or folder", input.display());
        }
    }
    let present: HashSet<PathBuf> = files.iter().map(|p| lower(p)).collect();
    files.retain(|p| {
        let stem = stem_of(p);
        let hd_of = stem.strip_suffix("_hi").filter(|_| kind_of(p) == Some(DrawableKind::Yft));
        !hd_of.is_some_and(|base| present.contains(&lower(&p.with_file_name(format!("{base}.yft")))))
    });
    let mut unique = HashSet::new();
    files.retain(|p| unique.insert(lower(p)));
    Ok(files)
}

fn lower(p: &Path) -> PathBuf {
    PathBuf::from(p.to_string_lossy().to_lowercase())
}

fn kind_of(p: &Path) -> Option<DrawableKind> {
    DrawableKind::from_extension(p.extension()?.to_str()?)
}

fn stem_of(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// A dictionary named `stem` with `ext` in `dir`, whatever its case.
fn sibling(dir: &Path, stem: &str, ext: &str) -> bool {
    let want = format!("{stem}.{ext}").to_lowercase();
    std::fs::read_dir(dir).is_ok_and(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().to_lowercase() == want))
}

fn archetypes_of(file: &Path, txd: Option<&str>, names: &NameTable) -> Result<Vec<ArchDef>> {
    let kind = kind_of(file).context("not a model file")?;
    let data = std::fs::read(file)?;
    let stem = stem_of(file);
    let dir = file.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let own_ytd = sibling(dir, &stem, "ytd");
    let texture_dictionary = |embeds: bool| match txd {
        Some(t) => t.to_owned(),
        None if own_ytd || embeds => stem.clone(),
        None => String::new(),
    };

    if kind == DrawableKind::Yft {
        let fragment = parse_yft(&data)?;
        let drawable = fragment.drawable.context("the fragment has no drawable")?;
        return Ok(vec![ArchDef {
            name: stem.clone(),
            asset_type: "ASSET_TYPE_FRAGMENT",
            drawable_dictionary: String::new(),
            texture_dictionary: texture_dictionary(embeds_textures(&drawable)),
            physics_dictionary: String::new(),
            bounds: bounds_of(&drawable),
        }]);
    }

    let entries = parse_drawables(&data, kind)?;
    // A .ydr holding a dictionary (or the reverse) is taken for what it is.
    let single = kind == DrawableKind::Ydr && entries.len() == 1;
    if single {
        let drawable = &entries[0].drawable;
        return Ok(vec![ArchDef {
            name: stem.clone(),
            asset_type: "ASSET_TYPE_DRAWABLE",
            drawable_dictionary: String::new(),
            texture_dictionary: texture_dictionary(embeds_textures(drawable)),
            physics_dictionary: if has_embedded_bound(&data) { stem.clone() } else { String::new() },
            bounds: bounds_of(drawable),
        }]);
    }

    // A physics dictionary (.ybd) keyed like the drawable dictionary.
    let physics = if sibling(dir, &stem, "ybd") { stem.clone() } else { String::new() };
    Ok(entries
        .iter()
        .map(|e| ArchDef {
            name: entry_name(&e.drawable.name, e.hash, names),
            asset_type: "ASSET_TYPE_DRAWABLEDICTIONARY",
            drawable_dictionary: stem.clone(),
            texture_dictionary: texture_dictionary(embeds_textures(&e.drawable)),
            physics_dictionary: physics.clone(),
            bounds: bounds_of(&e.drawable),
        })
        .collect())
}

/// A dictionary entry's name: what the drawable calls itself (less the
/// `.#dd` suffix) when that is what the entry is keyed by, else the key's
/// name from the name lists, else its `hash_` placeholder — the key is what
/// the game looks the archetype's model up by.
fn entry_name(own: &str, hash: u32, names: &NameTable) -> String {
    let base = own.split(".#").next().unwrap_or(own);
    if !base.is_empty() && rage_joaat(&base.to_lowercase()) == hash {
        return base.to_owned();
    }
    names.resolve(hash).into_owned()
}

fn embeds_textures(d: &Drawable) -> bool {
    d.shader_group.as_ref().is_some_and(|g| !g.textures.is_empty())
}

/// The stored bounds, or bounds from the vertices when the stored ones do
/// not describe the model (see `Drawable::bounds_or_computed`).
fn bounds_of(d: &Drawable) -> DrawableBounds {
    match d.best_lod() {
        Some(lod) => d.bounds_or_computed(lod).0,
        None => d.bounds.clone(),
    }
}

/// Whether a single-drawable resource carries its own collision: the
/// `gtaDrawable` bound pointer at 0xC8.
fn has_embedded_bound(data: &[u8]) -> bool {
    prepare_rsc7(data).is_ok_and(|(system, _)| {
        system.get(0xC8..0xD0).is_some_and(|b| u64::from_le_bytes(b.try_into().unwrap()) != 0)
    })
}

fn short_type(asset_type: &str) -> &str {
    match asset_type {
        "ASSET_TYPE_DRAWABLE" => "drawable",
        "ASSET_TYPE_DRAWABLEDICTIONARY" => "dictionary",
        "ASSET_TYPE_FRAGMENT" => "fragment",
        other => other,
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn empty_types(name: &str) -> String {
    format!(
        "<CMapTypes>\n  <extensions/>\n  <archetypes/>\n  <name>{}</name>\n  <dependencies/>\n  <compositeEntityTypes itemType=\"CCompositeEntityType\"/>\n</CMapTypes>\n",
        esc(name)
    )
}

fn xyz(tag: &str, v: Vec3) -> String {
    format!("<{tag} x=\"{}\" y=\"{}\" z=\"{}\"/>", v.x, v.y, v.z)
}

fn text_el(tag: &str, s: &str) -> String {
    if s.is_empty() { format!("<{tag}/>") } else { format!("<{tag}>{}</{tag}>", esc(s)) }
}

/// A `CBaseArchetypeDef` in CodeWalker's XML layout.
fn archetype_xml(d: &ArchDef, args: &FromDrawablesArgs) -> String {
    let b = &d.bounds;
    [
        "<Item type=\"CBaseArchetypeDef\">".to_owned(),
        format!("<lodDist value=\"{}\"/>", args.lod_dist),
        format!("<flags value=\"{}\"/>", args.flags),
        "<specialAttribute value=\"0\"/>".to_owned(),
        xyz("bbMin", b.box_min),
        xyz("bbMax", b.box_max),
        xyz("bsCentre", b.center),
        format!("<bsRadius value=\"{}\"/>", b.sphere_radius),
        format!("<hdTextureDist value=\"{}\"/>", args.hd_dist.min(args.lod_dist)),
        text_el("name", &d.name),
        text_el("textureDictionary", &d.texture_dictionary),
        "<clipDictionary/>".to_owned(),
        text_el("drawableDictionary", &d.drawable_dictionary),
        text_el("physicsDictionary", &d.physics_dictionary),
        text_el("assetType", d.asset_type),
        text_el("assetName", &d.name),
        "<extensions/>".to_owned(),
        "</Item>".to_owned(),
    ]
    .concat()
}

/// Puts `defs` into the `archetypes` array of `root`, replacing any of the
/// same name. Returns (added, replaced, kept).
fn splice(root: &mut MetaValue, defs: &[ArchDef], args: &FromDrawablesArgs) -> Result<(usize, usize, usize)> {
    let MetaValue::Struct(types) = root else { bail!("the type file's root is not a structure") };
    if types.type_hash != rage_joaat("CMapTypes") {
        bail!("not a type file (the root is not CMapTypes)");
    }
    let key = rage_joaat("archetypes");
    let slot = match types.fields.iter().position(|(h, _)| *h == key) {
        Some(i) => i,
        None => {
            types.fields.insert(1.min(types.fields.len()), (key, MetaValue::Null));
            1.min(types.fields.len() - 1)
        }
    };
    let old = std::mem::take(&mut types.fields[slot].1);
    let mut items = match old {
        MetaValue::Array(a) => a.items,
        _ => Vec::new(),
    };

    let (mut added, mut replaced) = (0, 0);
    for d in defs {
        let item = from_xml(&archetype_xml(d, args))?;
        let hash = rage_joaat(&d.name.to_lowercase());
        let existing = items.iter().position(|i| {
            i.as_struct().and_then(|s| s.field("name")).and_then(crate::extents::name_hash) == Some(hash)
        });
        match existing {
            Some(i) => {
                items[i] = item;
                replaced += 1;
            }
            None => {
                items.push(item);
                added += 1;
            }
        }
    }
    let kept = items.len() - added - replaced;
    types.fields[slot].1 = MetaValue::Array(rage_formats::value::MetaArray { item_type: None, typed_items: true, items });
    Ok((added, replaced, kept))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> FromDrawablesArgs {
        FromDrawablesArgs {
            inputs: vec![],
            output: PathBuf::from("x.ytyp"),
            txd: None,
            lod_dist: 100.0,
            hd_dist: 60.0,
            flags: 32,
            merge: None,
        }
    }

    fn def(name: &str) -> ArchDef {
        ArchDef {
            name: name.to_owned(),
            asset_type: "ASSET_TYPE_DRAWABLE",
            drawable_dictionary: String::new(),
            texture_dictionary: name.to_owned(),
            physics_dictionary: String::new(),
            bounds: DrawableBounds {
                center: Vec3::new(0.0, 0.0, 1.0),
                sphere_radius: 1.5,
                box_min: Vec3::new(-1.0, -0.5, 0.0),
                box_max: Vec3::new(1.0, 0.5, 2.0),
            },
        }
    }

    #[test]
    fn writes_a_type_file_that_reads_back() {
        let mut root = from_xml(&empty_types("props")).unwrap();
        splice(&mut root, &[def("my_prop"), def("my_other")], &args()).unwrap();
        let written = build_meta(&root, Schema::builtin()).unwrap();
        let ytyp = parse_ytyp(&written.bytes).unwrap();
        assert_eq!(ytyp.archetypes.len(), 2);
        let a = &ytyp.archetypes[0];
        assert_eq!(a.name_hash, rage_joaat("my_prop"));
        assert_eq!(a.asset_name_hash, rage_joaat("my_prop"));
        assert_eq!(a.texture_dict_hash, rage_joaat("my_prop"));
        assert_eq!(a.asset_type, rage_formats::Archetype::ASSET_TYPE_DRAWABLE);
        assert_eq!(a.lod_dist, 100.0);
        assert_eq!(a.bb_max, Vec3::new(1.0, 0.5, 2.0));
    }

    #[test]
    fn merging_replaces_by_name_and_keeps_the_rest() {
        let mut root = from_xml(&empty_types("props")).unwrap();
        splice(&mut root, &[def("a"), def("b")], &args()).unwrap();
        let mut again = def("b");
        again.bounds.box_max.z = 9.0;
        let (added, replaced, kept) = splice(&mut root, &[again, def("c")], &args()).unwrap();
        assert_eq!((added, replaced, kept), (1, 1, 1));
        let written = build_meta(&root, Schema::builtin()).unwrap();
        let ytyp = parse_ytyp(&written.bytes).unwrap();
        let b = ytyp.archetypes.iter().find(|a| a.name_hash == rage_joaat("b")).unwrap();
        assert_eq!(b.bb_max.z, 9.0);
        assert_eq!(ytyp.archetypes.len(), 3);
    }

    #[test]
    fn dictionary_entries_are_named_by_their_key() {
        let names = NameTable::default();
        assert_eq!(entry_name("minimap_7_6.#dd", rage_joaat("minimap_7_6"), &names), "minimap_7_6");
        assert_eq!(entry_name("minimap_0_6.#dd", rage_joaat("minimap_7_6"), &names), format!("hash_{:08X}", rage_joaat("minimap_7_6")));
    }
}
