//! `rage ymap lodlights`: the LOD lights of a resource's maps, generated
//! the way CodeWalker's project window does it (see `crate::lodlights`).
//! Every entity of the maps given is looked up in the resource's `.ytyp`
//! files (then the game's), its model found among the resource's `.ydr`,
//! `.ydd` and `.yft` files (then the game's), and each light of the model
//! becomes one row of `NAME_lodlights.ymap` and `NAME_distantlights.ymap`.

use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use rage_formats::{build_meta, dump_meta, from_xml, parse_ymap, MetaValue, NameTable, Schema, Vec3};

use crate::index::{GameIndex, Parts};
use crate::lodlights::{self, ArchLights, LodLight, ModelLights, Placement};
use crate::rpf::GtaKeys;

#[derive(clap::Args)]
pub struct LodlightsArgs {
    /// .ymap files, or folders searched for them
    #[arg(value_name = "PATH", required = true)]
    pub inputs: Vec<PathBuf>,

    /// Folder to write the two maps in; default: the first input's folder
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,

    /// The maps' name stem (NAME_lodlights, NAME_distantlights); default:
    /// the resource folder's name
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// .ytyp files (or folders of them) declaring the archetypes; default:
    /// every .ytyp in the resource folder, then the game's own through the
    /// index (--exe / GTAV_PATH)
    #[arg(long, value_name = "PATH")]
    pub ytyp: Vec<PathBuf>,

    /// Model files (.ydr, .ydd, .yft, or folders of them) the lights are
    /// read from; default: the resource folder, then the game's own
    #[arg(long, value_name = "PATH")]
    pub models: Vec<PathBuf>,
}

