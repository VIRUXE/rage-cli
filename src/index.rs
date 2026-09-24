// A cached, game-wide index of where things live in the game's archives, in
// three parts that are loaded, built and cached independently, so each
// command pays only for what it reads:
//
// - textures (`screenshot`): resolves a drawable's external texture
//   dictionary the way the game does, following CodeWalker's
//   `Renderer.TryGetRenderable` order. An archetype's `.ytyp` names a texture
//   dictionary by hash, that hash (and its parent chain, from `gtxd.meta`/
//   `gtxd.ymt`/`mph4_gtxd.ymt`/`vehicles.meta`) resolves to `.ytd`s by name,
//   and a texture still missing falls back to the two "resident"
//   dictionaries the game always keeps loaded (`mapdetail`, `vehshare`).
// - interiors (`plot <interior>`): which `.ytyp` declares an MLO archetype,
//   which `.ymap`s place it, and collision files by name.
// - models (`plot` props, `navmesh --game-props`): model files by name,
//   archetype bounding boxes and which model file an archetype draws from.
//
// Nobody has to build it by hand: `GameIndex::load` builds whatever part is
// missing, or stale because an archive changed, and caches it.

use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rage_formats::{parse_cache_dat, parse_txd_relationships, parse_ymap_entities, parse_ytd, parse_ytyp, rage_joaat, Vec3};
use rpf_archive::{parse_dlc_list, parse_dlc_setup_order};

use crate::commands::search::collect_archives;
use crate::keys;
use crate::rpf::{Archive, GtaKeys};

/// Where a file lives: a top-level `.rpf` on disk, then zero or more nested
/// `.rpf` entries to descend through, then the entry itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryLoc {
    pub top_archive: PathBuf,
    pub nested_rpfs: Vec<String>,
    pub inner_path: String,
}

/// Which parts of the index a command needs. Each part is cached in a file
/// of its own, loaded only when asked for and built only when it is missing
/// or stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Parts(u8);

impl Parts {
    pub const NONE: Parts = Parts(0);
    /// `ytd_by_name`, `archetype_txd`, `resident_textures`, `parent_txds`.
    pub const TEXTURES: Parts = Parts(1);
    /// `mlo_ytyp`, `mlo_instances`, `ybn_by_name`.
    pub const INTERIORS: Parts = Parts(2);
    /// `drawable_by_name`, `archetype_box`, `archetype_asset`.
    pub const MODELS: Parts = Parts(4);
    pub const ALL: Parts = Parts(7);

    pub fn contains(self, other: Parts) -> bool {
        other.0 != 0 && self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Each single part in `self`, in a fixed order.
    pub fn each(self) -> impl Iterator<Item = Parts> {
        [Parts::TEXTURES, Parts::INTERIORS, Parts::MODELS].into_iter().filter(move |p| self.contains(*p))
    }

    pub fn name(self) -> &'static str {
        match self {
            Parts::TEXTURES => "textures",
            Parts::INTERIORS => "interiors",
            Parts::MODELS => "models",
            _ => "index",
        }
    }

    /// The cache file a single part is stored in.
    pub fn file_name(self) -> String {
        format!("{}.bin", self.name())
    }

    /// `"textures, models"`.
    pub fn describe(self) -> String {
        self.each().map(Parts::name).collect::<Vec<_>>().join(", ")
    }
}

impl std::ops::BitOr for Parts {
    type Output = Parts;
    fn bitor(self, other: Parts) -> Parts {
        Parts(self.0 | other.0)
    }
}

impl std::ops::BitOrAssign for Parts {
    fn bitor_assign(&mut self, other: Parts) {
        self.0 |= other.0;
    }
}

/// The index, or the parts of it that were asked for: the maps of a part
/// not in `parts` are simply empty.
#[derive(Debug, Default)]
pub struct GameIndex {
    /// Which parts the maps below hold.
    pub parts: Parts,
    /// `.ytd` stem hash -> where that dictionary lives. Later archives win
    /// (last write), matching the game's own DLC-overrides-base ordering —
    /// which `GameIndex::build` provides by ranking `archives` base ->
    /// update -> DLC (in the game's own `dlclist.xml`/`setup2.xml` order)
    /// before indexing them, rather than relying on `collect_archives`'s
    /// alphabetical order.
    pub ytd_by_name: HashMap<u32, EntryLoc>,
    /// Archetype/model name hash -> its `textureDictionary` hash, from every
    /// `.ytyp`. Later `.ytyp`s win, same ranked scan order as `ytd_by_name`.
    pub archetype_txd: HashMap<u32, u32>,
    /// Archetype name hash -> its bounding box (`bbMin`, `bbMax`), from every
    /// .ytyp; what a placed prop occupies, for anything that reads placements.
    pub archetype_box: HashMap<u32, (Vec3, Vec3)>,
    /// Texture name hash -> the name hash of the resident dictionary
    /// (`mapdetail`/`vehshare`) that holds it.
    pub resident_textures: HashMap<u32, u32>,
    /// Child texture-dictionary hash -> parent texture-dictionary hash, from
    /// every `gtxd.meta`/`gtxd.ymt`/`mph4_gtxd.ymt`/`vehicles.meta`. First
    /// relationship for a given child wins, matching CodeWalker's own
    /// first-wins merge (see `merge_txd_relationships`).
    pub parent_txds: HashMap<u32, u32>,
    /// MLO archetype name hash -> the `.ytyp` that declares it. Later
    /// archives win (last write), the same rank order as the maps above.
    pub mlo_ytyp: HashMap<u32, EntryLoc>,
    /// MLO archetype name hash -> every `.ymap` that places a
    /// `CMloInstanceDef` of it. An interior is usually placed once, but
    /// repeated interiors (garages, apartments) are placed many times, so
    /// this keeps them all in scan order.
    pub mlo_instances: HashMap<u32, Vec<EntryLoc>>,
    /// `joaat(lowercase .ybn stem)` -> where that collision file lives. The
    /// collision of an interior usually shares the archetype's own name.
    pub ybn_by_name: HashMap<u32, EntryLoc>,
    /// `joaat(lowercase stem)` of every `.ydr`, `.ydd` and `.yft` -> where
    /// it lives; later archives win. What `plot` draws a placed prop from.
    pub drawable_by_name: HashMap<u32, EntryLoc>,
    /// Archetype name hash -> `(assetType, model hash)`: which file holds
    /// its model — a `.ydd` (asset type 3, the dictionary's hash) or a
    /// `.ydr`/`.yft` named by the model hash. Only kept when it differs
    /// from the archetype's own name, which is the common case's default.
    pub archetype_asset: HashMap<u32, (u32, u32)>,
}

const RESIDENT_DICTS: [&str; 2] = ["mapdetail", "vehshare"];

/// CodeWalker's `GameFileCache.InitGtxds` matches these names exactly
/// (`entry.NameLower == "..."`), not as a suffix — a filename like
/// `dlc_gtxd.ymt` is deliberately not picked up, matching the reference.
const TXD_RELATIONSHIP_FILES: [&str; 4] = ["gtxd.ymt", "gtxd.meta", "mph4_gtxd.ymt", "vehicles.meta"];

/// What one top-level archive contributes to a build: its share of the
/// index, plus what the interior pass needs once every archive is in.
#[derive(Default)]
struct Partial {
    index: GameIndex,
    /// Every `.ymap`, in scan order: `joaat(lowercase stem)` and where.
    ymaps: Vec<(u32, EntryLoc)>,
    /// Name hashes of the maps some `cache_y.dat` describes.
    covered: HashSet<u32>,
    /// `(interior archetype, placing map)` from the `cache_y.dat`s.
    proxies: Vec<(u32, u32)>,
}

