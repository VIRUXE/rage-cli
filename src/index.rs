// A cached, game-wide index that lets `screenshot` resolve a drawable's
// external texture dictionary the way the game itself does, instead of the
// single same-stem guess in `resources::load_texture_dictionary`.
//
// This follows CodeWalker's `Renderer.TryGetRenderable` resolution order: an
// archetype's `.ytyp` names a texture dictionary by hash, that hash (and its
// parent-dictionary chain, from `gtxd.meta`/`gtxd.ymt`/`mph4_gtxd.ymt`/
// `vehicles.meta`) is resolved to `.ytd`s by a game-wide name index, and a
// texture still missing after all of that falls back to the two "resident"
// dictionaries the game always keeps loaded (`mapdetail.ytd`, `vehshare.ytd`).

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rage_formats::{parse_txd_relationships, parse_ymap_entities, parse_ytd, parse_ytyp, rage_joaat, Vec3};
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

/// The built index: enough to resolve `<archetype name hash>` (a drawable's
/// file stem) to the `.ytd` bytes that hold its textures.
#[derive(Debug, Default)]
pub struct GameIndex {
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
}

const RESIDENT_DICTS: [&str; 2] = ["mapdetail", "vehshare"];

/// CodeWalker's `GameFileCache.InitGtxds` matches these names exactly
/// (`entry.NameLower == "..."`), not as a suffix — a filename like
/// `dlc_gtxd.ymt` is deliberately not picked up, matching the reference.
const TXD_RELATIONSHIP_FILES: [&str; 4] = ["gtxd.ymt", "gtxd.meta", "mph4_gtxd.ymt", "vehicles.meta"];

impl GameIndex {
    /// Builds the index by walking every `.rpf` under `game_root`,
    /// descending into nested archives. This decompresses every `.ytyp` and
    /// the two resident dictionaries, but only reads the directory listing
    /// of everything else — no other `.ytd` is decoded.
    ///
    /// `archives` is re-ranked base -> update -> DLC (see `archive_tier` and
    /// `dlc_load_order`) before indexing, matching the order the game itself
    /// mounts them in; `collect_archives`'s own alphabetical order is left
    /// untouched, since `rpf search` still wants that.
    pub fn build(game_root: &Path, keys: Option<&GtaKeys>) -> Result<Self> {
        let archives = ranked_archives(game_root, keys)?;

        let mut index = GameIndex::default();
        for archive_path in &archives {
            let archive = match Archive::open(archive_path, keys) {
                Ok(a) => a,
                Err(e) => { eprintln!("index: skipping {}: {}", archive_path.display(), e); continue; }
            };
            if archive.require_keys(keys).is_err() {
                eprintln!("index: skipping {} (needs keys)", archive_path.display());
                continue;
            }
            index_archive(&archive, archive_path, &[], keys, &mut index);
        }

        Ok(index)
    }

    /// Reads the raw bytes of an already-located entry, descending through
    /// any nested archives on the way.
    pub fn load_bytes(&self, loc: &EntryLoc, keys: Option<&GtaKeys>) -> Result<Vec<u8>> {
        let mut archive = Archive::open(&loc.top_archive, keys)?;
        archive.require_keys(keys)?;

        for nested_path in &loc.nested_rpfs {
            let file = archive
                .find_file(nested_path)
                .with_context(|| format!("'{}' not found in '{}'", nested_path, loc.top_archive.display()))?;
            // `Archive::from_bytes`'s `name` isn't cosmetic: RPF7's NG
            // decryption selects its per-archive key from this name (and
            // the TOC length), so it must be the archive's own bare file
            // name — the same value `file.name` already is elsewhere in
            // this crate (`search_recursive`, `extract_recursive`) — not
            // its full path within the parent, or the TOC decrypts to
            // garbage and every entry name comes back as a placeholder.
            let bare_name = file.name.clone();
            let data = archive.extract(file, keys)
                .with_context(|| format!("failed to extract nested archive '{}'", nested_path))?;
            archive = Archive::from_bytes(data, &bare_name, keys)?;
        }

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

    /// `~/.rage-cli/index/<game build>/index.bin`, keyed the same way as the
    /// key cache (`keys::cache_entry_name`) so a game update never serves a
    /// stale index.
    pub fn cache_path(exe_path: &Path) -> Option<PathBuf> {
        let root = crate::paths::config_root()?.join("index");
        let meta = std::fs::metadata(exe_path).ok()?;
        let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        Some(root.join(keys::cache_entry_name(meta.len(), modified)).join("index.bin"))
    }

    pub fn load_cached(path: &Path) -> Result<Self> {
        let mut data = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut data)?;
        decode(&data)
    }

