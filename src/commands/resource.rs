//! `rage resource info` and `rage resource dump`: inspect a loose resource
//! or metadata file (or an entry inside an archive). `info` prints the
//! container header and a summary of what the file holds — textures,
//! drawables, a map's entities, a type file's archetypes, a manifest's
//! dependencies. `dump` writes any Meta or PSO file out whole, as XML in
//! CodeWalker's layout or as JSON.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use rage_formats::{
    dump_meta, dump_metadata, parse_drawables, parse_ymap, parse_ymf, parse_ytd, parse_ytyp, prepare_rsc7, rage_joaat,
    resource_size_from_flags, resource_version_from_flags, to_json, to_xml, DrawableEntry, DrawableKind, Manifest,
    MetaContainer, MetaDump, NameTable, Vec3, Ymap, YmapEntity, YmapHeader, Ytyp, YtdTexture, RSC7_MAGIC, RSC8_MAGIC,
};

use crate::resources::load_resource_bytes;
use crate::rpf::GtaKeys;
use crate::utils::json_string;

#[derive(clap::Args)]
pub struct ResourceArgs {
    #[command(subcommand)]
    pub command: ResourceCommand,
}

#[derive(clap::Subcommand)]
pub enum ResourceCommand {
    /// Print the header and a summary of a .ydr/.ydd/.yft/.ytd/.ymap/.ytyp/.ymf
    Info(InfoArgs),
    /// Write a Meta or PSO file (.ymap .ytyp .ymt .ymf .pso) out as XML or JSON
    Dump(DumpArgs),
}

#[derive(clap::Args)]
pub struct InfoArgs {
    /// A loose resource file on disk, or (with --archive) a name inside the archive
    pub file: String,

    /// Look `FILE` up inside this RPF archive instead of on disk
    #[arg(short, long, value_name = "RPF")]
    pub archive: Option<PathBuf>,

    /// Print one JSON object instead of text
    #[arg(long)]
    pub json: bool,

    /// How many entities of a map to list (0 for all)
    #[arg(long, default_value = "50", value_name = "N")]
    pub limit: usize,

    /// Extra name lists (one name per line) for resolving hashes; repeatable
    #[arg(long, value_name = "FILE")]
    pub names: Vec<PathBuf>,
}

#[derive(clap::Args)]
pub struct DumpArgs {
    /// A loose file on disk, or (with --archive) a name inside the archive
    pub file: String,

    /// Look `FILE` up inside this RPF archive instead of on disk
    #[arg(short, long, value_name = "RPF")]
    pub archive: Option<PathBuf>,

    /// Write JSON instead of XML
    #[arg(long)]
    pub json: bool,

    /// Write here instead of stdout
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Extra name lists (one name per line) for resolving hashes; repeatable
    #[arg(long, value_name = "FILE")]
    pub names: Vec<PathBuf>,
}

/// "FXAP": the header Cfx.re asset escrow puts on encrypted stream files.
const FXAP_MAGIC: u32 = 0x5041_5846;

/// The 16-byte RSC7 header plus what could be learnt about the body.
#[derive(Debug, PartialEq)]
pub struct Rsc7Header {
    pub version: u32,
    pub system_flags: u32,
    pub graphics_flags: u32,
    pub system_size: usize,
    pub graphics_size: usize,
    /// Body is deflated (false when it is stored raw).
    pub compressed: bool,
    /// Size of the body as read, in bytes.
    pub body_len: usize,
}

/// What the first bytes say the file is.
#[derive(Debug, PartialEq)]
pub enum Container {
    Rsc7(Rsc7Header),
    /// PSO, RBF or XML metadata, with no RSC7 header to report.
    Meta(MetaContainer),
}

/// Recognises the container. An escrowed or Gen9 file is an error that
/// says which; so is anything that is none of RSC7, PSO, RBF or XML.
pub fn detect(data: &[u8]) -> Result<Container> {
    if data.len() >= 4 {
        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if magic == FXAP_MAGIC {
            bail!("FiveM escrow-encrypted asset (magic 'FXAP'); only the server that bought it can decrypt it");
        }
        if magic == RSC8_MAGIC {
            bail!("Gen9 RSC8 resources are not supported (magic 0x{magic:08X})");
        }
        if magic == RSC7_MAGIC {
            return Ok(Container::Rsc7(parse_header(data)?));
        }
        match MetaContainer::detect(data) {
            Some(MetaContainer::Meta) | None => {}
            Some(other) => return Ok(Container::Meta(other)),
        }
        bail!("not an RSC7 resource (magic 0x{magic:08X}, expected 0x{RSC7_MAGIC:08X}), nor a PSO, RBF or XML file");
    }
    bail!("file is {} bytes; an RSC7 header needs 16", data.len());
}

pub fn parse_header(data: &[u8]) -> Result<Rsc7Header> {
    if data.len() < 16 {
        bail!("file is {} bytes; an RSC7 header needs 16", data.len());
    }

    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if magic == FXAP_MAGIC {
        bail!("FiveM escrow-encrypted asset (magic 'FXAP'); only the server that bought it can decrypt it");
    }
    if magic == RSC8_MAGIC {
        bail!("Gen9 RSC8 resources are not supported (magic 0x{magic:08X})");
    }
    if magic != RSC7_MAGIC {
        bail!("not an RSC7 resource (magic 0x{magic:08X}, expected 0x{RSC7_MAGIC:08X})");
    }

    let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let system_flags = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let graphics_flags = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let system_size = resource_size_from_flags(system_flags);
    let graphics_size = resource_size_from_flags(graphics_flags);

    // prepare_rsc7 inflates when it can and otherwise hands the body back as
    // stored; the two cases are told apart by whether the system section it
    // returned is literally the start of the body.
    let body = &data[16..];
    let (system, _) = prepare_rsc7(data)?;
    let compressed = !(body.len() >= system_size + graphics_size && body[..system_size] == system[..]);

    Ok(Rsc7Header {
        version, system_flags, graphics_flags, system_size, graphics_size,
        compressed, body_len: body.len(),
    })
}