impl GameIndex {
    /// Builds `parts` of the index by walking every `.rpf` under
    /// `game_root`, nested archives included. Archives are memory-mapped,
    /// so only their tables of contents and the entries decoded are read:
    /// every `.ytyp`, the texture-relationship files and the two resident
    /// dictionaries, the world cache files, and the few `.ymap`s the
    /// interior pass needs (see `interior_placements`). Top-level archives
    /// are indexed in parallel, one partial index each, merged afterwards in
    /// rank order so the override rules are exactly those of a serial scan.
    pub fn build(game_root: &Path, keys: Option<&GtaKeys>, parts: Parts) -> Result<Self> {
        use rayon::prelude::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let archives = ranked_archives(game_root, keys)?;

        let total = archives.len();
        let done = AtomicUsize::new(0);
        let partials: Vec<Partial> = archives
            .par_iter()
            .map(|archive_path| {
                let mut part = Partial::default();
                match Archive::open(archive_path, keys) {
                    Ok(archive) if archive.require_keys(keys).is_err() => {
                        eprintln!("\nindex: skipping {} (needs keys)", archive_path.display());
                    }
                    Ok(archive) => index_archive(&archive, archive_path, &[], keys, parts, &mut part),
                    Err(e) => eprintln!("\nindex: skipping {}: {}", archive_path.display(), e),
                }
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                progress(n, total, archive_path.strip_prefix(game_root).unwrap_or(archive_path));
                part
            })
            .collect();
        eprint!("\r{:<78}\r", "");

        // `collect` keeps `archives`' order, so this replays a serial scan.
        let mut index = GameIndex::default();
        let (mut ymaps, mut covered, mut proxies) = (Vec::new(), HashSet::new(), Vec::new());
        for part in partials {
            index.merge(part.index);
            ymaps.extend(part.ymaps);
            covered.extend(part.covered);
            proxies.extend(part.proxies);
        }
        if parts.contains(Parts::INTERIORS) {
            index.mlo_instances = interior_placements(&ymaps, &covered, &proxies, keys);
        }
        index.parts = parts;
        Ok(index)
    }

    /// Folds in the index of an archive that loads after everything already
    /// merged: last-wins maps take `later`'s values, `parent_txds` keeps
    /// the first relationship seen, and placement lists are appended.
    fn merge(&mut self, later: GameIndex) {
        self.parts |= later.parts;
        self.ytd_by_name.extend(later.ytd_by_name);
        self.archetype_txd.extend(later.archetype_txd);
        self.archetype_box.extend(later.archetype_box);
        self.resident_textures.extend(later.resident_textures);
        for (child, parent) in later.parent_txds {
            self.parent_txds.entry(child).or_insert(parent);
        }
        self.mlo_ytyp.extend(later.mlo_ytyp);
        for (archetype, locs) in later.mlo_instances {
            self.mlo_instances.entry(archetype).or_default().extend(locs);
        }
        self.ybn_by_name.extend(later.ybn_by_name);
        self.drawable_by_name.extend(later.drawable_by_name);
        self.archetype_asset.extend(later.archetype_asset);
    }

    /// Reads the raw bytes of an already-located entry, descending through
    /// any nested archives on the way.
    pub fn load_bytes(&self, loc: &EntryLoc, keys: Option<&GtaKeys>) -> Result<Vec<u8>> {
        let top = Archive::open(&loc.top_archive, keys)?;
        top.require_keys(keys)?;
        let nested;
        let archive = if loc.nested_rpfs.is_empty() {
            &top
        } else {
            nested = open_chain(&top, &loc.nested_rpfs, keys)
                .with_context(|| format!("in '{}'", loc.top_archive.display()))?;
            &nested
        };
        let file = archive
            .find_file(&loc.inner_path)
            .with_context(|| format!("'{}' not found", loc.inner_path))?;
        archive.extract(file, keys).with_context(|| format!("failed to extract '{}'", loc.inner_path))
    }

    /// Every `.ytd` layer `screenshot` should try, in CodeWalker's order,
    /// for a drawable named `file_stem` and (if known) its archetype hash:
    /// the archetype's own texture dictionary and its full parent chain,
    /// then the drawable's own stem hash and *its* parent chain as the
    /// same-name guess this has always made. Names, not yet loaded — the
    /// caller loads and parses only the ones it still needs.
    ///
    /// Each candidate's parent chain is walked in full before the next
    /// candidate starts (CodeWalker's `Renderer.cs TryGetRenderable` builds
    /// exactly this array — `[own txd, parent, grandparent, ...]` — for the
    /// archetype's resolved dictionary). A single shared `seen` set is what
    /// keeps this a cycle guard rather than a repeat of the same chain:
    /// CodeWalker has no such guard anywhere in this walk.
    pub fn resolution_order(&self, file_stem_hash: u32) -> Vec<u32> {
        const MAX_HOPS: usize = 64;

        // The archetype hash is usually just the drawable's own file stem
        // (CodeWalker's `ModelForm.cs` fallback for a model with no known
        // archetype): try that hash's texture dictionary directly, and also
        // check whether an archetype named it explicitly.
        let mut starts = Vec::with_capacity(2);
        if let Some(&txd_hash) = self.archetype_txd.get(&file_stem_hash)
            && txd_hash != 0
        {
            starts.push(txd_hash);
        }
        starts.push(file_stem_hash);

        let mut seen = std::collections::HashSet::new();
        let mut order = Vec::new();

        for start in starts {
            let mut current = start;
            for _ in 0..=MAX_HOPS {
                if !seen.insert(current) {
                    break; // cycle, or already covered by an earlier chain
                }
                order.push(current);
                let Some(&parent) = self.parent_txds.get(&current) else { break };
                current = parent;
            }
        }

        order
    }

    /// Looks up which resident dictionary (`mapdetail`/`vehshare`) carries a
    /// texture named `texture_name_hash`, if either does. Used by
    /// `screenshot`'s resident-dictionary fallback once rendering has
    /// reported a texture name still missing after the whole chain.
    pub fn resident_dict_for_texture(&self, texture_name_hash: u32) -> Option<u32> {
        self.resident_textures.get(&texture_name_hash).copied()
    }

    // ─── On-disk cache ──────────────────────────────────────────────────

    /// `~/.rage-cli/index/<game build>/`, keyed the same way as the key
    /// cache (`keys::cache_entry_name`); one file per part inside it.
    pub fn cache_dir(exe_path: &Path) -> Option<PathBuf> {
        let root = crate::paths::config_root()?.join("index");
        let meta = std::fs::metadata(exe_path).ok()?;
        let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        Some(root.join(keys::cache_entry_name(meta.len(), modified)))
    }

    /// Reads one part's cache file. `Ok(Err(Stale))` when it was written for
    /// different archives than `fingerprint` describes.
    pub fn load_part(path: &Path, part: Parts, fingerprint: u64) -> Result<std::result::Result<Self, Stale>> {
        let mut data = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut data)?;
        let (written_for, index) = decode_part(&data, part)?;
        Ok(if written_for == fingerprint { Ok(index) } else { Err(Stale) })
    }

    /// Writes every part this index holds into `dir`, one file each.
    pub fn save(&self, dir: &Path, fingerprint: u64) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        for part in self.parts.each() {
            let path = dir.join(part.file_name());
            // Written aside and renamed into place, so a second `rage`
            // running at the same time never reads a half-written file.
            let tmp = dir.join(format!("{}.{}.tmp", part.file_name(), std::process::id()));
            std::fs::File::create(&tmp)?.write_all(&encode_part(self, part, fingerprint))?;
            std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        }
        // The single-file cache written before the index was split in parts.
        let _ = std::fs::remove_file(dir.join(LEGACY_FILE));
        Ok(())
    }

    pub fn stats(&self) -> IndexStats {
        IndexStats {
            ytds: self.ytd_by_name.len(),
            archetypes: self.archetype_txd.len(),
            resident_textures: self.resident_textures.len(),
            parent_txds: self.parent_txds.len(),
            interiors: self.mlo_ytyp.len(),
            interior_placements: self.mlo_instances.values().map(|v| v.len()).sum(),
            collision_files: self.ybn_by_name.len(),
            drawables: self.drawable_by_name.len(),
        }
    }

    /// A one-line summary of the parts this index holds.
    pub fn summary(&self) -> String {
        let s = self.stats();
        let mut out = Vec::new();
        if self.parts.contains(Parts::TEXTURES) {
            out.push(format!(
                "{} dictionaries, {} archetypes, {} resident textures, {} txd parent links",
                s.ytds, s.archetypes, s.resident_textures, s.parent_txds
            ));
        }
        if self.parts.contains(Parts::INTERIORS) {
            out.push(format!(
                "{} interiors, {} interior placement rows, {} collision files",
                s.interiors, s.interior_placements, s.collision_files
            ));
        }
        if self.parts.contains(Parts::MODELS) {
            out.push(format!("{} drawables, {} archetype boxes", s.drawables, self.archetype_box.len()));
        }
        out.join(", ")
    }

    /// The parts of the index a command needs: read from the cache, and
    /// whatever is missing or stale built now and cached. `None` (with a
    /// warning, not an error — callers each have their own fallback) when
    /// there's no `--exe`/`GTAV_PATH` to find the game directory from, or
    /// the build itself fails.
    pub fn load(exe: Option<&Path>, keys: Option<&GtaKeys>, parts: Parts) -> Option<Self> {
        let exe = exe?;
        let exe_path = match crate::keys::resolve_exe(exe) {
            Ok(p) => p,
            Err(err) => { eprintln!("warning: couldn't resolve --exe for the game index: {err}"); return None; }
        };
        let game_root = exe_path.parent()?.to_path_buf();
        let fingerprint = match archives_fingerprint(&game_root) {
            Ok(f) => f,
            Err(err) => { eprintln!("warning: couldn't list the game's archives: {err:#}"); return None; }
        };
        let dir = GameIndex::cache_dir(&exe_path);

        let mut index = GameIndex::default();
        let mut missing = Parts::NONE;
        let mut why = "first use";
        for part in parts.each() {
            let Some(path) = dir.as_ref().map(|d| d.join(part.file_name())).filter(|p| p.is_file()) else {
                missing |= part;
                continue;
            };
            match GameIndex::load_part(&path, part, fingerprint) {
                Ok(Ok(loaded)) => index.merge(loaded),
                Ok(Err(Stale)) => { missing |= part; why = "the game's archives changed"; }
                Err(err) => {
                    log::debug!("index: {} unreadable: {err:#}", path.display());
                    missing |= part;
                    why = "the cache is from another version of rage";
                }
            }
        }
        if missing.is_empty() {
            return Some(index);
        }

        eprintln!("Indexing the game's archives for {} ({why}); cached for next time.", missing.describe());
        let built = match GameIndex::build(&game_root, keys, missing) {
            Ok(built) => built,
            Err(err) => { eprintln!("warning: failed to index the game: {err:#}"); return None; }
        };
        if let Some(dir) = &dir
            && let Err(err) = built.save(dir, fingerprint)
        {
            eprintln!("warning: failed to cache the game index: {err:#}");
        }
        index.merge(built);
        Some(index)
    }
}