    pub fn save_cached(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::File::create(path)?.write_all(&encode(self))?;
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
        }
    }

    /// The one-line human summary shared by `rpf index build`/`info` and
    /// the on-demand build in `screenshot`.
    pub fn summary(&self) -> String {
        let s = self.stats();
        format!(
            "{} dictionaries, {} archetypes, {} resident textures, {} txd parent links, \
             {} interiors, {} interior placements, {} collision files",
            s.ytds, s.archetypes, s.resident_textures, s.parent_txds,
            s.interiors, s.interior_placements, s.collision_files
        )
    }

    /// Loads the cached game-wide index, building and caching it if there's
    /// none yet. `None` (with a warning, not an error — callers each have
    /// their own fallback) when there's no `--exe`/`GTAV_PATH` to find the
    /// game directory from, or the build itself fails.
    pub fn load_or_build(exe: Option<&Path>, keys: Option<&GtaKeys>) -> Option<Self> {
        let exe = exe?;
        let exe_path = match crate::keys::resolve_exe(exe) {
            Ok(p) => p,
            Err(err) => { eprintln!("warning: couldn't resolve --exe for the game index: {err}"); return None; }
        };
        let game_root = exe_path.parent()?.to_path_buf();

        let cache_path = GameIndex::cache_path(&exe_path);

        if let Some(path) = &cache_path
            && path.is_file()
        {
            match GameIndex::load_cached(path) {
                Ok(index) => return Some(index),
                Err(err) => eprintln!("game index cache at {} is invalid ({err}); rebuilding", path.display()),
            }
        }

        println!("Building game index for {} (one-off; cached for next time)...", game_root.display());
        let index = match GameIndex::build(&game_root, keys) {
            Ok(index) => index,
            Err(err) => { eprintln!("warning: failed to build game index: {err}"); return None; }
        };
        println!("Game index: {}", index.summary());

        if let Some(path) = &cache_path
            && let Err(err) = index.save_cached(path)
        {
            eprintln!("warning: failed to cache game index: {err}");
        }

        Some(index)
    }
}