/// What the body was parsed as, keyed off the file extension and, failing
/// that, the container.
pub enum Contents {
    Textures(Vec<YtdTexture>),
    Drawables(Vec<DrawableEntry>),
    Map(Ymap),
    Types(Ytyp),
    Manifest(Manifest),
    /// Any other self-describing metadata: the generic tree.
    Meta(MetaDump),
    Other,
}

fn extension_of(name: &str) -> String {
    Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
}

fn parse_contents(name: &str, data: &[u8], container: &Container) -> Result<Contents> {
    let ext = extension_of(name);
    match container {
        Container::Rsc7(_) => {
            if ext == "ytd" {
                return Ok(Contents::Textures(parse_ytd(data).context("failed to parse texture dictionary")?));
            }
            if let Some(kind) = DrawableKind::from_extension(&ext) {
                return Ok(Contents::Drawables(parse_drawables(data, kind).context("failed to parse drawable")?));
            }
            match ext.as_str() {
                "ymap" => Ok(Contents::Map(parse_ymap(data).context("failed to parse map")?)),
                "ytyp" => Ok(Contents::Types(parse_ytyp(data).context("failed to parse type file")?)),
                "ymf" => parse_ymf(data).map(|(_, m)| Contents::Manifest(m)).context("failed to parse manifest"),
                // A Meta file of any other kind (a .ymt, say) still carries
                // its schema; anything else is a fixed-layout resource this
                // command has no summary for.
                _ => Ok(dump_meta(data).map_or(Contents::Other, Contents::Meta)),
            }
        }
        Container::Meta(kind) => {
            if ext == "ymf" {
                return parse_ymf(data).map(|(_, m)| Contents::Manifest(m)).context("failed to parse manifest");
            }
            let (found, dump) = dump_metadata(data)?;
            debug_assert_eq!(found, *kind);
            match Manifest::from_value(&dump.root) {
                Ok(m) => Ok(Contents::Manifest(m)),
                Err(_) => Ok(Contents::Meta(dump)),
            }
        }
    }
}

pub fn run(args: &ResourceArgs, keys: Option<&GtaKeys>, verbose: bool) -> Result<()> {
    match &args.command {
        ResourceCommand::Info(info) => run_info(info, keys, verbose),
        ResourceCommand::Dump(dump) => run_dump(dump, keys),
    }
}

/// The file on disk whose siblings name the hashes, when the input is one.
fn local_path(file: &str, archive: Option<&Path>) -> Option<PathBuf> {
    archive.is_none().then(|| PathBuf::from(file))
}

fn run_info(args: &InfoArgs, keys: Option<&GtaKeys>, verbose: bool) -> Result<()> {
    let data = load_resource_bytes(&args.file, args.archive.as_deref(), keys)?;
    let container = detect(&data).with_context(|| format!("'{}'", args.file))?;
    let contents = parse_contents(&args.file, &data, &container)?;
    let local = local_path(&args.file, args.archive.as_deref());
    let names = crate::names::load(&args.names, local.as_deref())?;
    let checks = local.as_deref().map(|p| Checks::of(p, &contents, &args.names)).unwrap_or_default();

    let mut out = String::new();
    if args.json {
        write_json(&mut out, args, &container, &contents, &names, &checks, verbose);
    } else {
        write_text(&mut out, args, &container, &contents, &names, &checks, verbose);
        if let Some(note) = unresolved_note(&out) {
            out.push_str(&note);
        }
    }
    print!("{out}");
    Ok(())
}

/// A closing line when the text left `hash_XXXXXXXX` placeholders behind:
/// how many, what the active list covers, and that an absent name may be
/// newer than the list rather than missing from the game.
pub fn unresolved_note(text: &str) -> Option<String> {
    let count = text.match_indices("hash_").filter(|(i, _)| text[i + 5..].chars().take(8).all(|c| c.is_ascii_hexdigit()) && text[i + 5..].len() >= 8).count();
    if count == 0 {
        return None;
    }
    let list = match crate::names::harvested_header() {
        Some(h) => format!("the name list {}", h.coverage()),
        None => "there is no name list yet (`rage names fetch` needs no game install; `rage names harvest --exe` scans one)".to_owned(),
    };
    Some(format!(
        "Unresolved: {count} hash(es) with no known name; {list}. A name absent from a list older than the file is not proof the asset does not exist.\n"
    ))
}

/// What a loose file on disk says about itself versus its surroundings.
/// Neither check applies to an entry read out of an archive with
/// `--archive`, where the lookup name and the file name are one thing.
#[derive(Debug, Default, PartialEq)]
pub struct Checks {
    /// The `.ymap`'s internal `CMapData.name` is not the file's stem, so
    /// parent links and manifest `imapName` entries that use the internal
    /// name will never bind to the map the game registers under the file
    /// name. Holds the stem the game will use.
    pub map_name_mismatch: Option<String>,
    /// Manifest `imapName` entries with no `.ymap` of that name next to the
    /// manifest: either vanilla maps (the common case for a resource that
    /// edits base-game maps) or leftovers from a map since renamed or
    /// removed. Each is paired with whether any known name list (built-in,
    /// harvested, `--names`; not the siblings) has the name — when none
    /// does, it is most likely a leftover.
    pub orphan_imaps: Vec<(String, bool)>,
}