/// A part's cache file was written for other archives than the game has now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stale;

/// The file every part used to share, removed when parts are written.
pub const LEGACY_FILE: &str = "index.bin";

/// A hash of the relative path, size and modification time of every archive
/// under `game_root`. A cache written under another fingerprint is rebuilt,
/// so an updated, added or removed archive is picked up without anyone
/// clearing anything — the cache directory itself is only keyed on
/// `GTA5.exe`, which a DLC or a mod can change around.
pub fn archives_fingerprint(game_root: &Path) -> Result<u64> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for path in collect_archives(game_root)? {
        let meta = std::fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        let modified = meta.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64);
        let rel = path.strip_prefix(game_root).unwrap_or(&path).to_string_lossy().to_lowercase().replace('\\', "/");
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update(meta.len().to_le_bytes());
        hasher.update(modified.to_le_bytes());
    }
    Ok(u64::from_le_bytes(hasher.finalize()[..8].try_into().unwrap()))
}

/// Opens the archive at the end of `chain` (paths of nested `.rpf` entries,
/// each within the one before) starting from `top`.
fn open_chain(top: &Archive, chain: &[String], keys: Option<&GtaKeys>) -> Result<Archive> {
    let mut current: Option<Archive> = None;
    for nested_path in chain {
        let parent = current.as_ref().unwrap_or(top);
        let file = parent
            .find_file(nested_path)
            .with_context(|| format!("'{nested_path}' not found"))?;
        // The name passed on isn't cosmetic: RPF7's NG decryption selects
        // its per-archive key from the archive's own bare file name (and
        // the TOC length), so it must be `file.name`, not its full path
        // within the parent, or the TOC decrypts to garbage and every entry
        // name comes back as a placeholder. `open_nested` passes exactly that.
        let next = parent
            .open_nested(file, keys)
            .with_context(|| format!("failed to open nested archive '{nested_path}'"))?;
        current = Some(next);
    }
    current.context("empty archive chain")
}

/// Which maps place which interiors.
///
/// The game's world cache files (`gta5_cache_y.dat` and each DLC pack's
/// `cacheloaderdata_dlc/*_cache_y.dat`) list every map they describe and
/// every interior those maps place, so a map is read only when a cache file
/// names it as placing an interior, or when no cache file describes it at
/// all (script-loaded maps, heist apartments). That is about 1,300 of the
/// ~19,000 maps on a current install, with the same result as reading every
/// one; without any cache files, every map is read.
fn interior_placements(
    ymaps: &[(u32, EntryLoc)],
    covered: &HashSet<u32>,
    proxies: &[(u32, u32)],
    keys: Option<&GtaKeys>,
) -> HashMap<u32, Vec<EntryLoc>> {
    use rayon::prelude::*;

    // Top archive -> nested chain -> the maps in it, by scan position.
    let mut groups: HashMap<&Path, HashMap<&[String], Vec<usize>>> = HashMap::new();
    for seq in maps_to_read(ymaps, covered, proxies) {
        let loc = &ymaps[seq].1;
        groups.entry(loc.top_archive.as_path()).or_default().entry(loc.nested_rpfs.as_slice()).or_default().push(seq);
    }

    let mut found: Vec<(usize, Vec<u32>)> = groups
        .into_par_iter()
        .flat_map_iter(|(top, chains)| read_placements(top, chains, ymaps, keys))
        .collect();
    found.sort_by_key(|(seq, _)| *seq);

    let mut out = HashMap::new();
    for (seq, archetypes) in found {
        for archetype in archetypes {
            record_mlo_instance(&mut out, archetype, ymaps[seq].1.clone());
        }
    }
    out
}

/// Positions in `ymaps` worth reading for interior placements: maps a cache
/// file names as placing an interior, and maps no cache file describes.
fn maps_to_read(ymaps: &[(u32, EntryLoc)], covered: &HashSet<u32>, proxies: &[(u32, u32)]) -> Vec<usize> {
    let placing: HashSet<u32> = proxies.iter().map(|&(_, map)| map).collect();
    ymaps
        .iter()
        .enumerate()
        .filter(|(_, (stem, _))| placing.contains(stem) || !covered.contains(stem))
        .map(|(seq, _)| seq)
        .collect()
}

/// Reads the given maps out of one top-level archive and returns, for each
/// map that places interiors, its scan position and the distinct interior
/// archetypes it places, in entity order.
fn read_placements(
    top: &Path,
    chains: HashMap<&[String], Vec<usize>>,
    ymaps: &[(u32, EntryLoc)],
    keys: Option<&GtaKeys>,
) -> Vec<(usize, Vec<u32>)> {
    let top_archive = match Archive::open(top, keys) {
        Ok(a) => a,
        Err(err) => { log::debug!("index: {} unreadable: {err}", top.display()); return Vec::new(); }
    };
    let mut out = Vec::new();
    for (chain, seqs) in chains {
        let nested;
        let archive = if chain.is_empty() {
            &top_archive
        } else {
            match open_chain(&top_archive, chain, keys) {
                Ok(a) => { nested = a; &nested }
                Err(err) => { log::debug!("index: {}: {err:#}", top.display()); continue; }
            }
        };
        for seq in seqs {
            let inner = &ymaps[seq].1.inner_path;
            let Some(file) = archive.find_file(inner) else {
                log::debug!("index: '{inner}' not found again");
                continue;
            };
            let entities = match archive.extract(file, keys).and_then(|data| parse_ymap_entities(&data)) {
                Ok(entities) => entities,
                Err(err) => { log::debug!("index: failed to read '{inner}': {err}"); continue; }
            };
            // One entry per (archetype, .ymap): a map that places the same
            // interior twice is still read once, and `plot` re-reads every
            // instance out of it anyway.
            let mut seen = HashSet::new();
            let archetypes: Vec<u32> = entities
                .iter()
                .filter(|e| e.is_mlo_instance)
                .map(|e| e.archetype_hash)
                .filter(|h| seen.insert(*h))
                .collect();
            if !archetypes.is_empty() {
                out.push((seq, archetypes));
            }
        }
    }
    out
}

/// One `\r`-overwritten stderr line per archive while the index is built.
fn progress(n: usize, total: usize, archive: &Path) {
    let name = archive.to_string_lossy().replace('\\', "/");
    let label = if name.len() > 50 { format!("...{}", &name[name.len() - 47..]) } else { name };
    eprint!("\r[{n:>3}/{total}] {label:<52}");
    let _ = std::io::stderr().flush();
}

/// Sizes of each map in a [`GameIndex`], for `rage index build`/`info` and
/// `GameIndex::summary`.
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexStats {
    pub ytds: usize,
    pub archetypes: usize,
    pub resident_textures: usize,
    pub parent_txds: usize,
    pub interiors: usize,
    /// `.ymap` locations listed across every interior — rows, not distinct
    /// placements: a map overridden by a DLC pack is listed from both
    /// archives, and `plot` is what collapses the pair by position.
    pub interior_placements: usize,
    pub collision_files: usize,
    /// `.ydr`/`.ydd`/`.yft` files by name.
    pub drawables: usize,
}

