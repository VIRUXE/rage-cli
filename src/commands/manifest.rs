//! `rage manifest generate`: a `_manifest.ymf` worked out from a resource
//! folder, the way CodeWalker's project window generates one
//! (`EditProjectManifestPanel.GenerateProjectManifest`). Every map lists the
//! type files its entities and grass come from (`imapDependencies_2`), maps
//! placing an interior are flagged `INTERIOR_DATA`, every interior's type
//! file lists the other type files its rooms and entity sets draw on
//! (`itypDependencies_2`), and every interior gets its collision entry
//! (`Interiors`).

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rage_formats::{build_pso, dump_meta, from_xml, parse_ymf, parse_ytyp, rage_joaat, MetaStruct, MetaValue, NameTable, Schema};

use crate::extents::name_hash;
use crate::index::{GameIndex, Parts};
use crate::rpf::GtaKeys;

#[derive(clap::Args)]
pub struct ManifestArgs {
    #[command(subcommand)]
    pub command: ManifestCommand,
}

#[derive(clap::Subcommand)]
pub enum ManifestCommand {
    /// Work a _manifest.ymf out from the .ymap and .ytyp files of a resource folder
    Generate(GenerateArgs),
}

#[derive(clap::Args)]
pub struct GenerateArgs {
    /// The resource (or its stream folder); every .ymap and .ytyp under it is read
    pub folder: PathBuf,

    /// The file to write; default: _manifest.ymf in the folder's stream/
    /// (the folder itself when it has none), or stdout with --format xml
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// pso (the binary .ymf the game reads) or xml (CodeWalker's text);
    /// default: by the output extension, else pso
    #[arg(long, value_name = "pso|xml")]
    pub format: Option<String>,
}

pub fn run(args: &ManifestArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    match &args.command {
        ManifestCommand::Generate(a) => run_generate(a, keys, exe),
    }
}

/// A `.ytyp` in the folder: its name as the manifest spells it and what it declares.
struct TypeFile {
    name: String,
    archetypes: Vec<u32>,
    mlos: Vec<rage_formats::MloDef>,
}

/// A `.ymap` in the folder, reduced to what the manifest needs.
struct MapFile {
    name: String,
    /// Archetypes of its entities, interior placements and grass batches, in order.
    archetypes: Vec<u32>,
    /// Archetypes it places as interiors.
    interiors: Vec<u32>,
}

/// Names archetypes' type files: the folder's own first, then the game's.
struct Resolver<'a> {
    local: HashMap<u32, usize>,
    exe: Option<&'a Path>,
    keys: Option<&'a GtaKeys>,
    index: std::cell::OnceCell<Option<GameIndex>>,
    /// Archetypes the game supplied.
    from_game: std::cell::RefCell<std::collections::HashSet<u32>>,
}

impl Resolver<'_> {
    fn ytyp_of(&self, archetype: u32, types: &[TypeFile]) -> Option<String> {
        if let Some(&i) = self.local.get(&archetype) {
            return Some(types[i].name.clone());
        }
        self.exe?;
        let index = self.index.get_or_init(|| GameIndex::load(self.exe, self.keys, Parts::MODELS)).as_ref()?;
        let name = index.ytyp_names.get(index.archetype_ytyp.get(&archetype)?)?;
        self.from_game.borrow_mut().insert(archetype);
        Some(name.clone())
    }
}

/// Dependency lists in first-seen order, as CodeWalker's dictionaries keep them.
#[derive(Default)]
struct Ordered(Vec<(String, Vec<String>)>);

impl Ordered {
    fn entry(&mut self, key: &str) -> &mut Vec<String> {
        let i = match self.0.iter().position(|(k, _)| k == key) {
            Some(i) => i,
            None => {
                self.0.push((key.to_string(), Vec::new()));
                self.0.len() - 1
            }
        };
        &mut self.0[i].1
    }
}