impl Checks {
    pub fn of(path: &Path, contents: &Contents, extra: &[PathBuf]) -> Self {
        let mut checks = Self::default();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        match contents {
            Contents::Map(ymap) => {
                let h = ymap.header.name_hash;
                if !stem.is_empty() && h != rage_joaat(stem) && h != rage_joaat(&stem.to_lowercase()) {
                    checks.map_name_mismatch = Some(stem.to_owned());
                }
            }
            Contents::Manifest(m) => {
                let sibling_maps: std::collections::HashSet<u32> = path
                    .parent()
                    .and_then(|dir| crate::utils::walkdir(dir).ok())
                    .unwrap_or_default()
                    .iter()
                    .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ymap")))
                    .filter_map(|p| p.file_stem().and_then(|s| s.to_str()))
                    .flat_map(|s| [rage_joaat(s), rage_joaat(&s.to_lowercase())])
                    .collect();
                let known = crate::names::load(extra, None).unwrap_or_else(|_| NameTable::core());
                for dep in &m.imap_dependencies_2 {
                    let hash = match &dep.name.name {
                        Some(n) => rage_joaat(n),
                        None => dep.name.hash,
                    };
                    if sibling_maps.contains(&hash) {
                        continue;
                    }
                    let label = dep.name.name.clone().unwrap_or_else(|| known.resolve(hash).into_owned());
                    checks.orphan_imaps.push((label, known.get(hash).is_some()));
                }
            }
            _ => {}
        }
        checks
    }

    /// The manifest entries that no list names at all.
    fn unknown_imaps(&self) -> Vec<&str> {
        self.orphan_imaps.iter().filter(|(_, known)| !known).map(|(n, _)| n.as_str()).collect()
    }
}

fn run_dump(args: &DumpArgs, keys: Option<&GtaKeys>) -> Result<()> {
    let data = load_resource_bytes(&args.file, args.archive.as_deref(), keys)?;
    let container = detect(&data).with_context(|| format!("'{}'", args.file))?;
    let dump = match container {
        Container::Rsc7(_) => dump_meta(&data).with_context(|| {
            format!("'{}' is an RSC7 resource but not a Meta file (only .ymap/.ytyp/.ymt carry a schema to dump)", args.file)
        })?,
        Container::Meta(_) => dump_metadata(&data)?.1,
    };
    for warning in &dump.warnings {
        eprintln!("warning: {warning}");
    }
    let names = crate::names::load(&args.names, local_path(&args.file, args.archive.as_deref()).as_deref())?;
    let text = if args.json { to_json(&dump.root, &names).pretty(2) + "\n" } else { to_xml(&dump.root, &names) };
    match &args.output {
        Some(path) => {
            std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
            eprintln!("Wrote {}", path.display());
        }
        None => print!("{text}"),
    }
    if let Some(note) = unresolved_note(&text) {
        eprint!("{note}");
    }
    Ok(())
}

// ─── shared ──────────────────────────────────────────────────────────────────

/// The heading (yaw about z, degrees) an entity's stored rotation gives it.
/// Map entities store the inverse quaternion, so the conjugate is measured.
pub fn entity_yaw_degrees(e: &YmapEntity) -> f32 {
    let [x, y, z, w] = e.rotation;
    let (x, y, z) = (-x, -y, -z);
    (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z)).to_degrees()
}

/// The names of a `CEntityDef.flags` word's set bits, as CodeWalker labels them.
fn entity_flag_names(flags: u32) -> Vec<&'static str> {
    const NAMES: [&str; 32] = [
        "allow full rotation", "stream low priority", "disable embedded collision", "lod in parent map", "lod adopt me",
        "static entity", "interior lod", "lod use alt fade", "under water", "doesn't touch water", "doesn't spawn peds",
        "cast static shadows", "cast dynamic shadows", "ignore time in child rendering", "don't render in shadows",
        "only render in shadows", "don't render in reflections", "only render in reflections", "don't render in water reflections",
        "only render in water reflections", "don't render in mirror reflections", "only render in mirror reflections",
        "don't render in stream", "streaming ignore", "don't cast shadows", "unk25", "unk26", "unk27", "unk28", "unk29", "unk30", "unk31",
    ];
    NAMES.iter().enumerate().filter(|(i, _)| flags & (1 << i) != 0).map(|(_, n)| *n).collect()
}

fn header_flag_names(flags: u32) -> Vec<&'static str> {
    let mut names = Vec::new();
    if flags & YmapHeader::FLAG_SCRIPTED != 0 {
        names.push("SCRIPTED");
    }
    if flags & YmapHeader::FLAG_LOD != 0 {
        names.push("LOD");
    }
    names
}

fn fmt_vec3(v: Vec3) -> String {
    format!("({:.2}, {:.2}, {:.2})", v.x, v.y, v.z)
}

/// An empty map stores its boxes inverted (min at +MAX, max at -MAX); such
/// a box is not worth printing.
fn is_finite_box(lo: Vec3, hi: Vec3) -> bool {
    [lo.x, lo.y, lo.z, hi.x, hi.y, hi.z].iter().all(|v| v.is_finite() && v.abs() < 1.0e9) && lo.x <= hi.x && lo.y <= hi.y && lo.z <= hi.z
}

// ─── text ────────────────────────────────────────────────────────────────────