/// Every .rpf under `game_root` in the game's load order: base archives,
/// then `update.rpf`, then DLC packs in `dlclist.xml`/`setup2.xml` order.
/// Later archives override earlier ones.
pub fn ranked_archives(game_root: &Path, keys: Option<&GtaKeys>) -> Result<Vec<PathBuf>> {
    let mut archives = collect_archives(game_root)?;
    if archives.is_empty() {
        bail!("no .rpf archives found under {}", game_root.display());
    }
    let dlc_rank = dlc_load_order(game_root, keys);
    archives.sort_by_key(|p| {
        let tier = archive_tier(p);
        let rank = match &tier {
            ArchiveTier::Dlc(name) => dlc_rank.get(name).copied().unwrap_or(u32::MAX),
            _ => 0,
        };
        (tier.rank(), rank, p.clone())
    });
    Ok(archives)
}

/// Which of the game's three load tiers an archive belongs to, matching
/// CodeWalker's own base-RPFs-then-`update.rpf`-then-DLC-packs scan order
/// (`GameFileCache.cs`'s `InitFileCache`/`InitDlcList`). A DLC archive
/// carries its pack name (from its path, e.g. `mpheist` for
/// `.../dlcpacks/mpheist/dlc.rpf`) so `GameIndex::build` can rank it
/// against `dlc_load_order`'s result.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ArchiveTier {
    Base,
    Update,
    Dlc(String),
}

impl ArchiveTier {
    fn rank(&self) -> u8 {
        match self {
            ArchiveTier::Base => 0,
            ArchiveTier::Update => 1,
            ArchiveTier::Dlc(_) => 2,
        }
    }
}

fn archive_tier(path: &Path) -> ArchiveTier {
    let normalized = path.to_string_lossy().to_lowercase().replace('\\', "/");
    // Checked before the `update` test: every `dlcpacks` archive lives
    // under `update/x64/dlcpacks/...`, so it would otherwise be
    // misclassified as plain `update`.
    if let Some(name) = dlc_pack_name(&normalized) {
        return ArchiveTier::Dlc(name);
    }
    if normalized.contains("/update/") {
        return ArchiveTier::Update;
    }
    ArchiveTier::Base
}