fn add(list: &mut Vec<String>, name: String) {
    if !list.contains(&name) {
        list.push(name);
    }
}

fn stem(path: &Path) -> String {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase()
}

fn run_generate(args: &GenerateArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    if !args.folder.is_dir() {
        bail!("{}: not a folder", args.folder.display());
    }
    let xml_out = match (args.format.as_deref().map(str::to_lowercase).as_deref(), &args.output) {
        (Some("xml"), _) => true,
        (Some("pso"), _) => false,
        (Some(other), _) => bail!("--format {other}: expected pso or xml"),
        (None, Some(out)) => out.extension().is_some_and(|e| e.eq_ignore_ascii_case("xml")),
        (None, None) => false,
    };
    let output = match &args.output {
        Some(out) => Some(out.clone()),
        None if xml_out => None,
        None => {
            let stream = args.folder.join("stream");
            Some(if stream.is_dir() { stream } else { args.folder.clone() }.join("_manifest.ymf"))
        }
    };

    let mut files: Vec<PathBuf> = crate::utils::walkdir(&args.folder)?
        .into_iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("ymap") || e.eq_ignore_ascii_case("ytyp")))
        .collect();
    files.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    // Every file stem under the folder names what it holds.
    let names = crate::names::load(&[], Some(&args.folder.join("_manifest.ymf")))?;

    let mut types: Vec<TypeFile> = Vec::new();
    let mut maps: Vec<MapFile> = Vec::new();
    for path in &files {
        let data = std::fs::read(path).with_context(|| format!("failed to read '{}'", path.display()))?;
        if rage_formats::is_fxap(&data) {
            eprintln!("warning: skipping {}: FiveM escrow-encrypted, only the server that bought it can decrypt it", path.display());
            continue;
        }
        let is_ytyp = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ytyp"));
        let name = stem(path);
        if is_ytyp {
            let ytyp = match parse_ytyp(&data) {
                Ok(y) => y,
                Err(e) => {
                    eprintln!("warning: skipping {}: {e:#}", path.display());
                    continue;
                }
            };
            if types.iter().any(|t| t.name == name) {
                eprintln!("warning: two type files named {name}.ytyp; the game streams only one of them ({})", path.display());
            }
            types.push(TypeFile { name, archetypes: ytyp.archetypes.iter().map(|a| a.name_hash).collect(), mlos: ytyp.mlos });
        } else {
            let root = match dump_meta(&data) {
                Ok(d) => d.root,
                Err(e) => {
                    eprintln!("warning: skipping {}: {e:#}", path.display());
                    continue;
                }
            };
            let Some(map) = root.as_struct().filter(|s| s.type_hash == rage_joaat("CMapData")) else {
                eprintln!("warning: skipping {}: not a map (no CMapData)", path.display());
                continue;
            };
            if maps.iter().any(|m| m.name == name) {
                eprintln!("warning: two maps named {name}.ymap; the game streams only one of them ({})", path.display());
            }
            maps.push(read_map(name, map));
        }
    }
    if types.is_empty() && maps.is_empty() {
        bail!("no readable .ymap or .ytyp under {}", args.folder.display());
    }

    let mut local = HashMap::new();
    for (i, t) in types.iter().enumerate() {
        for a in &t.archetypes {
            local.entry(*a).or_insert(i);
        }
    }
    let resolver = Resolver { local, exe, keys, index: Default::default(), from_game: Default::default() };
    let generated = generate(&maps, &types, &resolver, &names);

    let xml = generated.xml;
    match &output {
        None => print!("{xml}"),
        Some(out) => {
            let bytes = if xml_out {
                xml.clone().into_bytes()
            } else {
                let value = from_xml(&xml).context("the generated manifest does not parse")?;
                let written = build_pso(&value, Schema::builtin())?;
                for warning in &written.warnings {
                    eprintln!("warning: {warning}");
                }
                parse_ymf(&written.bytes).context("the written manifest does not read back")?;
                written.bytes
            };
            if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
            }
            std::fs::write(out, &bytes).with_context(|| format!("writing {}", out.display()))?;
            eprintln!(
                "Wrote {} ({} bytes, {}): {} map(s), {} interior type file(s), {} interior(s)",
                out.display(),
                bytes.len(),
                if xml_out { "XML" } else { "PSO" },
                maps.len(),
                generated.ityp_deps,
                generated.interiors,
            );
        }
    }

    let from_game = resolver.from_game.borrow().len();
    if from_game > 0 {
        eprintln!("{from_game} archetype(s) resolved to the game's own type files");
    }
    if !generated.unresolved.is_empty() {
        let listed: Vec<String> = generated.unresolved.iter().take(8).map(|h| names.resolve(*h).into_owned()).collect();
        let more = if generated.unresolved.len() > 8 { format!(" and {} more", generated.unresolved.len() - 8) } else { String::new() };
        let hint = if exe.is_none() { "; pass --exe / GTAV_PATH so the game's own archetypes resolve" } else { "" };
        eprintln!(
            "warning: {} archetype(s) are declared by no .ytyp here nor in the game, so no dependency is listed for them: {}{more}{hint}",
            generated.unresolved.len(),
            listed.join(", ")
        );
    }
    Ok(())
}