fn write_text(out: &mut String, args: &InfoArgs, container: &Container, contents: &Contents, names: &NameTable, checks: &Checks, verbose: bool) {
    use std::fmt::Write;

    match &args.archive {
        Some(archive) => writeln!(out, "File:      {} : {}", archive.display(), args.file).unwrap(),
        None => writeln!(out, "File:      {}", args.file).unwrap(),
    }
    match container {
        Container::Rsc7(h) => {
            writeln!(out, "Format:    RSC7  version {}", h.version).unwrap();
            let from_flags = resource_version_from_flags(h.system_flags, h.graphics_flags);
            if from_flags != h.version {
                writeln!(out, "           (warning: flags encode version {from_flags})").unwrap();
            }
            writeln!(out, "System:    0x{:08X} flags -> {} bytes", h.system_flags, h.system_size).unwrap();
            writeln!(out, "Graphics:  0x{:08X} flags -> {} bytes", h.graphics_flags, h.graphics_size).unwrap();
            writeln!(out, "Body:      {} bytes ({})", h.body_len, if h.compressed { "deflated" } else { "stored" }).unwrap();
        }
        Container::Meta(kind) => writeln!(out, "Format:    {kind}").unwrap(),
    }

    match contents {
        Contents::Other => writeln!(out, "Summary:   not a drawable, texture dictionary, map, type file or manifest").unwrap(),
        Contents::Textures(textures) => {
            writeln!(out, "Textures:  {}", textures.len()).unwrap();
            for tex in textures {
                write_texture_line(out, tex);
            }
        }
        Contents::Drawables(entries) => {
            writeln!(out, "Drawables: {}", entries.len()).unwrap();
            for entry in entries {
                write_drawable(out, entry, verbose);
            }
        }
        Contents::Map(ymap) => write_map(out, ymap, names, checks, args.limit),
        Contents::Types(ytyp) => write_types(out, ytyp, names, args.limit),
        Contents::Manifest(manifest) => write_manifest(out, manifest, names, checks),
        Contents::Meta(dump) => {
            let root = dump.root.as_struct().map(|s| names.resolve(s.type_hash).into_owned()).unwrap_or_else(|| "?".into());
            let fields = dump.root.as_struct().map_or(0, |s| s.fields.len());
            writeln!(out, "Summary:   {root} with {fields} members; `rage resource dump` prints it in full").unwrap();
            for w in &dump.warnings {
                writeln!(out, "           warning: {w}").unwrap();
            }
        }
    }
}

fn write_map(out: &mut String, ymap: &Ymap, names: &NameTable, checks: &Checks, limit: usize) {
    use std::fmt::Write;
    let h = &ymap.header;
    let parent = if h.parent_hash == 0 { "-".to_string() } else { names.resolve(h.parent_hash).into_owned() };
    writeln!(out, "Map:       {}  parent {parent}", names.resolve(h.name_hash)).unwrap();
    if let Some(stem) = &checks.map_name_mismatch {
        let internal = names.resolve(h.name_hash);
        writeln!(out, "           warning: the file is called {stem} but the map calls itself {internal} (0x{:08X}); the game registers it as {stem},", h.name_hash).unwrap();
        writeln!(out, "           so parent links and _manifest.ymf imapName entries that say {internal} will not bind (renamed outside CodeWalker?)").unwrap();
    }
    writeln!(out, "Flags:     0x{:X} {}  content 0x{:X} {}", h.flags, header_flag_names(h.flags).join("|"), h.content_flags, h.content_flag_names().join("|")).unwrap();
    if is_finite_box(h.streaming_extents_min, h.streaming_extents_max) {
        writeln!(out, "Streaming: {}..{}", fmt_vec3(h.streaming_extents_min), fmt_vec3(h.streaming_extents_max)).unwrap();
    }
    if is_finite_box(h.entities_extents_min, h.entities_extents_max) && !ymap.entities.is_empty() {
        writeln!(out, "Extents:   {}..{}", fmt_vec3(h.entities_extents_min), fmt_vec3(h.entities_extents_max)).unwrap();
    }
    writeln!(out, "Entities:  {} ({} MLO instances)", ymap.entities.len(), ymap.mlo_instances.len()).unwrap();
    if ymap.entities.is_empty() {
        return;
    }
    let shown = if limit == 0 { ymap.entities.len() } else { limit.min(ymap.entities.len()) };
    writeln!(out, "  {:>4}  {:<32} {:>10} {:>10} {:>8} {:>7} {:>5} {:>6}  flags", "#", "archetype", "x", "y", "z", "yaw", "scale", "lod").unwrap();
    for (i, e) in ymap.entities.iter().take(shown).enumerate() {
        let name = names.resolve(e.archetype_hash);
        let kind = if e.is_mlo_instance { " (MLO)" } else { "" };
        writeln!(
            out,
            "  {i:>4}  {:<32} {:>10.3} {:>10.3} {:>8.3} {:>6.1}° {:>5.2} {:>6.0}  0x{:X}{kind}",
            format!("{name}"), e.position.x, e.position.y, e.position.z, entity_yaw_degrees(e), e.scale_xy, e.lod_dist, e.flags
        )
        .unwrap();
    }
    if shown < ymap.entities.len() {
        writeln!(out, "  ... {} more (--limit 0 for all)", ymap.entities.len() - shown).unwrap();
    }
    let mut set = std::collections::BTreeSet::new();
    for e in &ymap.entities {
        for name in entity_flag_names(e.flags) {
            set.insert(name);
        }
    }
    if !set.is_empty() {
        writeln!(out, "Entity flags seen: {}", set.into_iter().collect::<Vec<_>>().join(", ")).unwrap();
    }
}

fn write_types(out: &mut String, ytyp: &Ytyp, names: &NameTable, limit: usize) {
    use std::fmt::Write;
    writeln!(out, "Archetypes: {} ({} MLO)", ytyp.archetypes.len(), ytyp.mlos.len()).unwrap();
    let shown = if limit == 0 { ytyp.archetypes.len() } else { limit.min(ytyp.archetypes.len()) };
    writeln!(out, "  {:<32} {:<24} {:>7}  bounds", "name", "texture dictionary", "lod").unwrap();
    for a in ytyp.archetypes.iter().take(shown) {
        let txd = if a.texture_dict_hash == 0 { "-".to_string() } else { names.resolve(a.texture_dict_hash).into_owned() };
        writeln!(out, "  {:<32} {:<24} {:>7.0}  {}..{}{}", names.resolve(a.name_hash), txd, a.lod_dist, fmt_vec3(a.bb_min), fmt_vec3(a.bb_max), if a.is_mlo { " (MLO)" } else { "" }).unwrap();
    }
    if shown < ytyp.archetypes.len() {
        writeln!(out, "  ... {} more (--limit 0 for all)", ytyp.archetypes.len() - shown).unwrap();
    }
    for mlo in &ytyp.mlos {
        writeln!(
            out,
            "MLO {}: {} rooms, {} portals, {} entities, {} entity sets",
            names.resolve(mlo.name_hash), mlo.rooms.len(), mlo.portals.len(), mlo.entities.len(), mlo.entity_sets.len()
        )
        .unwrap();
        for (i, room) in mlo.rooms.iter().enumerate() {
            writeln!(out, "  room {i:>2} {:<24} {} props", room.name, room.attached_objects.len()).unwrap();
        }
    }
}