/// Extracts the pack directory name from a normalized (lowercase, `/`)
/// archive path such as `.../update/x64/dlcpacks/mpheist/dlc1.rpf` ->
/// `Some("mpheist")`. Matches on any `dlcN.rpf` (some packs ship
/// `dlc.rpf` plus `dlc1.rpf`/`dlc2.rpf` subpacks alongside it), since all
/// of a pack's own archives share one load-order rank.
fn dlc_pack_name(normalized_path: &str) -> Option<String> {
    let after = normalized_path.split("/dlcpacks/").nth(1)?;
    let name = after.split('/').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Computes each DLC pack's load-order rank the way CodeWalker's
/// `InitDlcList` does: `dlclist.xml`'s own listed order as the base
/// ordering, then a stable sort by each pack's `setup2.xml` `<order>`
/// (`DlcSetupFiles.OrderBy(o => o.order)` — `OrderBy` is stable, so ties
/// keep their `dlclist.xml` position). Lower rank loads first. A pack
/// this can't place — `update.rpf`/`dlclist.xml` unreadable, the pack
/// missing from `dlclist.xml`, or its own `setup2.xml` unreadable — is
/// simply absent from the result; `GameIndex::build` falls such packs
/// back to `u32::MAX` (after every ranked pack, in path order), since a
/// texture index too incomplete to rank every DLC is still far more
/// useful than no index at all.
fn dlc_load_order(game_root: &Path, keys: Option<&GtaKeys>) -> HashMap<String, u32> {
    let update_rpf = game_root.join("update").join("update.rpf");
    let archive = match Archive::open(&update_rpf, keys) {
        Ok(a) => a,
        Err(err) => {
            log::debug!("index: no DLC load order ({} unreadable: {err})", update_rpf.display());
            return HashMap::new();
        }
    };
    if archive.require_keys(keys).is_err() {
        log::debug!("index: no DLC load order ({} needs keys)", update_rpf.display());
        return HashMap::new();
    }

    let names = match archive.find_file("common/data/dlclist.xml") {
        Some(file) => match archive.extract(file, keys) {
            Ok(data) => match parse_dlc_list(&data) {
                Ok(names) => names,
                Err(err) => { log::debug!("index: failed to parse dlclist.xml: {err}"); return HashMap::new(); }
            },
            Err(err) => { log::debug!("index: failed to extract dlclist.xml: {err}"); return HashMap::new(); }
        },
        None => { log::debug!("index: update.rpf has no common/data/dlclist.xml"); return HashMap::new(); }
    };

    let mut packs: Vec<(String, i32)> = Vec::with_capacity(names.len());
    for name in names {
        let dlc_rpf = game_root.join("update").join("x64").join("dlcpacks").join(&name).join("dlc.rpf");
        let order = match Archive::open(&dlc_rpf, keys) {
            Ok(pack) => match pack.find_file("setup2.xml") {
                Some(file) => match pack.extract(file, keys) {
                    Ok(data) => parse_dlc_setup_order(&data).unwrap_or(-1),
                    Err(err) => { log::debug!("index: failed to extract '{}' setup2.xml: {err}", name); -1 }
                },
                None => { log::debug!("index: '{}' has no setup2.xml", name); -1 }
            },
            Err(err) => { log::debug!("index: DLC pack '{}' unreadable ({err}); using dlclist.xml order only", name); -1 }
        };
        packs.push((name, order));
    }

    rank_dlc_packs(packs)
}

/// The pure sort/rank step of `dlc_load_order`, split out so it's testable
/// without real archives on disk: `packs` (name, `setup2.xml` order),
/// already in `dlclist.xml` order, in, `name -> rank` (0 = loads first) out.
/// A *stable* sort by `order` keeps `dlclist.xml` position as the tie-break
/// for equal (including default `-1`) orders — mirroring CodeWalker's
/// `DlcSetupFiles.OrderBy(o => o.order)`, itself a stable sort.
fn rank_dlc_packs(mut packs: Vec<(String, i32)>) -> HashMap<String, u32> {
    packs.sort_by_key(|(_, order)| *order);
    packs.into_iter().enumerate().map(|(rank, (name, _))| (name, rank as u32)).collect()
}

fn index_archive(archive: &Archive, archive_path: &Path, nested_rpfs: &[String], keys: Option<&GtaKeys>, parts: Parts, out: &mut Partial) {
    let textures = parts.contains(Parts::TEXTURES);
    let interiors = parts.contains(Parts::INTERIORS);
    let models = parts.contains(Parts::MODELS);

    for file in archive.list_files() {
        let name_lower = file.name.to_lowercase();

        if name_lower.ends_with(".rpf") {
            let Ok(nested) = archive.open_nested(file, keys) else { continue };
            let mut chain = nested_rpfs.to_vec();
            chain.push(file.path.clone());
            index_archive(&nested, archive_path, &chain, keys, parts, out);
            continue;
        }

        if TXD_RELATIONSHIP_FILES.contains(&name_lower.as_str()) {
            if textures {
                match archive.extract(file, keys) {
                    Ok(data) => match parse_txd_relationships(&data) {
                        Ok(rels) => merge_txd_relationships(&mut out.index.parent_txds, &rels),
                        Err(err) => log::debug!("index: failed to parse '{}': {err}", file.path),
                    },
                    Err(err) => log::debug!("index: failed to extract '{}': {err}", file.path),
                }
            }
            continue;
        }

        if name_lower.ends_with("cache_y.dat") {
            // CodeWalker's own test (`GameFileCache.cs`: `EndsWith("cache_y.dat")`).
            if interiors {
                match archive.extract(file, keys).and_then(|data| parse_cache_dat(&data)) {
                    Ok(cache) => {
                        out.covered.extend(cache.map_nodes.iter().map(|n| n.name));
                        out.proxies.extend(cache.interior_proxies.iter().map(|p| (p.name, p.parent)));
                    }
                    Err(err) => log::debug!("index: failed to read '{}': {err}", file.path),
                }
            }
            continue;
        }

        let stem = crate::resources::file_stem(&name_lower);
        let loc = || EntryLoc {
            top_archive: archive_path.to_path_buf(),
            nested_rpfs: nested_rpfs.to_vec(),
            inner_path: file.path.clone(),
        };

        if name_lower.ends_with(".ytd") {
            if !textures {
                continue;
            }
            out.index.ytd_by_name.insert(rage_joaat(&stem), loc());

            if RESIDENT_DICTS.contains(&stem.as_str())
                && let Ok(data) = archive.extract(file, keys)
                && let Ok(dict) = parse_ytd(&data)
            {
                let owner_hash = rage_joaat(&stem);
                for tex in dict {
                    // The stored hash block falls back to 0 when absent, so
                    // this is indexed under both that hash and the hashed
                    // lowercase name to make the lookup unmissable — the
                    // caller (a still-missing texture name, post-render)
                    // only ever has the name.
                    if tex.name_hash != 0 {
                        out.index.resident_textures.insert(tex.name_hash, owner_hash);
                    }
                    out.index.resident_textures.insert(rage_joaat(&tex.name.to_lowercase()), owner_hash);
                }
            }
        } else if name_lower.ends_with(".ytyp") {
            // Every part reads archetypes, so every build decodes these.
            let Ok(data) = archive.extract(file, keys) else { continue };
            let Ok(ytyp) = parse_ytyp(&data) else { continue };
            for a in ytyp.archetypes {
                if textures && a.texture_dict_hash != 0 {
                    out.index.archetype_txd.insert(a.name_hash, a.texture_dict_hash);
                }
                if interiors && a.is_mlo {
                    out.index.mlo_ytyp.insert(a.name_hash, loc());
                }
                if models {
                    out.index.archetype_box.insert(a.name_hash, (a.bb_min, a.bb_max));
                    let model = if a.in_drawable_dictionary() { a.drawable_dictionary_hash } else { a.model_hash() };
                    if a.in_drawable_dictionary() || a.is_fragment() || model != a.name_hash {
                        out.index.archetype_asset.insert(a.name_hash, (a.asset_type, model));
                    }
                }
            }
        } else if name_lower.ends_with(".ybn") {
            // Name only: a `.ybn` is never parsed while indexing.
            if interiors {
                out.index.ybn_by_name.insert(rage_joaat(&stem), loc());
            }
        } else if name_lower.ends_with(".ydr") || name_lower.ends_with(".ydd") || name_lower.ends_with(".yft") {
            // Name only, like collision: a model is read when a plot places it.
            if models {
                out.index.drawable_by_name.insert(rage_joaat(&stem), loc());
            }
        } else if name_lower.ends_with(".ymap") && interiors {
            // Where it is, for now: whether it is read at all is decided
            // once every cache_y.dat is in (`interior_placements`).
            out.ymaps.push((rage_joaat(&stem), loc()));
        }
    }
}

/// Files one `.ymap`'s placement of an interior. Every location is kept, in
/// the scan order the archives were ranked into (base -> update -> DLC), so
/// the last entry for a given placement is the copy the game loads.
///
/// Nothing is merged here on the strength of a file name: a DLC copy of a
/// base-game map and an unrelated map that happens to share its name look
/// identical from the index's side, and dropping one would silently hide a
/// real placement. `plot` reads every location and decides which of them
/// place the interior in the same spot.
fn record_mlo_instance(out: &mut HashMap<u32, Vec<EntryLoc>>, archetype: u32, loc: EntryLoc) {
    out.entry(archetype).or_default().push(loc);
}

/// Merges `rels` into `out`, first child-wins — matching CodeWalker's
/// `addTxdRelationships` (`GameFileCache.cs`: `if (!parentTxds.ContainsKey(chash))`)
/// and each individual file's own parse. Combined with `GameIndex::build`'s
/// base -> update -> DLC scan order (ranked by `archive_tier`/
/// `dlc_load_order`, itself CodeWalker's `InitDlcList`: `dlclist.xml` order
/// with a stable `setup2.xml` `<order>` sort on top — see
/// `GameFileCache.cs:476`), this means the base game's relationship beats a
/// DLC's, matching CodeWalker's own first-wins merge for real, not just a
/// stable pick among an arbitrary scan order.
fn merge_txd_relationships(out: &mut HashMap<u32, u32>, rels: &[rage_formats::TxdRelationship]) {
    for rel in rels {
        let child = rage_joaat(&rel.child.to_lowercase());
        let parent = rage_joaat(&rel.parent.to_lowercase());
        if child == 0 || parent == 0 || child == parent {
            continue;
        }
        out.entry(child).or_insert(parent);
    }
}

// ─── Minimal binary (de)serialization — no serde dependency for one struct ──

const MAGIC: u32 = 0x5850_4652; // "RPFX" little-endian
// The cache directory is keyed only on the GTA5 executable's size+mtime,
// not on any tool version, so every change to what a build produces bumps
// this, or an old cache would be served to the new code.
// 2: parent_txds filled in. 3: archives scanned base -> update -> DLC.
// 4: archetype bounding boxes. 5: MLO ytyp/ymap locations and ybn names.
// 6: every placement ymap is kept. 7: drawable locations and archetype
// asset bindings.
// 8: one file per part, each carrying the archives' fingerprint; entries
// written in key order.
const FORMAT_VERSION: u32 = 8;

fn write_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    write_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

fn write_loc(buf: &mut Vec<u8>, loc: &EntryLoc) {
    write_str(buf, &loc.top_archive.to_string_lossy());
    write_u32(buf, loc.nested_rpfs.len() as u32);
    for n in &loc.nested_rpfs {
        write_str(buf, n);
    }
    write_str(buf, &loc.inner_path);
}

/// Keys in order, so two builds of the same game write identical files.
fn sorted<V>(map: &HashMap<u32, V>) -> Vec<(u32, &V)> {
    let mut entries: Vec<(u32, &V)> = map.iter().map(|(k, v)| (*k, v)).collect();
    entries.sort_by_key(|(k, _)| *k);
    entries
}

fn write_locs(buf: &mut Vec<u8>, map: &HashMap<u32, EntryLoc>) {
    write_u32(buf, map.len() as u32);
    for (hash, loc) in sorted(map) {
        write_u32(buf, hash);
        write_loc(buf, loc);
    }
}

fn write_pairs(buf: &mut Vec<u8>, map: &HashMap<u32, u32>) {
    write_u32(buf, map.len() as u32);
    for (k, v) in sorted(map) {
        write_u32(buf, k);
        write_u32(buf, *v);
    }
}

fn encode_part(index: &GameIndex, part: Parts, fingerprint: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u32(&mut buf, MAGIC);
    write_u32(&mut buf, FORMAT_VERSION);
    write_u32(&mut buf, part.0 as u32);
    buf.extend_from_slice(&fingerprint.to_le_bytes());

    match part {
        Parts::TEXTURES => {
            write_locs(&mut buf, &index.ytd_by_name);
            write_pairs(&mut buf, &index.archetype_txd);
            write_pairs(&mut buf, &index.resident_textures);
            write_pairs(&mut buf, &index.parent_txds);
        }
        Parts::INTERIORS => {
            write_locs(&mut buf, &index.mlo_ytyp);
            write_u32(&mut buf, index.mlo_instances.len() as u32);
            for (hash, locs) in sorted(&index.mlo_instances) {
                write_u32(&mut buf, hash);
                write_u32(&mut buf, locs.len() as u32);
                for loc in locs {
                    write_loc(&mut buf, loc);
                }
            }
            write_locs(&mut buf, &index.ybn_by_name);
        }
        Parts::MODELS => {
            write_u32(&mut buf, index.archetype_box.len() as u32);
            for (k, (lo, hi)) in sorted(&index.archetype_box) {
                write_u32(&mut buf, k);
                for f in [lo.x, lo.y, lo.z, hi.x, hi.y, hi.z] {
                    buf.extend_from_slice(&f.to_le_bytes());
                }
            }
            write_locs(&mut buf, &index.drawable_by_name);
            write_u32(&mut buf, index.archetype_asset.len() as u32);
            for (hash, (kind, model)) in sorted(&index.archetype_asset) {
                write_u32(&mut buf, hash);
                write_u32(&mut buf, *kind);
                write_u32(&mut buf, *model);
            }
        }
        _ => unreachable!("encode_part takes a single part"),
    }
    buf
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn u32(&mut self) -> Result<u32> {
        let bytes = self.data.get(self.pos..self.pos + 4).context("index: truncated (u32)")?;
        self.pos += 4;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(self.u32()? as u64 | (self.u32()? as u64) << 32)
    }

    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        let bytes = self.data.get(self.pos..self.pos + len).context("index: truncated (string)")?;
        self.pos += len;
        String::from_utf8(bytes.to_vec()).context("index: invalid UTF-8")
    }

    /// A count read from the file, which may be truncated or garbled:
    /// capped for preallocation so the reads that follow fail with
    /// "truncated" rather than aborting on a huge allocation.
    fn count(&mut self) -> Result<(usize, usize)> {
        let n = self.u32()? as usize;
        Ok((n, n.min(1 << 20)))
    }
}