/// The folder a resource is rooted at: the nearest ancestor of `input`
/// (the folder itself, or a file's) holding a resource manifest, else
/// that folder.
fn resource_root(input: &Path) -> PathBuf {
    let dir = if input.is_dir() { input } else { input.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")) };
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    dir.ancestors().find(|d| d.join("fxmanifest.lua").is_file() || d.join("__resource.lua").is_file()).map(Path::to_path_buf).unwrap_or(dir)
}

fn has_ext(p: &Path, exts: &[&str]) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// The files with one of `exts` among `roots`: folders walked, files taken
/// as given.
fn files_of(roots: &[PathBuf], exts: &[&str], what: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for root in roots {
        if root.is_dir() {
            out.extend(crate::utils::walkdir(root)?.into_iter().filter(|p| has_ext(p, exts)));
        } else if root.is_file() {
            out.push(root.clone());
        } else {
            bail!("{what} {}: no such file or folder", root.display());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn stem_hash(p: &Path) -> u32 {
    rage_formats::rage_joaat(&p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default())
}

/// The archetypes of a type file, by name, with what the generator needs.
pub fn archetypes_of(bytes: &[u8]) -> Result<HashMap<u32, ArchLights>> {
    let dump = dump_meta(bytes)?;
    let root = dump.root.as_struct().context("not a type file")?;
    let mut out = HashMap::new();
    for item in root.field("archetypes").map_or(&[][..], MetaValue::items) {
        let Some(a) = item.as_struct() else { continue };
        let Some(name) = a.field("name").and_then(crate::extents::name_hash) else { continue };
        let vec3 = |n: &str| a.field(n).and_then(MetaValue::as_vec3).unwrap_or(Vec3::ZERO);
        out.insert(
            name,
            ArchLights {
                bb_min: vec3("bbMin"),
                bb_max: vec3("bbMax"),
                drawable_dict: a.field("drawableDictionary").and_then(crate::extents::name_hash).unwrap_or(0),
                extensions: a.field("extensions").map_or(0, |e| e.items().len() as u32),
            },
        );
    }
    Ok(out)
}

/// A model file's bytes, or nothing when it is found nowhere.
type ModelFile = Option<Rc<Vec<u8>>>;

/// Where archetypes and models come from: the resource's files, then the
/// game index, loaded the first time something is not found locally.
struct Sources<'a> {
    archetypes: HashMap<u32, ArchLights>,
    ydr: HashMap<u32, PathBuf>,
    ydd: HashMap<u32, PathBuf>,
    yft: HashMap<u32, PathBuf>,
    exe: Option<&'a Path>,
    keys: Option<&'a GtaKeys>,
    index: OnceCell<Option<GameIndex>>,
    /// Game type files read, by stem hash.
    game_ytyps: RefCell<HashMap<u32, HashMap<u32, ArchLights>>>,
    /// Models read, by the file's stem hash: a dictionary shared by many
    /// archetypes is read once.
    files: RefCell<HashMap<(u32, bool), ModelFile>>,
    archetypes_from_game: RefCell<HashSet<u32>>,
    models_from_game: RefCell<HashSet<u32>>,
    warned_no_index: RefCell<bool>,
}

impl<'a> Sources<'a> {
    fn new(ytyps: &[PathBuf], models: &[PathBuf], exe: Option<&'a Path>, keys: Option<&'a GtaKeys>) -> Self {
        let mut archetypes = HashMap::new();
        for path in ytyps {
            match std::fs::read(path).map_err(anyhow::Error::from).and_then(|d| archetypes_of(&d)) {
                Ok(found) => archetypes.extend(found),
                Err(e) => eprintln!("warning: {} is not a readable .ytyp; its archetypes are not used: {e:#}", path.display()),
            }
        }
        let (mut ydr, mut ydd, mut yft) = (HashMap::new(), HashMap::new(), HashMap::new());
        for path in models {
            let table = if has_ext(path, &["ydr"]) {
                &mut ydr
            } else if has_ext(path, &["ydd"]) {
                &mut ydd
            } else {
                &mut yft
            };
            table.insert(stem_hash(path), path.clone());
        }
        Sources {
            archetypes,
            ydr,
            ydd,
            yft,
            exe,
            keys,
            index: OnceCell::new(),
            game_ytyps: RefCell::default(),
            files: RefCell::default(),
            archetypes_from_game: RefCell::default(),
            models_from_game: RefCell::default(),
            warned_no_index: RefCell::new(false),
        }
    }

    fn index(&self) -> Option<&GameIndex> {
        let index = self.index.get_or_init(|| GameIndex::load(self.exe, self.keys, Parts::MODELS)).as_ref();
        if index.is_none() && self.exe.is_some() && !*self.warned_no_index.borrow() {
            *self.warned_no_index.borrow_mut() = true;
            eprintln!("warning: the game's archetypes and models are not used: no game index");
        }
        index
    }

    fn archetype(&self, a: u32) -> Option<ArchLights> {
        if let Some(found) = self.archetypes.get(&a) {
            return Some(*found);
        }
        self.exe?;
        let index = self.index()?;
        let ytyp = *index.archetype_ytyp.get(&a)?;
        if !self.game_ytyps.borrow().contains_key(&ytyp) {
            let found = index
                .ytyp_by_name
                .get(&ytyp)
                .and_then(|loc| index.load_bytes(loc, self.keys).ok())
                .and_then(|data| archetypes_of(&data).ok())
                .unwrap_or_default();
            self.game_ytyps.borrow_mut().insert(ytyp, found);
        }
        let found = self.game_ytyps.borrow().get(&ytyp)?.get(&a).copied();
        if found.is_some() {
            self.archetypes_from_game.borrow_mut().insert(a);
        }
        found
    }

    /// The bytes of the model file named `hash`, of the kind wanted (a
    /// dictionary, or a drawable or fragment), from the resource then the
    /// game.
    fn model_file(&self, hash: u32, dictionary: bool) -> ModelFile {
        let key = (hash, dictionary);
        if let Some(cached) = self.files.borrow().get(&key) {
            return cached.clone();
        }
        let local = if dictionary { self.ydd.get(&hash) } else { self.ydr.get(&hash).or_else(|| self.yft.get(&hash)) };
        let loaded = match local {
            Some(path) => match std::fs::read(path) {
                Ok(data) => Some(Rc::new(data)),
                Err(e) => {
                    eprintln!("warning: skipping {}: {e}", path.display());
                    None
                }
            },
            None => self.game_model(hash, dictionary),
        };
        self.files.borrow_mut().insert(key, loaded.clone());
        loaded
    }

    fn game_model(&self, hash: u32, dictionary: bool) -> ModelFile {
        self.exe?;
        let index = self.index()?;
        let loc = index.drawable_by_name.get(&hash)?;
        let ext = crate::resources::extension_of(&loc.inner_path).to_lowercase();
        if (ext == "ydd") != dictionary {
            return None;
        }
        match index.load_bytes(loc, self.keys) {
            Ok(data) => {
                self.models_from_game.borrow_mut().insert(hash);
                Some(Rc::new(data))
            }
            Err(e) => {
                eprintln!("warning: skipping {}: {e:#}", loc.inner_path);
                None
            }
        }
    }

    /// What the file named `hash` holds.
    fn kind_of(&self, hash: u32) -> Option<&'static str> {
        if self.ydr.contains_key(&hash) {
            return Some("ydr");
        }
        if self.yft.contains_key(&hash) {
            return Some("yft");
        }
        let loc = self.index()?.drawable_by_name.get(&hash)?;
        match crate::resources::extension_of(&loc.inner_path).to_lowercase().as_str() {
            "ydr" => Some("ydr"),
            "yft" => Some("yft"),
            _ => None,
        }
    }

    /// `GameFileCache.TryGetDrawable`: the dictionary member named after the
    /// archetype when it declares a dictionary (and nothing when the
    /// dictionary exists but lacks it), else its `.ydr`, else its `.yft`.
    fn model(&self, a: u32, arch: &ArchLights) -> Result<Option<ModelLights>> {
        if arch.drawable_dict != 0
            && let Some(data) = self.model_file(arch.drawable_dict, true)
        {
            refuse_escrowed(&data)?;
            return lodlights::read_ydd_member(&data, a);
        }
        let Some(data) = self.model_file(a, false) else { return Ok(None) };
        refuse_escrowed(&data)?;
        match self.kind_of(a) {
            Some("yft") => lodlights::read_yft(&data).map(Some),
            _ => lodlights::read_ydr(&data).map(Some),
        }
    }
}

/// A FiveM escrow-protected file holds nothing a tool can read.
fn refuse_escrowed(data: &[u8]) -> Result<()> {
    if rage_formats::is_fxap(data) {
        bail!("it is an escrowed asset (FXAP), which only the server can open");
    }
    Ok(())
}

/// What the generator found, for the report.
#[derive(Default)]
struct Tally {
    maps: usize,
    entities: usize,
    lights: usize,
    no_archetype: Vec<u32>,
    no_model: Vec<u32>,
}

fn push_once(v: &mut Vec<u32>, h: u32) {
    if !v.contains(&h) {
        v.push(h);
    }
}

fn list(names: &NameTable, hashes: &[u32]) -> String {
    let listed: Vec<String> = hashes.iter().take(8).map(|h| names.resolve(*h).into_owned()).collect();
    let more = if hashes.len() > 8 { format!(" and {} more", hashes.len() - 8) } else { String::new() };
    format!("{}{more}", listed.join(", "))
}

pub fn run(args: &LodlightsArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    let maps = files_of(&args.inputs, &["ymap"], "input")?;
    if maps.is_empty() {
        bail!("no .ymap files among the inputs");
    }
    let root = resource_root(&args.inputs[0]);
    let ytyps = if args.ytyp.is_empty() { files_of(std::slice::from_ref(&root), &["ytyp"], "--ytyp")? } else { files_of(&args.ytyp, &["ytyp"], "--ytyp")? };
    let models = if args.models.is_empty() { files_of(std::slice::from_ref(&root), &["ydr", "ydd", "yft"], "--models")? } else { files_of(&args.models, &["ydr", "ydd", "yft"], "--models")? };
    let sources = Sources::new(&ytyps, &models, exe, keys);

    let name = match &args.name {
        Some(n) => n.clone(),
        None => root.file_name().map(|s| s.to_string_lossy().into_owned()).filter(|s| !s.is_empty()).context("the resource folder has no name to call the maps by; pass --name")?,
    };
    let out_dir = match &args.output {
        Some(d) => d.clone(),
        None => {
            let first = &args.inputs[0];
            if first.is_dir() { first.clone() } else { first.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf() }
        }
    };

    let names = crate::names::load(&[], Some(&maps[0]))?;
    let mut tally = Tally::default();
    let mut lights: Vec<LodLight> = Vec::new();
    let mut models_read: HashMap<u32, Option<Rc<ModelLights>>> = HashMap::new();
    for path in &maps {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let ymap = parse_ymap(&data).with_context(|| format!("{}", path.display()))?;
        tally.maps += 1;
        for e in &ymap.entities {
            tally.entities += 1;
            let Some(arch) = sources.archetype(e.archetype_hash) else {
                push_once(&mut tally.no_archetype, e.archetype_hash);
                continue;
            };
            let model = models_read
                .entry(e.archetype_hash)
                .or_insert_with(|| match sources.model(e.archetype_hash, &arch) {
                    Ok(Some(m)) => Some(Rc::new(m)),
                    Ok(None) => None,
                    Err(err) => {
                        eprintln!("warning: the model of {} cannot be read: {err:#}", names.resolve(e.archetype_hash));
                        None
                    }
                })
                .clone();
            let Some(model) = model else {
                push_once(&mut tally.no_model, e.archetype_hash);
                continue;
            };
            let placement = Placement::new(e.position, e.rotation, e.scale_xy, e.scale_z, e.is_mlo_instance);
            lights.extend(lodlights::entity_lights(&placement, &arch, &model));
        }
    }
    tally.lights = lights.len();

    if !tally.no_archetype.is_empty() {
        let hint = if exe.is_none() { "; pass --ytyp, or --exe / GTAV_PATH for vanilla archetypes" } else { "; pass --ytyp with the files that declare them" };
        eprintln!("warning: no archetype for {} entities' models: {}{hint}", tally.no_archetype.len(), list(&names, &tally.no_archetype));
    }
    if !tally.no_model.is_empty() {
        let hint = if exe.is_none() { "; pass --models, or --exe / GTAV_PATH for vanilla models" } else { "; pass --models with the files that hold them" };
        eprintln!("warning: no model for {} archetypes: {}{hint}", tally.no_model.len(), list(&names, &tally.no_model));
    }
    if lights.is_empty() {
        bail!("no lights found in {} map(s): {} entities, none with a model that has lights", tally.maps, tally.entities);
    }

    lodlights::sort_lights(&mut lights);
    let (lod, dist) = lodlights::headers(&name, &lights);
    let lod_map = build_map(&lod, &lights, &[])?;
    let dist_map = build_map(&dist, &[], &lights)?;
    std::fs::create_dir_all(&out_dir).with_context(|| format!("failed to create {}", out_dir.display()))?;
    let lod_path = out_dir.join(format!("{}.ymap", lod.name));
    let dist_path = out_dir.join(format!("{}.ymap", dist.name));
    std::fs::write(&lod_path, &lod_map).with_context(|| format!("writing {}", lod_path.display()))?;
    std::fs::write(&dist_path, &dist_map).with_context(|| format!("writing {}", dist_path.display()))?;

    let from_game = (sources.archetypes_from_game.borrow().len(), sources.models_from_game.borrow().len());
    let mut sources_note = format!("archetypes from {} .ytyp, models from {} files", ytyps.len(), models.len());
    if from_game != (0, 0) {
        sources_note.push_str(&format!("; {} archetypes and {} model files from the game", from_game.0, from_game.1));
    }
    eprintln!(
        "Wrote {} and {} ({} lights from {} entities in {} map(s); {sources_note})",
        lod_path.display(),
        dist_path.display(),
        tally.lights,
        tally.entities,
        tally.maps
    );
    Ok(())
}

/// One of the two maps as a file: the header, then the light rows.
fn build_map(h: &lodlights::MapHeader, lod: &[LodLight], distant: &[LodLight]) -> Result<Vec<u8>> {
    let xml = super::ymap::map_document(&h.name, &h.parent, h.flags, h.content_flags, Some((h.entities_extents, h.streaming_extents)), "", "");
    let mut value = from_xml(&xml).context("building the map")?;
    let MetaValue::Struct(map) = &mut value else { unreachable!("a map document is a structure") };
    lodlights::set_member(map, "LODLightsSOA", lodlights::lod_lights_soa(lod));
    lodlights::set_member(map, "DistantLODLightsSOA", lodlights::distant_lights_soa(distant));
    let written = build_meta(&value, Schema::builtin())?;
    for warning in &written.warnings {
        eprintln!("warning: {}: {warning}", h.name);
    }
    dump_meta(&written.bytes).with_context(|| format!("{}: the written file does not read back", h.name))?;
    Ok(written.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rage_formats::{rage_joaat, Vec3};

    fn light(pos: Vec3, hash: u32) -> LodLight {
        LodLight { position: pos, colour: 0xFF80_4020, direction: Vec3::new(0.0, 0.0, -1.0), falloff: 5.0, falloff_exponent: 30.0, time_and_state_flags: 15728703 | (2 << 26), hash, cone_inner_angle: 42, cone_outer_angle_or_cap_ext: 127, corona_intensity: 6, is_street_light: false }
    }

    #[test]
    fn the_two_maps_write_and_read_back_with_their_rows() {
        let lights = vec![light(Vec3::new(1.0, 2.0, 3.0), 7), light(Vec3::new(10.0, 20.0, 30.0), 9)];
        let (lod, dist) = lodlights::headers("park", &lights);
        let lod_bytes = build_map(&lod, &lights, &[]).unwrap();
        let dist_bytes = build_map(&dist, &[], &lights).unwrap();

        let header = rage_formats::parse_ymap_header(&lod_bytes).unwrap();
        assert_eq!((header.name_hash, header.parent_hash, header.flags, header.content_flags), (rage_joaat("park_lodlights"), rage_joaat("park_distantlights"), 0, 128));
        assert_eq!(header.entities_extents_min, Vec3::new(-19.0, -18.0, -17.0));
        assert_eq!(header.streaming_extents_max, Vec3::new(960.0, 970.0, 980.0));
        let header = rage_formats::parse_ymap_header(&dist_bytes).unwrap();
        assert_eq!((header.name_hash, header.parent_hash, header.flags, header.content_flags), (rage_joaat("park_distantlights"), 0, 2, 256));
        assert_eq!(header.streaming_extents_min, Vec3::new(-2999.0, -2998.0, -2997.0));

        let dump = dump_meta(&lod_bytes).unwrap();
        assert!(dump.warnings.is_empty(), "{:?}", dump.warnings);
        let map = dump.root.as_struct().unwrap();
        let soa = map.field("LODLightsSOA").and_then(MetaValue::as_struct).unwrap();
        let items = |n: &str| soa.field(n).unwrap().items().to_vec();
        assert_eq!(items("direction"), vec![MetaValue::Vec3(Vec3::new(0.0, 0.0, -1.0)); 2]);
        assert_eq!(items("hash").iter().map(|v| v.as_u32().unwrap()).collect::<Vec<_>>(), vec![7, 9]);
        assert_eq!(items("timeAndStateFlags")[0].as_u32(), Some(15728703 | (2 << 26)));
        assert_eq!(items("coneInnerAngle").iter().map(|v| v.as_i64().unwrap()).collect::<Vec<_>>(), vec![42, 42]);
        assert_eq!(items("coneOuterAngleOrCapExt")[1].as_i64(), Some(127));
        assert_eq!(items("coronaIntensity")[0].as_i64(), Some(6));
        assert_eq!(items("falloffExponent")[0].as_f32(), Some(30.0));
        let distant = map.field("DistantLODLightsSOA").and_then(MetaValue::as_struct).unwrap();
        assert!(distant.field("position").unwrap().items().is_empty());

        let dump = dump_meta(&dist_bytes).unwrap();
        assert!(dump.warnings.is_empty(), "{:?}", dump.warnings);
        let map = dump.root.as_struct().unwrap();
        let distant = map.field("DistantLODLightsSOA").and_then(MetaValue::as_struct).unwrap();
        assert_eq!(distant.field("position").unwrap().items()[1], MetaValue::Vec3(Vec3::new(10.0, 20.0, 30.0)));
        assert_eq!(distant.field("RGBI").unwrap().items()[0].as_u32(), Some(0xFF80_4020));
        assert_eq!(distant.field("numStreetLights").and_then(MetaValue::as_i64), Some(0));
        assert_eq!(distant.field("category").and_then(MetaValue::as_i64), Some(1));
        assert!(map.field("LODLightsSOA").and_then(MetaValue::as_struct).unwrap().field("direction").unwrap().items().is_empty());
    }

    #[test]
    fn archetypes_come_with_their_box_dictionary_and_extension_count() {
        let xml = r#"<CMapTypes><archetypes>
            <Item type="CBaseArchetypeDef"><lodDist value="100"/><flags value="0"/><bbMin x="-1" y="-2" z="0"/><bbMax x="1" y="2" z="3"/>
              <name>prop_lamp</name><textureDictionary/><drawableDictionary>lamps</drawableDictionary><assetType>ASSET_TYPE_DRAWABLEDICTIONARY</assetType><assetName>prop_lamp</assetName>
              <extensions><Item type="CExtensionDefAudioEmitter"><name>prop_lamp</name><offsetPosition x="0" y="0" z="0"/></Item></extensions></Item>
            <Item type="CBaseArchetypeDef"><bbMin x="0" y="0" z="0"/><bbMax x="1" y="1" z="1"/><name>prop_plain</name><drawableDictionary/><extensions/></Item>
        </archetypes></CMapTypes>"#;
        let bytes = build_meta(&from_xml(xml).unwrap(), Schema::builtin()).unwrap().bytes;
        let archetypes = archetypes_of(&bytes).unwrap();
        let lamp = archetypes[&rage_joaat("prop_lamp")];
        assert_eq!(lamp, ArchLights { bb_min: Vec3::new(-1.0, -2.0, 0.0), bb_max: Vec3::new(1.0, 2.0, 3.0), drawable_dict: rage_joaat("lamps"), extensions: 1 });
        let plain = archetypes[&rage_joaat("prop_plain")];
        assert_eq!((plain.drawable_dict, plain.extensions), (0, 0));
    }
}