/// Sizes of each map in a [`GameIndex`], for `rpf index build`/`info` and
/// the on-demand build notice in `screenshot`.
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexStats {
    pub ytds: usize,
    pub archetypes: usize,
    pub resident_textures: usize,
    pub parent_txds: usize,
    pub interiors: usize,
    pub interior_placements: usize,
    pub collision_files: usize,
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

fn index_archive(archive: &Archive, archive_path: &Path, nested_rpfs: &[String], keys: Option<&GtaKeys>, out: &mut GameIndex) {
    for file in archive.list_files() {
        let name_lower = file.name.to_lowercase();

        if name_lower.ends_with(".rpf") {
            let Ok(data) = archive.extract(file, keys) else { continue };
            let Ok(nested) = Archive::from_bytes(data, &file.name, keys) else { continue };
            let mut chain = nested_rpfs.to_vec();
            chain.push(file.path.clone());
            index_archive(&nested, archive_path, &chain, keys, out);
            continue;
        }

        if TXD_RELATIONSHIP_FILES.contains(&name_lower.as_str()) {
            match archive.extract(file, keys) {
                Ok(data) => match parse_txd_relationships(&data) {
                    Ok(rels) => merge_txd_relationships(&mut out.parent_txds, &rels),
                    Err(err) => log::debug!("index: failed to parse '{}': {err}", file.path),
                },
                Err(err) => log::debug!("index: failed to extract '{}': {err}", file.path),
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
            out.ytd_by_name.insert(rage_joaat(&stem), loc());

            if RESIDENT_DICTS.contains(&stem.as_str())
                && let Ok(data) = archive.extract(file, keys)
                && let Ok(textures) = parse_ytd(&data)
            {
                let owner_hash = rage_joaat(&stem);
                for tex in textures {
                    // The stored hash block falls back to 0 when absent, so
                    // this is indexed under both that hash and the hashed
                    // lowercase name to make the lookup unmissable — the
                    // caller (a still-missing texture name, post-render)
                    // only ever has the name.
                    if tex.name_hash != 0 {
                        out.resident_textures.insert(tex.name_hash, owner_hash);
                    }
                    out.resident_textures.insert(rage_joaat(&tex.name.to_lowercase()), owner_hash);
                }
            }
        } else if name_lower.ends_with(".ytyp")
            && let Ok(data) = archive.extract(file, keys)
            && let Ok(ytyp) = parse_ytyp(&data)
        {
            for a in ytyp.archetypes {
                if a.texture_dict_hash != 0 {
                    out.archetype_txd.insert(a.name_hash, a.texture_dict_hash);
                }
                out.archetype_box.insert(a.name_hash, (a.bb_min, a.bb_max));
                if a.is_mlo {
                    out.mlo_ytyp.insert(a.name_hash, loc());
                }
            }
        } else if name_lower.ends_with(".ybn") {
            // Name only: a `.ybn` is never parsed while indexing, so this
            // branch costs nothing beyond the directory listing already read.
            out.ybn_by_name.insert(rage_joaat(&stem), loc());
        } else if name_lower.ends_with(".ymap") {
            // The one branch that reads a file it would otherwise skip. Only
            // the entity list is decoded (`parse_ymap_entities`), and only
            // the `CMloInstanceDef`s in it are kept — enough to answer "which
            // .ymap places this interior" without holding any of the map.
            match archive.extract(file, keys) {
                Ok(data) => match parse_ymap_entities(&data) {
                    Ok(entities) => {
                        // One entry per (archetype, .ymap): a map that places
                        // the same interior twice is still read once, and
                        // `plot` re-reads every instance out of it anyway.
                        let mut seen = std::collections::HashSet::new();
                        for entity in entities.iter().filter(|e| e.is_mlo_instance) {
                            if seen.insert(entity.archetype_hash) {
                                record_mlo_instance(&mut out.mlo_instances, entity.archetype_hash, loc());
                            }
                        }
                    }
                    Err(err) => log::debug!("index: failed to parse '{}': {err}", file.path),
                },
                Err(err) => log::debug!("index: failed to extract '{}': {err}", file.path),
            }
        }
    }
}

/// Files one `.ymap`'s placement of an interior, last-wins per map file —
/// the same DLC-overrides-base rule `ytd_by_name` and `archetype_txd`
/// follow, which for a *list* means replacing the matching entry in place
/// rather than appending beside it. A DLC that ships its own copy of a
/// base-game map would otherwise leave two entries, and `plot` (which draws
/// the first) would draw the superseded one.
///
/// Two locations are the same map when their file names match, ignoring
/// case: a DLC override keeps the map's name but rarely its full path
/// (`x64/levels/...` in the pack instead of `levels/gta5/...` in the base).
/// Distinct maps that place the same interior keep their own entries.
fn record_mlo_instance(out: &mut HashMap<u32, Vec<EntryLoc>>, archetype: u32, loc: EntryLoc) {
    let entries = out.entry(archetype).or_default();
    match entries.iter_mut().find(|e| ymap_file_name(&e.inner_path) == ymap_file_name(&loc.inner_path)) {
        Some(existing) => *existing = loc,
        None => entries.push(loc),
    }
}

/// The last `/`-separated segment of an entry path, lowercased.
fn ymap_file_name(inner_path: &str) -> String {
    inner_path.rsplit(['/', '\\']).next().unwrap_or(inner_path).to_lowercase()
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
// Bumped from 1 to 2: the on-disk layout was unchanged (the parent_txds
// section already existed and was already last), but the cache path is
// keyed only on the GTA5 executable's size+mtime, not on any tool version
// — so without a bump, every existing cache (written with an always-empty
// parent_txds) would be served straight to the new resolver and the
// parent chain would be silently inert until someone ran `rpf index
// clear`.
//
// Bumped from 2 to 3 for the same reason, on the same key: `GameIndex::
// build` now scans archives base -> update -> DLC instead of alphabetically
// (see `archive_tier`/`dlc_load_order`), which changes which archive wins
// in `ytd_by_name`, `archetype_txd`, `resident_textures` (last-wins) and
// `parent_txds` (first-wins). The layout is still unchanged; an old
// index.bin just reflects the wrong scan order and must be rebuilt.
// 4: archetype bounding boxes added (`archetype_box`).
// 5: MLO ytyp/ymap locations and ybn names for `rage plot`.
const FORMAT_VERSION: u32 = 5;

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

fn encode(index: &GameIndex) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u32(&mut buf, MAGIC);
    write_u32(&mut buf, FORMAT_VERSION);

    write_u32(&mut buf, index.ytd_by_name.len() as u32);
    for (hash, loc) in &index.ytd_by_name {
        write_u32(&mut buf, *hash);
        write_loc(&mut buf, loc);
    }

    write_u32(&mut buf, index.archetype_txd.len() as u32);
    for (k, v) in &index.archetype_txd {
        write_u32(&mut buf, *k);
        write_u32(&mut buf, *v);
    }

    write_u32(&mut buf, index.archetype_box.len() as u32);
    for (k, (lo, hi)) in &index.archetype_box {
        write_u32(&mut buf, *k);
        for f in [lo.x, lo.y, lo.z, hi.x, hi.y, hi.z] {
            buf.extend_from_slice(&f.to_le_bytes());
        }
    }

    write_u32(&mut buf, index.resident_textures.len() as u32);
    for (k, v) in &index.resident_textures {
        write_u32(&mut buf, *k);
        write_u32(&mut buf, *v);
    }

    write_u32(&mut buf, index.parent_txds.len() as u32);
    for (k, v) in &index.parent_txds {
        write_u32(&mut buf, *k);
        write_u32(&mut buf, *v);
    }

    write_u32(&mut buf, index.mlo_ytyp.len() as u32);
    for (hash, loc) in &index.mlo_ytyp {
        write_u32(&mut buf, *hash);
        write_loc(&mut buf, loc);
    }

    write_u32(&mut buf, index.mlo_instances.len() as u32);
    for (hash, locs) in &index.mlo_instances {
        write_u32(&mut buf, *hash);
        write_u32(&mut buf, locs.len() as u32);
        for loc in locs {
            write_loc(&mut buf, loc);
        }
    }

    write_u32(&mut buf, index.ybn_by_name.len() as u32);
    for (hash, loc) in &index.ybn_by_name {
        write_u32(&mut buf, *hash);
        write_loc(&mut buf, loc);
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

    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        let bytes = self.data.get(self.pos..self.pos + len).context("index: truncated (string)")?;
        self.pos += len;
        String::from_utf8(bytes.to_vec()).context("index: invalid UTF-8")
    }
}

fn read_loc(c: &mut Cursor) -> Result<EntryLoc> {
    let top_archive = PathBuf::from(c.string()?);
    let nested_count = c.u32()? as usize;
    // The count comes from the cache file, which may be truncated or
    // garbled: reserve a sane amount and let the reads below fail with
    // "truncated" rather than aborting the process on a huge allocation.
    let mut nested_rpfs = Vec::with_capacity(nested_count.min(1024));
    for _ in 0..nested_count {
        nested_rpfs.push(c.string()?);
    }
    let inner_path = c.string()?;
    Ok(EntryLoc { top_archive, nested_rpfs, inner_path })
}

fn decode(data: &[u8]) -> Result<GameIndex> {
    let mut c = Cursor { data, pos: 0 };
    if c.u32()? != MAGIC {
        bail!("index: bad magic");
    }
    let version = c.u32()?;
    if version != FORMAT_VERSION {
        bail!("index: unsupported format version {version}");
    }

    let mut index = GameIndex::default();

    let ytd_count = c.u32()? as usize;
    for _ in 0..ytd_count {
        let hash = c.u32()?;
        let loc = read_loc(&mut c)?;
        index.ytd_by_name.insert(hash, loc);
    }

    let archetype_count = c.u32()? as usize;
    for _ in 0..archetype_count {
        let k = c.u32()?;
        let v = c.u32()?;
        index.archetype_txd.insert(k, v);
    }

    let box_count = c.u32()? as usize;
    for _ in 0..box_count {
        let k = c.u32()?;
        let mut f = [0f32; 6];
        for v in &mut f { *v = c.f32()?; }
        index.archetype_box.insert(k, (Vec3::new(f[0], f[1], f[2]), Vec3::new(f[3], f[4], f[5])));
    }

    let resident_count = c.u32()? as usize;
    for _ in 0..resident_count {
        let k = c.u32()?;
        let v = c.u32()?;
        index.resident_textures.insert(k, v);
    }

    let parent_count = c.u32()? as usize;
    for _ in 0..parent_count {
        let k = c.u32()?;
        let v = c.u32()?;
        index.parent_txds.insert(k, v);
    }

    let mlo_ytyp_count = c.u32()? as usize;
    for _ in 0..mlo_ytyp_count {
        let hash = c.u32()?;
        let loc = read_loc(&mut c)?;
        index.mlo_ytyp.insert(hash, loc);
    }

    let mlo_instance_count = c.u32()? as usize;
    for _ in 0..mlo_instance_count {
        let hash = c.u32()?;
        let locs_count = c.u32()? as usize;
        let mut locs = Vec::with_capacity(locs_count.min(1024));
        for _ in 0..locs_count {
            locs.push(read_loc(&mut c)?);
        }
        index.mlo_instances.insert(hash, locs);
    }

    let ybn_count = c.u32()? as usize;
    for _ in 0..ybn_count {
        let hash = c.u32()?;
        let loc = read_loc(&mut c)?;
        index.ybn_by_name.insert(hash, loc);
    }

    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_encode_decode() {
        let mut index = GameIndex::default();
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

        let bytes = encode(&index);
        let decoded = decode(&bytes).expect("should decode");

        assert_eq!(decoded.ytd_by_name.get(&1), index.ytd_by_name.get(&1));
        assert_eq!(decoded.archetype_txd, index.archetype_txd);
        assert_eq!(decoded.resident_textures, index.resident_textures);
        assert_eq!(decoded.parent_txds, index.parent_txds);
        assert_eq!(decoded.archetype_box, index.archetype_box);
        assert_eq!(decoded.mlo_ytyp, index.mlo_ytyp);
        assert_eq!(decoded.mlo_instances, index.mlo_instances);
        assert_eq!(decoded.ybn_by_name, index.ybn_by_name);
    }

    fn loc(inner: &str) -> EntryLoc {
        EntryLoc {
            top_archive: PathBuf::from("C:/game/x64a.rpf"),
            nested_rpfs: vec!["levels/gta5/interiors.rpf".to_string()],
            inner_path: inner.to_string(),
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

    /// A DLC that ships its own copy of a base-game `.ymap` must *replace*
    /// the base entry, not sit beside it: `plot` draws the first placement,
    /// so a stale base-game copy left in front of the DLC one would be the
    /// exact opposite of the last-wins rule every other map follows.
    #[test]
    fn a_later_archive_replaces_the_same_ymap_rather_than_adding_it() {
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
        record_mlo_instance(&mut out, 7, base);
        record_mlo_instance(&mut out, 7, elsewhere.clone());
        record_mlo_instance(&mut out, 7, dlc.clone());

        assert_eq!(out[&7], vec![dlc, elsewhere], "the DLC copy should replace the base one in place");
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
        let mut index = GameIndex::default();
        index.mlo_ytyp.insert(1, loc("i.ytyp"));
        index.mlo_instances.insert(1, vec![loc("a.ymap"), loc("b.ymap")]);
        index.ybn_by_name.insert(2, loc("i.ybn"));

        let summary = index.summary();
        assert!(summary.contains("1 interiors"), "{summary}");
        assert!(summary.contains("2 interior placements"), "{summary}");
        assert!(summary.contains("1 collision files"), "{summary}");
    }

    #[test]
    fn rejects_bad_magic() {
        let bytes = vec![0u8; 8];
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn rejects_old_format_version() {
        let mut bytes = Vec::new();
        write_u32(&mut bytes, MAGIC);
        write_u32(&mut bytes, 1); // the pre-parent-chain format
        assert!(decode(&bytes).is_err());
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