fn read_loc(c: &mut Cursor) -> Result<EntryLoc> {
    let top_archive = PathBuf::from(c.string()?);
    let nested_count = c.u32()? as usize;
    let mut nested_rpfs = Vec::with_capacity(nested_count.min(1024));
    for _ in 0..nested_count {
        nested_rpfs.push(c.string()?);
    }
    let inner_path = c.string()?;
    Ok(EntryLoc { top_archive, nested_rpfs, inner_path })
}

fn read_locs(c: &mut Cursor) -> Result<HashMap<u32, EntryLoc>> {
    let (n, cap) = c.count()?;
    let mut map = HashMap::with_capacity(cap);
    for _ in 0..n {
        let hash = c.u32()?;
        map.insert(hash, read_loc(c)?);
    }
    Ok(map)
}

fn read_pairs(c: &mut Cursor) -> Result<HashMap<u32, u32>> {
    let (n, cap) = c.count()?;
    let mut map = HashMap::with_capacity(cap);
    for _ in 0..n {
        let k = c.u32()?;
        map.insert(k, c.u32()?);
    }
    Ok(map)
}

/// Decodes one part's file into the fingerprint it was written for and an
/// index holding just that part.
fn decode_part(data: &[u8], part: Parts) -> Result<(u64, GameIndex)> {
    let mut c = Cursor { data, pos: 0 };
    if c.u32()? != MAGIC {
        bail!("index: bad magic");
    }
    let version = c.u32()?;
    if version != FORMAT_VERSION {
        bail!("index: unsupported format version {version}");
    }
    let stored = c.u32()?;
    if stored != part.0 as u32 {
        bail!("index: file holds part {stored}, expected {}", part.0);
    }
    let fingerprint = c.u64()?;

    let mut index = GameIndex { parts: part, ..Default::default() };
    match part {
        Parts::TEXTURES => {
            index.ytd_by_name = read_locs(&mut c)?;
            index.archetype_txd = read_pairs(&mut c)?;
            index.resident_textures = read_pairs(&mut c)?;
            index.parent_txds = read_pairs(&mut c)?;
        }
        Parts::INTERIORS => {
            index.mlo_ytyp = read_locs(&mut c)?;
            let (n, cap) = c.count()?;
            index.mlo_instances.reserve(cap);
            for _ in 0..n {
                let hash = c.u32()?;
                let (locs_count, locs_cap) = c.count()?;
                let mut locs = Vec::with_capacity(locs_cap.min(1024));
                for _ in 0..locs_count {
                    locs.push(read_loc(&mut c)?);
                }
                index.mlo_instances.insert(hash, locs);
            }
            index.ybn_by_name = read_locs(&mut c)?;
        }
        Parts::MODELS => {
            let (n, cap) = c.count()?;
            index.archetype_box.reserve(cap);
            for _ in 0..n {
                let k = c.u32()?;
                let mut f = [0f32; 6];
                for v in &mut f {
                    *v = c.f32()?;
                }
                index.archetype_box.insert(k, (Vec3::new(f[0], f[1], f[2]), Vec3::new(f[3], f[4], f[5])));
            }
            index.drawable_by_name = read_locs(&mut c)?;
            let (n, cap) = c.count()?;
            index.archetype_asset.reserve(cap);
            for _ in 0..n {
                let hash = c.u32()?;
                let kind = c.u32()?;
                let model = c.u32()?;
                index.archetype_asset.insert(hash, (kind, model));
            }
        }
        _ => bail!("index: {} is not a single part", part.0),
    }
    if c.pos != data.len() {
        bail!("index: {} trailing bytes", data.len() - c.pos);
    }
    Ok((fingerprint, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(inner: &str) -> EntryLoc {
        EntryLoc {
            top_archive: PathBuf::from("C:/game/x64a.rpf"),
            nested_rpfs: vec!["levels/gta5/interiors.rpf".to_string()],
            inner_path: inner.to_string(),
        }
    }

    fn sample() -> GameIndex {
        let mut index = GameIndex { parts: Parts::ALL, ..Default::default() };
        index.ytd_by_name.insert(1, EntryLoc {
            top_archive: PathBuf::from("C:/game/x64f.rpf"),
            nested_rpfs: vec!["levels/gta5/x.rpf".to_string()],
            inner_path: "prop_table_chair_02.ytd".to_string(),
        });
        index.archetype_txd.insert(2, 3);
        index.resident_textures.insert(4, 5);
        index.parent_txds.insert(6, 7);
        index.archetype_box.insert(8, (Vec3::new(-1.0, -2.0, -3.0), Vec3::new(1.0, 2.0, 3.0)));
        index.mlo_ytyp.insert(10, loc("v_int_3.ytyp"));
        index.mlo_instances.insert(10, vec![loc("a.ymap"), loc("b.ymap")]);
        index.ybn_by_name.insert(10, loc("v_int_3.ybn"));
        index.drawable_by_name.insert(11, loc("prop_x.ydr"));
        index.archetype_asset.insert(12, (3, 13));
        index
    }

    #[test]
    fn each_part_round_trips_through_its_own_file() {
        let index = sample();
        let mut back = GameIndex::default();
        for part in Parts::ALL.each() {
            let (fingerprint, decoded) = decode_part(&encode_part(&index, part, 0xABCD_1234_5678), part).expect("should decode");
            assert_eq!(fingerprint, 0xABCD_1234_5678);
            assert_eq!(decoded.parts, part);
            back.merge(decoded);
        }
        assert_eq!(back.parts, Parts::ALL);
        assert_eq!(back.ytd_by_name, index.ytd_by_name);
        assert_eq!(back.archetype_txd, index.archetype_txd);
        assert_eq!(back.resident_textures, index.resident_textures);
        assert_eq!(back.parent_txds, index.parent_txds);
        assert_eq!(back.archetype_box, index.archetype_box);
        assert_eq!(back.mlo_ytyp, index.mlo_ytyp);
        assert_eq!(back.mlo_instances, index.mlo_instances);
        assert_eq!(back.ybn_by_name, index.ybn_by_name);
        assert_eq!(back.drawable_by_name, index.drawable_by_name);
        assert_eq!(back.archetype_asset, index.archetype_asset);
    }

    #[test]
    fn a_part_file_holds_only_its_own_maps() {
        let (_, textures) = decode_part(&encode_part(&sample(), Parts::TEXTURES, 0), Parts::TEXTURES).unwrap();
        assert_eq!(textures.ytd_by_name.len(), 1);
        assert!(textures.mlo_ytyp.is_empty() && textures.drawable_by_name.is_empty());
    }

    #[test]
    fn encoding_does_not_depend_on_insertion_order() {
        let mut a = GameIndex::default();
        let mut b = GameIndex::default();
        for k in 0..500u32 {
            a.archetype_txd.insert(k, k + 1);
            b.archetype_txd.insert(499 - k, 500 - k);
        }
        assert_eq!(encode_part(&a, Parts::TEXTURES, 1), encode_part(&b, Parts::TEXTURES, 1));
    }

    #[test]
    fn a_file_for_another_part_is_refused() {
        let bytes = encode_part(&sample(), Parts::MODELS, 0);
        assert!(decode_part(&bytes, Parts::TEXTURES).is_err());
    }

    #[test]
    fn a_truncated_or_padded_file_is_refused() {
        let bytes = encode_part(&sample(), Parts::INTERIORS, 0);
        assert!(decode_part(&bytes[..bytes.len() - 1], Parts::INTERIORS).is_err());
        let mut padded = bytes.clone();
        padded.push(0);
        assert!(decode_part(&padded, Parts::INTERIORS).is_err());
    }

    #[test]
    fn a_part_written_for_other_archives_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = sample();
        index.parts = Parts::TEXTURES;
        index.save(dir.path(), 7).unwrap();
        let path = dir.path().join(Parts::TEXTURES.file_name());
        assert!(matches!(GameIndex::load_part(&path, Parts::TEXTURES, 7), Ok(Ok(_))));
        assert!(matches!(GameIndex::load_part(&path, Parts::TEXTURES, 8), Ok(Err(Stale))));
    }

    #[test]
    fn saving_writes_one_file_per_part_and_drops_the_old_single_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LEGACY_FILE), b"old").unwrap();
        let mut index = sample();
        index.parts = Parts::TEXTURES | Parts::MODELS;
        index.save(dir.path(), 1).unwrap();
        assert!(dir.path().join("textures.bin").is_file());
        assert!(dir.path().join("models.bin").is_file());
        assert!(!dir.path().join("interiors.bin").exists());
        assert!(!dir.path().join(LEGACY_FILE).exists());
    }

    #[test]
    fn the_fingerprint_follows_every_archive_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("update")).unwrap();
        std::fs::write(dir.path().join("x64a.rpf"), b"one").unwrap();
        std::fs::write(dir.path().join("update/update.rpf"), b"two").unwrap();
        let first = archives_fingerprint(dir.path()).unwrap();
        assert_eq!(archives_fingerprint(dir.path()).unwrap(), first, "stable when nothing changed");

        std::fs::write(dir.path().join("GTA5.exe.log"), b"not an archive").unwrap();
        assert_eq!(archives_fingerprint(dir.path()).unwrap(), first, "other files do not count");

        std::fs::write(dir.path().join("update/update.rpf"), b"two, patched").unwrap();
        let patched = archives_fingerprint(dir.path()).unwrap();
        assert_ne!(patched, first, "a changed archive changes it");

        std::fs::write(dir.path().join("update/dlc.rpf"), b"new").unwrap();
        assert_ne!(archives_fingerprint(dir.path()).unwrap(), patched, "an added archive changes it");
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(decode_part(&[0u8; 20], Parts::TEXTURES).is_err());
    }

    #[test]
    fn rejects_every_older_format_version() {
        for stale in 1..FORMAT_VERSION {
            let mut bytes = encode_part(&sample(), Parts::TEXTURES, 0);
            bytes[4..8].copy_from_slice(&stale.to_le_bytes());
            assert!(decode_part(&bytes, Parts::TEXTURES).is_err(), "version {stale} should be rebuilt, not read");
        }
    }

    #[test]
    fn write_loc_and_read_loc_are_symmetric() {
        for original in [
            loc("v_int_3.ytyp"),
            EntryLoc {
                top_archive: PathBuf::from(r"C:\game\update\update.rpf"),
                nested_rpfs: Vec::new(),
                inner_path: "x.ybn".to_string(),
            },
            EntryLoc {
                top_archive: PathBuf::from("C:/game/x64a.rpf"),
                nested_rpfs: vec!["a.rpf".to_string(), "b.rpf".to_string()],
                inner_path: "deep/c.ymap".to_string(),
            },
        ] {
            let mut buf = Vec::new();
            write_loc(&mut buf, &original);
            let mut c = Cursor { data: &buf, pos: 0 };
            assert_eq!(read_loc(&mut c).expect("should read back"), original);
            assert_eq!(c.pos, buf.len(), "read_loc should consume exactly what write_loc wrote");
        }
    }

    #[test]
    fn parts_combine_and_list_in_a_fixed_order() {
        let p = Parts::MODELS | Parts::TEXTURES;
        assert!(p.contains(Parts::TEXTURES) && p.contains(Parts::MODELS) && !p.contains(Parts::INTERIORS));
        assert!(!p.contains(Parts::NONE));
        assert_eq!(p.describe(), "textures, models");
        assert_eq!(Parts::ALL.each().count(), 3);
        assert!(Parts::NONE.is_empty());
    }

    #[test]
    fn merging_a_later_archive_overrides_names_but_not_parent_links() {
        let mut first = GameIndex { parts: Parts::TEXTURES, ..Default::default() };
        first.ytd_by_name.insert(1, loc("base.ytd"));
        first.parent_txds.insert(5, 6);
        let mut later = GameIndex { parts: Parts::TEXTURES, ..Default::default() };
        later.ytd_by_name.insert(1, loc("dlc.ytd"));
        later.parent_txds.insert(5, 9);
        later.mlo_instances.insert(3, vec![loc("b.ymap")]);
        first.mlo_instances.insert(3, vec![loc("a.ymap")]);

        first.merge(later);
        assert_eq!(first.ytd_by_name[&1], loc("dlc.ytd"));
        assert_eq!(first.parent_txds[&5], 6);
        assert_eq!(first.mlo_instances[&3], vec![loc("a.ymap"), loc("b.ymap")]);
    }

    /// The world cache vouches for the maps it describes: only those it
    /// names as placing an interior are read, plus every map it does not
    /// describe at all.
    #[test]
    fn maps_to_read_are_the_placing_and_the_undescribed_ones() {
        let ymaps: Vec<(u32, EntryLoc)> = [10, 20, 30, 40, 20]
            .iter()
            .map(|&h| (h, loc(&format!("{h}.ymap"))))
            .collect();
        let covered: HashSet<u32> = [10, 20, 30].into_iter().collect();
        let proxies = vec![(777, 20)];
        // 20 places an interior (both copies), 40 is described by nothing.
        assert_eq!(maps_to_read(&ymaps, &covered, &proxies), vec![1, 3, 4]);
    }

    #[test]
    fn without_world_cache_files_every_map_is_read() {
        let ymaps: Vec<(u32, EntryLoc)> = (0..4).map(|h| (h, loc("m.ymap"))).collect();
        assert_eq!(maps_to_read(&ymaps, &HashSet::new(), &[]), vec![0, 1, 2, 3]);
    }

    /// A same-named map in a later archive is usually a DLC copy of the base
    /// one, but it can equally be an unrelated map that happens to share the
    /// name — so both are kept, in rank order, and `plot` decides which
    /// placements are really the same by where they put the interior.
    #[test]
    fn same_named_maps_from_different_archives_are_both_kept() {
        let base = EntryLoc {
            top_archive: PathBuf::from("C:/game/x64a.rpf"),
            nested_rpfs: vec!["levels/gta5/_citye/indust_01.rpf".to_string()],
            inner_path: "id1_03_interior.ymap".to_string(),
        };
        let dlc = EntryLoc {
            top_archive: PathBuf::from("C:/game/update/x64/dlcpacks/mpheist/dlc.rpf"),
            nested_rpfs: vec!["x64/levels/mpheist/interiors.rpf".to_string()],
            inner_path: "ID1_03_Interior.ymap".to_string(), // same map, different case
        };
        let elsewhere = EntryLoc { inner_path: "other.ymap".to_string(), ..base.clone() };

        let mut out: HashMap<u32, Vec<EntryLoc>> = HashMap::new();
        let base_kept = base.clone();
        record_mlo_instance(&mut out, 7, base);
        record_mlo_instance(&mut out, 7, elsewhere.clone());
        record_mlo_instance(&mut out, 7, dlc.clone());

        assert_eq!(
            out[&7],
            vec![base_kept, elsewhere, dlc],
            "every location should be listed, in the order the archives were scanned"
        );
    }

    #[test]
    fn different_archetypes_keep_their_own_placement_lists() {
        let a = loc("a.ymap");
        let mut out: HashMap<u32, Vec<EntryLoc>> = HashMap::new();
        record_mlo_instance(&mut out, 1, a.clone());
        record_mlo_instance(&mut out, 2, a.clone());
        assert_eq!(out[&1], vec![a.clone()]);
        assert_eq!(out[&2], vec![a]);
    }

    #[test]
    fn summary_counts_the_interior_maps() {
        let mut index = GameIndex { parts: Parts::INTERIORS, ..Default::default() };
        index.mlo_ytyp.insert(1, loc("i.ytyp"));
        index.mlo_instances.insert(1, vec![loc("a.ymap"), loc("b.ymap")]);
        index.ybn_by_name.insert(2, loc("i.ybn"));

        let summary = index.summary();
        assert!(summary.contains("1 interiors"), "{summary}");
        assert!(summary.contains("2 interior placement rows"), "{summary}");
        assert!(summary.contains("1 collision files"), "{summary}");
        assert!(!summary.contains("dictionaries"), "only loaded parts are summarised: {summary}");
    }

    #[test]
    fn merge_txd_relationships_is_first_wins() {
        let mut out = HashMap::new();
        merge_txd_relationships(&mut out, &[rel("a", "b")]);
        merge_txd_relationships(&mut out, &[rel("a", "c")]);
        assert_eq!(out.get(&rage_joaat("a")), Some(&rage_joaat("b")));
    }

    #[test]
    fn merge_txd_relationships_lowercases_before_hashing() {
        let mut out = HashMap::new();
        merge_txd_relationships(&mut out, &[rel("PROP_Foo", "VehShare")]);
        assert_eq!(out.get(&rage_joaat("prop_foo")), Some(&rage_joaat("vehshare")));
    }

    #[test]
    fn merge_txd_relationships_rejects_self_parent() {
        let mut out = HashMap::new();
        merge_txd_relationships(&mut out, &[rel("same", "same")]);
        assert!(out.is_empty());
    }

    fn rel(child: &str, parent: &str) -> rage_formats::TxdRelationship {
        rage_formats::TxdRelationship { child: child.to_string(), parent: parent.to_string() }
    }

    #[test]
    fn resolution_order_tries_archetype_txd_then_own_stem_hash() {
        let mut index = GameIndex::default();
        index.archetype_txd.insert(100, 200);

        let order = index.resolution_order(100);
        assert_eq!(order, vec![200, 100]);

        // No archetype known: falls back to the stem hash alone.
        let order = index.resolution_order(999);
        assert_eq!(order, vec![999]);
    }

    #[test]
    fn resolution_order_walks_parent_chain_with_cycle_guard() {
        let mut index = GameIndex::default();
        index.parent_txds.insert(1, 2);
        index.parent_txds.insert(2, 1); // cycle

        let order = index.resolution_order(1);
        // 1 (self), 2 (parent), then the cycle back to 1 is rejected.
        assert_eq!(order, vec![1, 2]);
    }

    #[test]
    fn resolution_order_walks_the_archetype_txds_chain_not_the_stems() {
        // Regression test: the chain must be walked from the archetype's
        // resolved dictionary (200), not from the stem hash (100) that
        // happens to be pushed last — a prior version seeded the walk from
        // `order.last()` and so never found this parent at all.
        let mut index = GameIndex::default();
        index.archetype_txd.insert(100, 200);
        index.parent_txds.insert(200, 300);

        let order = index.resolution_order(100);
        assert_eq!(order, vec![200, 300, 100]);
    }

    #[test]
    fn resolution_order_merges_both_chains_without_duplicates() {
        let mut index = GameIndex::default();
        index.archetype_txd.insert(100, 200);
        index.parent_txds.insert(200, 400);
        index.parent_txds.insert(100, 400); // same parent as the archetype chain

        let order = index.resolution_order(100);
        assert_eq!(order, vec![200, 400, 100]);
    }

    #[test]
    fn resolution_order_stops_at_the_hop_limit() {
        let mut index = GameIndex::default();
        for i in 0..200u32 {
            index.parent_txds.insert(i, i + 1);
        }

        let order = index.resolution_order(0);
        assert!(order.len() <= 65, "expected the walk to stop at the hop limit, got {} entries", order.len());
    }

    // ─── Archive load-order ranking (issue #6) ─────────────────────────────

    #[test]
    fn archive_tier_classifies_base_archives() {
        assert_eq!(archive_tier(Path::new(r"C:\game\x64a.rpf")), ArchiveTier::Base);
        assert_eq!(archive_tier(Path::new(r"C:\game\x64\audio\sfx.rpf")), ArchiveTier::Base);
        assert_eq!(archive_tier(Path::new("C:/game/common.rpf")), ArchiveTier::Base);
    }

    #[test]
    fn archive_tier_classifies_update_archives() {
        assert_eq!(archive_tier(Path::new(r"C:\game\update\update.rpf")), ArchiveTier::Update);
        assert_eq!(archive_tier(Path::new(r"C:\game\update\update2.rpf")), ArchiveTier::Update);
        assert_eq!(archive_tier(Path::new("C:/game/update/x64/patch/data.rpf")), ArchiveTier::Update);
    }

    #[test]
    fn archive_tier_classifies_dlc_archives_over_update() {
        // Every DLC archive lives under .../update/..., so the DLC check
        // must win even though the path also matches "update".
        assert_eq!(
            archive_tier(Path::new(r"C:\game\update\x64\dlcpacks\mpheist\dlc.rpf")),
            ArchiveTier::Dlc("mpheist".to_string())
        );
        assert_eq!(
            archive_tier(Path::new("C:/game/update/x64/dlcpacks/mpheist4/dlc2.rpf")),
            ArchiveTier::Dlc("mpheist4".to_string())
        );
    }

    #[test]
    fn dlc_pack_name_extracts_the_directory_after_dlcpacks() {
        assert_eq!(dlc_pack_name("c:/game/update/x64/dlcpacks/mpheist/dlc.rpf"), Some("mpheist".to_string()));
        assert_eq!(dlc_pack_name("c:/game/update/x64/dlcpacks/mptuner/dlc1.rpf"), Some("mptuner".to_string()));
        assert_eq!(dlc_pack_name("c:/game/x64a.rpf"), None);
        assert_eq!(dlc_pack_name("c:/game/update/x64/dlcpacks/"), None); // no pack name at all
    }

    #[test]
    fn rank_dlc_packs_uses_setup2_order_with_dlclist_position_as_tiebreak() {
        // The real head of this install's dlclist.xml/setup2.xml data
        // (VIRUXE/rpf-cli#6): dlclist lists mpheist, mppatchesng,
        // patchday1ng, patchday2ng, mpchristmas2 in that order; their own
        // setup2.xml <order> values are 10, 0, -1, 2, 9 respectively.
        let packs = vec![
            ("mpheist".to_string(), 10),
            ("mppatchesng".to_string(), 0),
            ("patchday1ng".to_string(), -1),
            ("patchday2ng".to_string(), 2),
            ("mpchristmas2".to_string(), 9),
        ];
        let ranks = rank_dlc_packs(packs);
        let mut by_rank: Vec<&str> = ranks.keys().map(|s| s.as_str()).collect();
        by_rank.sort_by_key(|name| ranks[*name]);
        assert_eq!(by_rank, vec!["patchday1ng", "mppatchesng", "patchday2ng", "mpchristmas2", "mpheist"]);
    }

    #[test]
    fn rank_dlc_packs_stable_sort_breaks_ties_by_dlclist_position() {
        // Two packs with the same `order` keep their dlclist.xml order
        // (mirrors OrderBy's stability), rather than being reordered.
        let packs = vec![
            ("first_in_dlclist".to_string(), 5),
            ("second_in_dlclist".to_string(), 5),
        ];
        let ranks = rank_dlc_packs(packs);
        assert!(ranks["first_in_dlclist"] < ranks["second_in_dlclist"]);
    }

    #[test]
    fn archive_tier_rank_orders_base_before_update_before_dlc() {
        assert!(ArchiveTier::Base.rank() < ArchiveTier::Update.rank());
        assert!(ArchiveTier::Update.rank() < ArchiveTier::Dlc("x".to_string()).rank());
    }

    #[test]
    fn a_dlc_pack_missing_from_the_ranking_sorts_after_every_ranked_pack() {
        // Mirrors GameIndex::build's fallback: dlc_rank.get(name).unwrap_or(u32::MAX).
        let ranks = rank_dlc_packs(vec![("ranked".to_string(), 0)]);
        let unranked_fallback = ranks.get("unranked_pack").copied().unwrap_or(u32::MAX);
        assert!(unranked_fallback > *ranks.get("ranked").unwrap());
    }

    #[test]
    fn full_archive_ordering_sorts_base_then_update_then_ranked_dlc() {
        let dlc_rank: HashMap<String, u32> =
            [("mppatchesng".to_string(), 0u32), ("mpheist".to_string(), 1)].into_iter().collect();

        let mut archives = vec![
            PathBuf::from(r"C:\game\update\x64\dlcpacks\mpheist\dlc.rpf"),
            PathBuf::from(r"C:\game\x64w.rpf"),
            PathBuf::from(r"C:\game\update\update.rpf"),
            PathBuf::from(r"C:\game\update\x64\dlcpacks\mppatchesng\dlc.rpf"),
            PathBuf::from(r"C:\game\x64a.rpf"),
        ];
        archives.sort_by_key(|p| {
            let tier = archive_tier(p);
            let rank = match &tier {
                ArchiveTier::Dlc(name) => dlc_rank.get(name).copied().unwrap_or(u32::MAX),
                _ => 0,
            };
            (tier.rank(), rank, p.clone())
        });

        assert_eq!(
            archives,
            vec![
                PathBuf::from(r"C:\game\x64a.rpf"),
                PathBuf::from(r"C:\game\x64w.rpf"),
                PathBuf::from(r"C:\game\update\update.rpf"),
                PathBuf::from(r"C:\game\update\x64\dlcpacks\mppatchesng\dlc.rpf"),
                PathBuf::from(r"C:\game\update\x64\dlcpacks\mpheist\dlc.rpf"),
            ]
        );
    }
}