fn write_manifest(out: &mut String, m: &Manifest, names: &NameTable, checks: &Checks) {
    use std::fmt::Write;
    let name = |h: &rage_formats::HashName| match &h.name {
        Some(n) => n.clone(),
        None => names.resolve(h.hash).into_owned(),
    };
    let list = |v: &[rage_formats::HashName]| v.iter().map(name).collect::<Vec<_>>().join(", ");
    if m.is_empty() {
        writeln!(out, "Manifest:  empty").unwrap();
        return;
    }
    if !m.imap_dependencies_2.is_empty() {
        writeln!(out, "Map dependencies ({}):", m.imap_dependencies_2.len()).unwrap();
        for d in &m.imap_dependencies_2 {
            let flags = if d.manifest_flags & 1 != 0 { " [INTERIOR_DATA]" } else { "" };
            let n = name(&d.name);
            let note = match checks.orphan_imaps.iter().find(|(o, _)| *o == n) {
                Some((_, true)) => "  (no such .ymap here; a vanilla map?)",
                Some((_, false)) => "  (no such .ymap here, and no list knows the name)",
                None => "",
            };
            writeln!(out, "  {n}{flags} -> {}{note}", if d.ityp_deps.is_empty() { "-".to_string() } else { list(&d.ityp_deps) }).unwrap();
        }
        let unknown = checks.unknown_imaps();
        if !unknown.is_empty() {
            writeln!(
                out,
                "           warning: {} declared map(s) exist neither next to this manifest nor in any name list ({}); a leftover from a map since renamed or removed?",
                unknown.len(),
                unknown.join(", ")
            )
            .unwrap();
        }
    }
    if !m.ityp_dependencies_2.is_empty() {
        writeln!(out, "Type dependencies ({}):", m.ityp_dependencies_2.len()).unwrap();
        for d in &m.ityp_dependencies_2 {
            let flags = if d.manifest_flags & 1 != 0 { " [INTERIOR_DATA]" } else { "" };
            writeln!(out, "  {}{flags} -> {}", name(&d.name), if d.ityp_deps.is_empty() { "-".to_string() } else { list(&d.ityp_deps) }).unwrap();
        }
    }
    if !m.imap_dependencies.is_empty() {
        writeln!(out, "Map dependencies, old form ({}):", m.imap_dependencies.len()).unwrap();
        for d in &m.imap_dependencies {
            writeln!(out, "  {} -> {} (pack {})", name(&d.imap), name(&d.ityp), name(&d.pack_file)).unwrap();
        }
    }
    if !m.interiors.is_empty() {
        writeln!(out, "Interiors ({}):", m.interiors.len()).unwrap();
        for i in &m.interiors {
            writeln!(out, "  {} -> {}", name(&i.name), list(&i.bounds)).unwrap();
        }
    }
    if !m.hd_txd_bindings.is_empty() {
        writeln!(out, "HD texture bindings ({}):", m.hd_txd_bindings.len()).unwrap();
        for b in &m.hd_txd_bindings {
            writeln!(out, "  {} {} -> {}", name(&b.asset_type), b.target_asset, b.hd_txd).unwrap();
        }
    }
    if !m.map_data_groups.is_empty() {
        writeln!(out, "Map data groups ({}):", m.map_data_groups.len()).unwrap();
        for g in &m.map_data_groups {
            writeln!(out, "  {} flags 0x{:X} hours 0x{:X} bounds [{}] weather [{}]", name(&g.name), g.flags, g.hours_on_off, list(&g.bounds), list(&g.weather_types)).unwrap();
        }
    }
}

/// Same line format as `rpf textures`, so the two commands agree.
fn write_texture_line(out: &mut String, tex: &YtdTexture) {
    use std::fmt::Write;
    let name = if tex.name.is_empty() { format!("0x{:08X}", tex.name_hash) } else { tex.name.clone() };
    writeln!(
        out, "  {} — {}x{}x{} {} {} mip(s) ({} bytes)",
        name, tex.width, tex.height, tex.depth, tex.format, tex.levels, tex.pixel_data.len(),
    ).unwrap();
}