fn read_map(name: String, map: &MetaStruct) -> MapFile {
    let items = |s: &MetaStruct, field: &str| -> Vec<MetaStruct> {
        s.field(field).map_or(&[][..], MetaValue::items).iter().filter_map(MetaValue::as_struct).cloned().collect()
    };
    let mlo_type = rage_joaat("CMloInstanceDef");
    let mut out = MapFile { name, archetypes: Vec::new(), interiors: Vec::new() };
    for e in items(map, "entities") {
        let Some(a) = e.field("archetypeName").and_then(name_hash) else { continue };
        out.archetypes.push(a);
        if e.type_hash == mlo_type {
            out.interiors.push(a);
        }
    }
    if let Some(data) = map.field("instancedData").and_then(MetaValue::as_struct) {
        for batch in items(data, "GrassInstanceList") {
            if let Some(a) = batch.field("archetypeName").and_then(name_hash) {
                out.archetypes.push(a);
            }
        }
    }
    out
}

struct Generated {
    xml: String,
    ityp_deps: usize,
    interiors: usize,
    /// Archetypes no type file could be found for, in first-seen order.
    unresolved: Vec<u32>,
}

/// CodeWalker's manifest text: the same elements, in the same order.
fn generate(maps: &[MapFile], types: &[TypeFile], resolver: &Resolver, names: &NameTable) -> Generated {
    let mut unresolved: Vec<u32> = Vec::new();
    let mut resolve = |a: u32| {
        let found = resolver.ytyp_of(a, types);
        if found.is_none() && !unresolved.contains(&a) {
            unresolved.push(a);
        }
        found
    };
    let mlo_of = |a: u32| types.iter().find_map(|t| t.mlos.iter().find(|m| m.name_hash == a).map(|m| (t, m)));

    let mut typdeps = Ordered::default();
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>\n<CPackFileMetaData>\n");
    s.push_str("  <MapDataGroups/>\n  <HDTxdBindingArray/>\n  <imapDependencies/>\n");

    if maps.is_empty() {
        s.push_str("  <imapDependencies_2/>\n");
    } else {
        s.push_str("  <imapDependencies_2>\n");
        for map in maps {
            let mut deps = Vec::new();
            for &a in &map.archetypes {
                if let Some(y) = resolve(a) {
                    add(&mut deps, y);
                }
            }
            // An interior placed here: its rooms' type files, as CodeWalker
            // lists them from the placed instance (entity sets come from
            // the type file pass below).
            for &a in &map.interiors {
                let Some((t, mlo)) = mlo_of(a) else { continue };
                let list = typdeps.entry(&t.name);
                for e in &mlo.entities {
                    if let Some(y) = resolve(e.archetype_hash).filter(|y| *y != t.name) {
                        add(list, y);
                    }
                }
            }
            s.push_str("    <Item>\n");
            writeln!(s, "      <imapName>{}</imapName>", map.name).unwrap();
            if map.interiors.is_empty() {
                s.push_str("      <manifestFlags/>\n");
            } else {
                s.push_str("      <manifestFlags>INTERIOR_DATA</manifestFlags>\n");
            }
            s.push_str("      <itypDepArray>\n");
            for d in &deps {
                writeln!(s, "        <Item>{d}</Item>").unwrap();
            }
            s.push_str("      </itypDepArray>\n    </Item>\n");
        }
        s.push_str("  </imapDependencies_2>\n");
    }

    let mut interiors = Vec::new();
    for t in types {
        for mlo in &t.mlos {
            interiors.push(names.resolve(mlo.name_hash).into_owned());
            let list = typdeps.entry(&t.name);
            let set_entities = mlo.entity_sets.iter().flat_map(|set| &set.entities);
            for e in mlo.entities.iter().chain(set_entities) {
                if let Some(y) = resolve(e.archetype_hash).filter(|y| *y != t.name) {
                    add(list, y);
                }
            }
        }
    }

    if typdeps.0.is_empty() {
        s.push_str("  <itypDependencies_2/>\n");
    } else {
        s.push_str("  <itypDependencies_2>\n");
        for (ytyp, deps) in &typdeps.0 {
            s.push_str("    <Item>\n");
            writeln!(s, "      <itypName>{ytyp}</itypName>").unwrap();
            s.push_str("      <manifestFlags>INTERIOR_DATA</manifestFlags>\n      <itypDepArray>\n");
            for d in deps {
                writeln!(s, "        <Item>{d}</Item>").unwrap();
            }
            s.push_str("      </itypDepArray>\n    </Item>\n");
        }
        s.push_str("  </itypDependencies_2>\n");
    }

    if interiors.is_empty() {
        s.push_str("  <Interiors/>\n");
    } else {
        s.push_str("  <Interiors itemType=\"CInteriorBoundsFiles\">\n");
        for name in &interiors {
            writeln!(s, "    <Item>\n      <Name>{name}</Name>\n      <Bounds>\n        <Item>{name}</Item>\n      </Bounds>\n    </Item>").unwrap();
        }
        s.push_str("  </Interiors>\n");
    }
    s.push_str("</CPackFileMetaData>\n");

    Generated { xml: s, ityp_deps: typdeps.0.len(), interiors: interiors.len(), unresolved }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rage_formats::{MloDef, MloEntitySet, Vec3, YmapEntity};

    fn entity(archetype: &str) -> YmapEntity {
        YmapEntity {
            archetype_hash: rage_joaat(archetype),
            flags: 0,
            guid: 0,
            position: Vec3::new(0.0, 0.0, 0.0),
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale_xy: 1.0,
            scale_z: 1.0,
            parent_index: -1,
            lod_dist: 0.0,
            child_lod_dist: -1.0,
            lod_level: 0,
            num_children: 0,
            is_mlo_instance: false,
        }
    }

    fn types() -> Vec<TypeFile> {
        vec![
            TypeFile { name: "props".into(), archetypes: vec![rage_joaat("chair"), rage_joaat("lamp")], mlos: vec![] },
            TypeFile {
                name: "int_shop".into(),
                archetypes: vec![rage_joaat("int_shop"), rage_joaat("shop_shell")],
                mlos: vec![MloDef {
                    name_hash: rage_joaat("int_shop"),
                    entities: vec![entity("shop_shell"), entity("chair")],
                    rooms: vec![],
                    portals: vec![],
                    entity_sets: vec![MloEntitySet { name_hash: 1, locations: vec![0], entities: vec![entity("lamp"), entity("prop_nowhere")] }],
                }],
            },
        ]
    }

    fn resolver(types: &[TypeFile]) -> Resolver<'static> {
        let mut local = HashMap::new();
        for (i, t) in types.iter().enumerate() {
            for a in &t.archetypes {
                local.insert(*a, i);
            }
        }
        Resolver { local, exe: None, keys: None, index: Default::default(), from_game: Default::default() }
    }

    fn names() -> NameTable {
        let mut n = NameTable::default();
        n.add("int_shop");
        n
    }

    #[test]
    fn lists_what_codewalker_lists() {
        let types = types();
        let maps = vec![
            MapFile { name: "shop_ext".into(), archetypes: vec![rage_joaat("chair"), rage_joaat("int_shop")], interiors: vec![rage_joaat("int_shop")] },
            MapFile { name: "street".into(), archetypes: vec![rage_joaat("lamp"), rage_joaat("chair")], interiors: vec![] },
        ];
        let g = generate(&maps, &types, &resolver(&types), &names());
        let expected = "\
<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>
<CPackFileMetaData>
  <MapDataGroups/>
  <HDTxdBindingArray/>
  <imapDependencies/>
  <imapDependencies_2>
    <Item>
      <imapName>shop_ext</imapName>
      <manifestFlags>INTERIOR_DATA</manifestFlags>
      <itypDepArray>
        <Item>props</Item>
        <Item>int_shop</Item>
      </itypDepArray>
    </Item>
    <Item>
      <imapName>street</imapName>
      <manifestFlags/>
      <itypDepArray>
        <Item>props</Item>
      </itypDepArray>
    </Item>
  </imapDependencies_2>
  <itypDependencies_2>
    <Item>
      <itypName>int_shop</itypName>
      <manifestFlags>INTERIOR_DATA</manifestFlags>
      <itypDepArray>
        <Item>props</Item>
      </itypDepArray>
    </Item>
  </itypDependencies_2>
  <Interiors itemType=\"CInteriorBoundsFiles\">
    <Item>
      <Name>int_shop</Name>
      <Bounds>
        <Item>int_shop</Item>
      </Bounds>
    </Item>
  </Interiors>
</CPackFileMetaData>
";
        assert_eq!(g.xml, expected);
        assert_eq!(g.unresolved, vec![rage_joaat("prop_nowhere")]);
    }

    #[test]
    fn the_text_builds_as_a_pso_manifest() {
        let types = types();
        let maps = vec![MapFile { name: "shop_ext".into(), archetypes: vec![rage_joaat("int_shop")], interiors: vec![rage_joaat("int_shop")] }];
        let g = generate(&maps, &types, &resolver(&types), &names());
        let written = build_pso(&from_xml(&g.xml).unwrap(), Schema::builtin()).unwrap();
        assert!(written.warnings.is_empty(), "{:?}", written.warnings);
        let (_, m) = parse_ymf(&written.bytes).unwrap();
        assert_eq!(m.imap_dependencies_2.len(), 1);
        let hashes = |d: &rage_formats::Dependencies| d.ityp_deps.iter().map(|n| n.hash).collect::<Vec<_>>();
        assert_eq!(m.imap_dependencies_2[0].name.hash, rage_joaat("shop_ext"));
        assert_eq!(m.imap_dependencies_2[0].manifest_flags & 1, 1);
        assert_eq!(hashes(&m.imap_dependencies_2[0]), vec![rage_joaat("int_shop")]);
        assert_eq!(hashes(&m.ityp_dependencies_2[0]), vec![rage_joaat("props")]);
        assert_eq!(m.interiors.len(), 1);
    }

    #[test]
    fn an_empty_folder_writes_the_empty_elements() {
        let g = generate(&[], &[], &resolver(&[]), &names());
        assert!(g.xml.contains("<imapDependencies_2/>") && g.xml.contains("<itypDependencies_2/>") && g.xml.contains("<Interiors/>"));
    }
}