fn write_drawable(out: &mut String, entry: &DrawableEntry, verbose: bool) {
    use std::fmt::Write;
    let d = &entry.drawable;

    writeln!(out).unwrap();
    writeln!(out, "  {} (0x{:08X})", if d.name.is_empty() { &entry.name } else { &d.name }, entry.hash).unwrap();

    let (bounds, computed) = match d.best_lod() {
        Some(lod) => d.bounds_or_computed(lod),
        None => (d.bounds.clone(), false),
    };
    let c = bounds.center;
    writeln!(out, "    bounds:   center ({:.3}, {:.3}, {:.3}) radius {:.3}{}",
             c.x, c.y, c.z, bounds.sphere_radius, if computed { " (computed)" } else { "" }).unwrap();
    writeln!(out, "              min ({:.3}, {:.3}, {:.3}) max ({:.3}, {:.3}, {:.3})",
             bounds.box_min.x, bounds.box_min.y, bounds.box_min.z,
             bounds.box_max.x, bounds.box_max.y, bounds.box_max.z).unwrap();
    writeln!(out, "    lod dist: {:.1} / {:.1} / {:.1} / {:.1}",
             d.lod_distances[0], d.lod_distances[1], d.lod_distances[2], d.lod_distances[3]).unwrap();

    if verbose {
        if let Some(lod) = d.best_lod() {
            let geoms = d.geometry_bounds(lod);
            if !geoms.is_empty() {
                writeln!(out, "    geometry bounds:").unwrap();
                for g in &geoms {
                    writeln!(out, "      #{} model {} shader {}   {} tris, {} verts",
                             g.geometry, g.model, g.shader_id, g.triangles, g.vertices).unwrap();
                    writeln!(out, "           min ({:.3}, {:.3}, {:.3}) max ({:.3}, {:.3}, {:.3}) centroid ({:.3}, {:.3}, {:.3})",
                             g.min.x, g.min.y, g.min.z, g.max.x, g.max.y, g.max.z,
                             g.centroid.x, g.centroid.y, g.centroid.z).unwrap();
                }
            }
        }
    }

    for lod in &d.lods {
        let geometries: usize = lod.models.iter().map(|m| m.geometries.len()).sum();
        writeln!(out, "    {:<8} {} models, {} geometries, {} triangles",
                 format!("{}:", lod.level), lod.models.len(), geometries, d.triangle_count(lod)).unwrap();
    }

    if let Some(group) = &d.shader_group {
        writeln!(out, "    shaders:  {}", group.shaders.len()).unwrap();
        for (id, shader) in group.shaders.iter().enumerate() {
            let diffuse = d.diffuse_texture_name(id as u16).unwrap_or("-");
            writeln!(out, "      #{:<3} name 0x{:08X}  file 0x{:08X}  bucket {}  diffuse {}",
                     id, shader.name_hash, shader.file_name_hash, shader.render_bucket, diffuse).unwrap();
        }
        writeln!(out, "    embedded textures: {}", group.textures.len()).unwrap();
        for tex in &group.textures {
            out.push_str("  ");
            write_texture_line(out, tex);
        }
    } else {
        writeln!(out, "    shaders:  none").unwrap();
    }
}

// ─── json ────────────────────────────────────────────────────────────────────

fn write_json(out: &mut String, args: &InfoArgs, container: &Container, contents: &Contents, names: &NameTable, checks: &Checks, verbose: bool) {
    use std::fmt::Write;

    let (kind, body) = match contents {
        Contents::Other => ("other", String::new()),
        Contents::Textures(textures) => ("textures", format!(",\"textures\":{}", json_textures(textures))),
        Contents::Drawables(entries) => {
            let items: Vec<String> = entries.iter().map(|e| json_drawable(e, verbose)).collect();
            ("drawables", format!(",\"drawables\":[{}]", items.join(",")))
        }
        Contents::Map(ymap) => ("map", format!(",\"map\":{}", json_map(ymap, names, checks).dump())),
        Contents::Types(ytyp) => ("types", format!(",\"types\":{}", json_types(ytyp, names).dump())),
        Contents::Manifest(m) => ("manifest", format!(",\"manifest\":{}", json_manifest(m, names, checks).dump())),
        Contents::Meta(dump) => ("meta", format!(",\"meta\":{}", to_json(&dump.root, names).dump())),
    };

    let header = match container {
        Container::Rsc7(h) => format!(
            "\"format\":\"RSC7\",\"version\":{},\"system_flags\":\"0x{:08X}\",\"system_size\":{},\"graphics_flags\":\"0x{:08X}\",\"graphics_size\":{},\"body_bytes\":{},\"compressed\":{}",
            h.version, h.system_flags, h.system_size, h.graphics_flags, h.graphics_size, h.body_len, h.compressed
        ),
        Container::Meta(kind) => format!("\"format\":{}", json_string(&kind.to_string())),
    };
    write!(
        out,
        "{{\"file\":{},\"archive\":{},{header},\"kind\":\"{kind}\"{body}}}\n",
        json_string(&args.file),
        args.archive.as_ref().map_or("null".to_string(), |a| json_string(&a.to_string_lossy())),
    )
    .unwrap();
}

fn json_vec(v: Vec3) -> json::JsonValue {
    json::array![v.x, v.y, v.z]
}

fn json_name(hash: u32, names: &NameTable) -> json::JsonValue {
    if hash == 0 {
        json::JsonValue::Null
    } else {
        json::JsonValue::from(names.resolve(hash).as_ref())
    }
}

fn json_map(ymap: &Ymap, names: &NameTable, checks: &Checks) -> json::JsonValue {
    let h = &ymap.header;
    let entities: Vec<json::JsonValue> = ymap
        .entities
        .iter()
        .map(|e| {
            json::object! {
                archetype: json_name(e.archetype_hash, names),
                archetype_hash: format!("0x{:08X}", e.archetype_hash),
                position: json_vec(e.position),
                rotation: json::array![e.rotation[0], e.rotation[1], e.rotation[2], e.rotation[3]],
                yaw_degrees: entity_yaw_degrees(e),
                scale_xy: e.scale_xy,
                scale_z: e.scale_z,
                lod_dist: e.lod_dist,
                parent_index: e.parent_index,
                flags: e.flags,
                flag_names: entity_flag_names(e.flags),
                guid: e.guid,
                mlo_instance: e.is_mlo_instance,
            }
        })
        .collect();
    let instances: Vec<json::JsonValue> = ymap
        .mlo_instances
        .iter()
        .map(|i| {
            json::object! {
                archetype: json_name(i.entity.archetype_hash, names),
                position: json_vec(i.entity.position),
                group_id: i.group_id,
                floor_id: i.floor_id,
                default_entity_sets: i.default_entity_sets.iter().map(|h| json_name(*h, names)).collect::<Vec<_>>(),
                num_exit_portals: i.num_exit_portals,
            }
        })
        .collect();
    json::object! {
        name: json_name(h.name_hash, names),
        name_hash: format!("0x{:08X}", h.name_hash),
        file_name_mismatch: checks.map_name_mismatch.as_deref().map_or(json::JsonValue::Null, json::JsonValue::from),
        parent: json_name(h.parent_hash, names),
        flags: h.flags,
        flag_names: header_flag_names(h.flags),
        content_flags: h.content_flags,
        content_flag_names: h.content_flag_names(),
        streaming_extents: json::array![json_vec(h.streaming_extents_min), json_vec(h.streaming_extents_max)],
        entities_extents: json::array![json_vec(h.entities_extents_min), json_vec(h.entities_extents_max)],
        entities: entities,
        mlo_instances: instances,
    }
}

fn json_types(ytyp: &Ytyp, names: &NameTable) -> json::JsonValue {
    let archetypes: Vec<json::JsonValue> = ytyp
        .archetypes
        .iter()
        .map(|a| {
            json::object! {
                name: json_name(a.name_hash, names),
                name_hash: format!("0x{:08X}", a.name_hash),
                texture_dictionary: json_name(a.texture_dict_hash, names),
                lod_dist: a.lod_dist,
                bb_min: json_vec(a.bb_min),
                bb_max: json_vec(a.bb_max),
                mlo: a.is_mlo,
            }
        })
        .collect();
    let mlos: Vec<json::JsonValue> = ytyp
        .mlos
        .iter()
        .map(|m| {
            json::object! {
                name: json_name(m.name_hash, names),
                rooms: m.rooms.iter().map(|r| json::object! { name: r.name.as_str(), props: r.attached_objects.len(), bb_min: json_vec(r.bb_min), bb_max: json_vec(r.bb_max) }).collect::<Vec<_>>(),
                portals: m.portals.len(),
                entities: m.entities.len(),
                entity_sets: m.entity_sets.iter().map(|s| json_name(s.name_hash, names)).collect::<Vec<_>>(),
            }
        })
        .collect();
    json::object! { archetypes: archetypes, mlos: mlos }
}

fn json_manifest(m: &Manifest, names: &NameTable, checks: &Checks) -> json::JsonValue {
    let name = |h: &rage_formats::HashName| match &h.name {
        Some(n) => json::JsonValue::from(n.as_str()),
        None => json_name(h.hash, names),
    };
    let list = |v: &[rage_formats::HashName]| v.iter().map(name).collect::<Vec<_>>();
    let deps = |v: &[rage_formats::Dependencies]| {
        v.iter().map(|d| json::object! { name: name(&d.name), manifest_flags: d.manifest_flags, ityp_deps: list(&d.ityp_deps) }).collect::<Vec<_>>()
    };
    let orphan = |(n, known): &(String, bool)| json::object! { name: n.as_str(), known_name: *known };
    json::object! {
        map_data_groups: m.map_data_groups.iter().map(|g| json::object! { name: name(&g.name), flags: g.flags, hours_on_off: g.hours_on_off, bounds: list(&g.bounds), weather_types: list(&g.weather_types) }).collect::<Vec<_>>(),
        imap_dependencies: m.imap_dependencies.iter().map(|d| json::object! { imap: name(&d.imap), ityp: name(&d.ityp), pack_file: name(&d.pack_file) }).collect::<Vec<_>>(),
        imap_dependencies_2: deps(&m.imap_dependencies_2),
        imaps_not_here: checks.orphan_imaps.iter().map(orphan).collect::<Vec<_>>(),
        ityp_dependencies_2: deps(&m.ityp_dependencies_2),
        hd_txd_bindings: m.hd_txd_bindings.iter().map(|b| json::object! { asset_type: name(&b.asset_type), target_asset: b.target_asset.as_str(), hd_txd: b.hd_txd.as_str() }).collect::<Vec<_>>(),
        interiors: m.interiors.iter().map(|i| json::object! { name: name(&i.name), bounds: list(&i.bounds) }).collect::<Vec<_>>(),
    }
}

fn json_textures(textures: &[YtdTexture]) -> String {
    let items: Vec<String> = textures.iter().map(|tex| format!(
        "{{\"name\":{},\"hash\":\"0x{:08X}\",\"width\":{},\"height\":{},\"depth\":{},\"format\":\"{}\",\"mips\":{},\"bytes\":{}}}",
        json_string(&tex.name), tex.name_hash, tex.width, tex.height, tex.depth,
        tex.format, tex.levels, tex.pixel_data.len(),
    )).collect();
    format!("[{}]", items.join(","))
}

fn json_vec3(v: &rage_formats::Vec3) -> String {
    format!("[{},{},{}]", v.x, v.y, v.z)
}

fn json_geometry_bounds(geoms: &[rage_formats::GeometryBounds]) -> String {
    let items: Vec<String> = geoms.iter().map(|g| format!(
        "{{\"model\":{},\"geometry\":{},\"shader\":{},\"vertices\":{},\"triangles\":{},\"min\":{},\"max\":{},\"centroid\":{}}}",
        g.model, g.geometry, g.shader_id, g.vertices, g.triangles,
        json_vec3(&g.min), json_vec3(&g.max), json_vec3(&g.centroid),
    )).collect();
    format!("[{}]", items.join(","))
}

fn json_drawable(entry: &DrawableEntry, verbose: bool) -> String {
    let d = &entry.drawable;
    let (bounds, computed) = match d.best_lod() {
        Some(lod) => d.bounds_or_computed(lod),
        None => (d.bounds.clone(), false),
    };

    let geometry_bounds = if verbose {
        d.best_lod().map(|lod| format!(",\"geometry_bounds\":{}", json_geometry_bounds(&d.geometry_bounds(lod))))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let lods: Vec<String> = d.lods.iter().map(|lod| {
        let geometries: usize = lod.models.iter().map(|m| m.geometries.len()).sum();
        format!("{{\"level\":\"{}\",\"models\":{},\"geometries\":{},\"triangles\":{}}}",
                lod.level, lod.models.len(), geometries, d.triangle_count(lod))
    }).collect();

    let (shaders, textures) = match &d.shader_group {
        Some(group) => {
            let shaders: Vec<String> = group.shaders.iter().enumerate().map(|(id, s)| format!(
                "{{\"id\":{},\"name_hash\":\"0x{:08X}\",\"file_name_hash\":\"0x{:08X}\",\"render_bucket\":{},\"diffuse\":{}}}",
                id, s.name_hash, s.file_name_hash, s.render_bucket,
                d.diffuse_texture_name(id as u16).map_or("null".to_string(), json_string),
            )).collect();
            (format!("[{}]", shaders.join(",")), json_textures(&group.textures))
        }
        None => ("[]".to_string(), "[]".to_string()),
    };

    format!(
        "{{\"name\":{},\"hash\":\"0x{:08X}\",\"bounds\":{{\"center\":{},\"radius\":{},\"min\":{},\"max\":{},\"computed\":{}}}{},\"lod_distances\":[{},{},{},{}],\"lods\":[{}],\"shaders\":{},\"textures\":{}}}",
        json_string(if d.name.is_empty() { &entry.name } else { &d.name }), entry.hash,
        json_vec3(&bounds.center), bounds.sphere_radius, json_vec3(&bounds.box_min), json_vec3(&bounds.box_max), computed,
        geometry_bounds,
        d.lod_distances[0], d.lod_distances[1], d.lod_distances[2], d.lod_distances[3],
        lods.join(","), shaders, textures,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(magic: &[u8; 4], version: u32, sys: u32, gfx: u32, body: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(magic);
        data.extend_from_slice(&version.to_le_bytes());
        data.extend_from_slice(&sys.to_le_bytes());
        data.extend_from_slice(&gfx.to_le_bytes());
        data.extend_from_slice(body);
        data
    }

    #[test]
    fn parses_a_stored_header() {
        let data = header(b"RSC7", 165, 0xA800_0004, 0x5000_0000, &[0u8; 8192]);
        let h = parse_header(&data).unwrap();
        assert_eq!(h, Rsc7Header {
            version: 165, system_flags: 0xA800_0004, graphics_flags: 0x5000_0000,
            system_size: 8192, graphics_size: 0, compressed: false, body_len: 8192,
        });
    }

    #[test]
    fn rejects_bad_magic() {
        let err = parse_header(&header(b"XXXX", 0, 0, 0, &[])).unwrap_err().to_string();
        assert!(err.contains("0x58585858"), "{err}");
        let err = detect(&header(b"XXXX", 0, 0, 0, &[])).unwrap_err().to_string();
        assert!(err.contains("0x58585858"), "{err}");
    }

    #[test]
    fn names_fivem_escrow_files() {
        let err = parse_header(&header(b"FXAP", 0, 0, 0, &[0u8; 32])).unwrap_err().to_string();
        assert!(err.contains("FiveM escrow"), "{err}");
    }

    #[test]
    fn rejects_rsc8() {
        let err = parse_header(&header(b"RSC8", 0, 0, 0, &[])).unwrap_err().to_string();
        assert!(err.contains("RSC8"), "{err}");
    }

    #[test]
    fn rejects_short_input() {
        assert!(parse_header(b"RSC7").is_err());
        assert!(detect(b"RS").is_err());
    }

    #[test]
    fn detects_the_metadata_containers() {
        assert_eq!(detect(b"PSIN\0\0\0\x10\0\0\0\0").unwrap(), Container::Meta(MetaContainer::Pso));
        assert_eq!(detect(b"RBF0\0\0\0\0").unwrap(), Container::Meta(MetaContainer::Rbf));
        assert_eq!(detect(b"<?xml version=\"1.0\"?><CPackFileMetaData/>").unwrap(), Container::Meta(MetaContainer::Xml));
        assert!(matches!(detect(&header(b"RSC7", 2, 0x0800_0004, 0, &[0u8; 8192])).unwrap(), Container::Rsc7(_)));
    }

    #[test]
    fn yaw_is_measured_from_the_conjugate() {
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let e = |rotation: [f32; 4]| YmapEntity {
            archetype_hash: 0, flags: 0, guid: 0, position: Vec3::new(0.0, 0.0, 0.0), rotation,
            scale_xy: 1.0, scale_z: 1.0, parent_index: -1, lod_dist: 0.0, is_mlo_instance: false,
        };
        assert!((entity_yaw_degrees(&e([0.0, 0.0, 0.0, 1.0]))).abs() < 1e-4);
        // A stored +90° about z is a -90° heading in the world.
        assert!((entity_yaw_degrees(&e([0.0, 0.0, h, h])) + 90.0).abs() < 1e-3);
        assert!((entity_yaw_degrees(&e([0.0, 0.0, -0.17364818, 0.9848077])) - 20.0).abs() < 1e-3);
    }

    #[test]
    fn unresolved_note_counts_placeholders_only() {
        assert!(unresolved_note("Map: paleto_props\n").is_none());
        let note = unresolved_note("Map: hash_AEC13995\n  hash_4FD621BC x\n  hash_not_one\n").unwrap();
        assert!(note.starts_with("Unresolved: 2 hash(es)"), "{note}");
    }

    #[test]
    fn entity_flags_are_named_by_bit() {
        assert_eq!(entity_flag_names(0x20), ["static entity"]);
        assert_eq!(entity_flag_names(0), Vec::<&str>::new());
        assert_eq!(entity_flag_names(1 | 1 << 5), ["allow full rotation", "static entity"]);
    }
}
