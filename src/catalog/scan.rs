//! `catalog build`: walk the install in load order and record every
//! drawable, fragment, drawable-dictionary entry and texture.
//!
//! Archives are scanned one at a time, in load order, with every entry of an
//! archive (nested archives included) parsed in parallel. Each archive's rows
//! go to the single SQLite writer as one transaction, so an interrupted
//! build resumes where it stopped: an archive whose size, modification time,
//! leading bytes and scanner version all match its row is skipped.

use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::{params, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rage_formats::{
    parse_txd_relationships, parse_ydd, parse_ydr, parse_yft, parse_ytd_system, parse_ytyp, rage_joaat,
    resource_version_from_flags, ytd_chain_size, Drawable, NameTable, YtdTexture,
};
use rpf_archive::RpfEntryKind;

use super::tokens::{tokenize_name, tokenize_path};
use super::{asset_key, hex8, human_duration, now_secs, thousands, Catalog, Kind};
use crate::index::{archive_tier, ranked_archives, TXD_RELATIONSHIP_FILES};
use crate::rpf::{Archive, FileRef, GtaKeys};

/// Bumped when what a scan records changes, so every archive is rescanned.
pub const SCANNER_VERSION: i64 = 1;
/// How much of each archive's head is hashed to notice a changed TOC.
const HEAD_BYTES: u64 = 1 << 20;

pub struct BuildOptions {
    pub game_root: PathBuf,
    pub exe: Option<PathBuf>,
    /// Kinds to record; empty means all.
    pub kinds: Vec<Kind>,
    /// Load tiers to scan: any of `base`, `update`, `dlc`; empty means all.
    pub scope: Vec<String>,
    pub full: bool,
    pub jobs: Option<usize>,
    pub names: Vec<PathBuf>,
    pub report: Option<PathBuf>,
    pub quiet: bool,
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub archive: String,
    pub nested: String,
    pub path: String,
    pub stage: &'static str,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct BuildSummary {
    pub archives_total: usize,
    pub scanned: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub needs_keys: usize,
    pub failed: usize,
    pub items_by_kind: BTreeMap<String, u64>,
    pub failures: Vec<Failure>,
    pub elapsed: Duration,
    pub phases: Vec<(&'static str, Duration)>,
    pub report_path: PathBuf,
    /// Annotations reattached to rebuilt assets.
    pub rematched: usize,
}

/// Which container file types a set of kinds needs opened.
#[derive(Clone, Copy)]
struct Wanted {
    ydr: bool,
    ydd: bool,
    yft: bool,
    ytd: bool,
}

impl Wanted {
    fn from(kinds: &[Kind]) -> Self {
        let all = kinds.is_empty();
        let has = |k: Kind| all || kinds.contains(&k);
        Wanted {
            ydr: has(Kind::Drawable),
            ydd: has(Kind::Dictionary) || has(Kind::DdEntry),
            yft: has(Kind::Fragment),
            ytd: has(Kind::Txd) || has(Kind::Texture),
        }
    }
}

// ─── Rows produced by a scan ────────────────────────────────────────────────

#[derive(Default, Clone)]
struct DrawableStats {
    bb_min: [f32; 3],
    bb_max: [f32; 3],
    radius: f32,
    computed: bool,
    lods: [f32; 4],
    triangles: u64,
    lod_count: u32,
    shaders: u32,
    embedded: u32,
    diffuse: Option<String>,
}

#[derive(Clone)]
struct TexStats {
    width: u16,
    height: u16,
    depth: u16,
    levels: u8,
    format: String,
    bytes: u64,
    embedded: bool,
}

struct ItemRow {
    kind: Kind,
    hash: u32,
    name: Option<String>,
    member: Option<String>,
    /// Index of the parent row within the same entry's rows.
    parent: Option<usize>,
    asset_key: String,
    txd_hash: Option<u32>,
    file_size: Option<u32>,
    mem_size: Option<u32>,
    resource_version: Option<u32>,
    source_sha256: Option<String>,
    drawable: Option<DrawableStats>,
    texture: Option<TexStats>,
    parse_error: Option<String>,
    refs: Vec<(u32, String, u32, u32)>,
}

impl ItemRow {
    fn new(kind: Kind, hash: u32, name: Option<String>) -> Self {
        ItemRow {
            kind,
            hash,
            name,
            member: None,
            parent: None,
            asset_key: asset_key(kind, hash, None),
            txd_hash: None,
            file_size: None,
            mem_size: None,
            resource_version: None,
            source_sha256: None,
            drawable: None,
            texture: None,
            parse_error: None,
            refs: Vec::new(),
        }
    }
}

struct ArchetypeRow {
    ytyp_key: String,
    name_hash: u32,
    name: Option<String>,
    bb_min: [f32; 3],
    bb_max: [f32; 3],
    lod_dist: f32,
    txd_hash: u32,
    drawable_dictionary_hash: u32,
    asset_name_hash: u32,
    asset_type: u32,
    is_mlo: bool,
}

/// Everything one archive entry contributed.
#[derive(Default)]
struct EntryOut {
    nested: String,
    inner_path: String,
    entry_name: String,
    items: Vec<ItemRow>,
    archetypes: Vec<ArchetypeRow>,
    txd_parents: Vec<(u32, u32)>,
    failure: Option<Failure>,
}

enum ScanStatus {
    Ok,
    NeedsKeys,
    Failed(String),
}

struct ArchiveScan {
    archive_id: i64,
    status: ScanStatus,
    entries: usize,
    outs: Vec<EntryOut>,
    failures: Vec<Failure>,
    size: u64,
    mtime: i64,
    head: String,
}

// ─── Build ──────────────────────────────────────────────────────────────────

struct ArchivePlan {
    id: i64,
    path: PathBuf,
    rel: String,
    size: u64,
    mtime: i64,
    head: String,
    changed: bool,
}

pub fn build(cat: &mut Catalog, keys: Option<&GtaKeys>, opts: &BuildOptions) -> Result<BuildSummary> {
    let started = Instant::now();
    let started_secs = now_secs();
    let mut summary = BuildSummary::default();
    let mut phase = Instant::now();

    // 1. Rank every archive the way the game mounts them.
    let ranked = ranked_archives(&opts.game_root, keys)?;
    let in_scope = |p: &Path| opts.scope.is_empty() || opts.scope.iter().any(|s| s == archive_tier(p).tier_name());
    summary.archives_total = ranked.iter().filter(|p| in_scope(p)).count();
    summary.phases.push(("rank", phase.elapsed()));
    phase = Instant::now();

    // 2. Decide what changed.
    let mut known: HashMap<String, (i64, i64, i64, String, i64, String)> = HashMap::new();
    {
        let mut stmt = cat.conn.prepare("SELECT id, path, size, mtime, head_sha256, scanner, status FROM archives")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(1)?, (r.get(0)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
        })?;
        for row in rows {
            let (path, v) = row?;
            known.insert(path, v);
        }
    }

    let mut plans: Vec<ArchivePlan> = Vec::new();
    let mut rank_changed = false;
    {
        let tx = cat.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (rank, path) in ranked.iter().enumerate() {
            let key = path.to_string_lossy().to_string();
            let rel = rel_path(&opts.game_root, path);
            let tier = archive_tier(path);
            let meta = std::fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
            let size = meta.len();
            let mtime = mtime_secs(&meta);
            let prior = known.remove(&key);
            if !in_scope(path) {
                // Out of scope this run: keep its rows, but keep its rank current.
                if let Some((id, ..)) = prior {
                    tx.execute("UPDATE archives SET load_rank = ?2 WHERE id = ?1", params![id, rank as i64])?;
                }
                continue;
            }
            let head = head_sha256(path).unwrap_or_default();
            let unchanged = matches!(&prior, Some((_, s, m, h, v, st))
                if *s == size as i64 && *m == mtime && *h == head && *v == SCANNER_VERSION && st == "ok");
            let id = match prior {
                Some((id, ..)) => {
                    let changed = tx.execute(
                        "UPDATE archives SET rel_path = ?2, tier = ?3, dlc_pack = ?4, load_rank = ?5 WHERE id = ?1
                         AND (rel_path IS NOT ?2 OR tier IS NOT ?3 OR dlc_pack IS NOT ?4 OR load_rank IS NOT ?5)",
                        params![id, rel, tier.tier_name(), tier.dlc_pack(), rank as i64],
                    )?;
                    rank_changed |= changed > 0;
                    id
                }
                None => {
                    tx.execute(
                        "INSERT INTO archives(path, rel_path, tier, dlc_pack, load_rank, size, mtime, head_sha256, scanner, status, scanned_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', 0, 'pending', 0)",
                        params![key, rel, tier.tier_name(), tier.dlc_pack(), rank as i64, size as i64, mtime],
                    )?;
                    tx.last_insert_rowid()
                }
            };
            let changed = opts.full || !unchanged;
            plans.push(ArchivePlan { id, path: path.clone(), rel, size, mtime, head, changed });
        }
        // Whatever is left in `known` is no longer on disk.
        for (path, (id, ..)) in &known {
            if !Path::new(path).exists() {
                tx.execute("DELETE FROM archives WHERE id = ?1", [id])?;
                summary.removed += 1;
            }
        }
        tx.commit()?;
    }
    let to_scan: Vec<&ArchivePlan> = plans.iter().filter(|p| p.changed).collect();
    summary.unchanged = plans.len() - to_scan.len();
    if !opts.quiet {
        eprintln!(
            "{} archive(s) in scope: {} unchanged, {} to scan{}",
            plans.len(),
            summary.unchanged,
            to_scan.len(),
            if summary.removed > 0 { format!(", {} removed", summary.removed) } else { String::new() }
        );
    }
    summary.phases.push(("plan", phase.elapsed()));
    phase = Instant::now();

    // 3. Scan changed archives in parallel, write each one as it lands.
    let names = crate::names::load(&opts.names, None)?;
    let wanted = Wanted::from(&opts.kinds);
    let pool = {
        let mut b = rayon::ThreadPoolBuilder::new();
        if let Some(j) = opts.jobs {
            b = b.num_threads(j.max(1));
        }
        b.build().context("failed to start the scan thread pool")?
    };

    let total = to_scan.len();
    let (sender, receiver) = std::sync::mpsc::sync_channel::<ArchiveScan>(1);
    let quiet = opts.quiet;
    let write_result: Result<()> = std::thread::scope(|scope| {
        let names = &names;
        scope.spawn(move || {
            pool.install(|| {
                for (n, plan) in to_scan.iter().enumerate() {
                    if !quiet {
                        progress(n + 1, total, &plan.rel);
                    }
                    let scan = scan_archive(plan, keys, wanted, names);
                    if sender.send(scan).is_err() {
                        break;
                    }
                }
            });
        });
        for scan in receiver {
            match &scan.status {
                ScanStatus::Ok => summary.scanned += 1,
                ScanStatus::NeedsKeys => summary.needs_keys += 1,
                ScanStatus::Failed(_) => summary.failed += 1,
            }
            summary.failures.extend(scan.failures.iter().cloned());
            for out in &scan.outs {
                if let Some(f) = &out.failure {
                    summary.failures.push(f.clone());
                }
            }
            let tx = cat.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            write_archive(&tx, &scan)?;
            tx.commit()?;
        }
        Ok(())
    });
    write_result?;
    if !quiet && total > 0 {
        eprint!("\r{:<78}\r", "");
    }
    summary.phases.push(("scan", phase.elapsed()));
    phase = Instant::now();

    // 4. Derived data: which copy the game loads, late-resolved names.
    let changed = summary.scanned + summary.needs_keys + summary.failed + summary.removed > 0 || rank_changed || opts.full;
    if changed {
        winner_pass(cat)?;
        summary.phases.push(("winners", phase.elapsed()));
        phase = Instant::now();
    }
    let renamed = names_pass(cat, &names)?;
    summary.phases.push(("names", phase.elapsed()));
    phase = Instant::now();
    summary.rematched = super::pack::rematch(cat, keys)?;
    summary.phases.push(("rematch", phase.elapsed()));
    if changed || renamed > 0 {
        phase = Instant::now();
        cat.conn.execute_batch("PRAGMA optimize;")?;
        summary.phases.push(("optimize", phase.elapsed()));
    }

    // 5. Counts, metadata, report.
    {
        let mut stmt = cat.conn.prepare("SELECT kind, count(*) FROM items GROUP BY kind")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (k, n) = row?;
            summary.items_by_kind.insert(k, n as u64);
        }
    }
    summary.elapsed = started.elapsed();
    record_meta(cat, opts, started_secs, summary.elapsed)?;
    summary.report_path = opts
        .report
        .clone()
        .unwrap_or_else(|| cat.path.with_file_name("catalog-report.json"));
    write_report(&summary, started_secs, cat)?;
    Ok(summary)
}

fn record_meta(cat: &Catalog, opts: &BuildOptions, started: i64, elapsed: Duration) -> Result<()> {
    cat.set_meta("scanner_version", &SCANNER_VERSION.to_string())?;
    cat.set_meta("game_root", &opts.game_root.to_string_lossy())?;
    cat.set_meta("built_at", &started.to_string())?;
    cat.set_meta("rage_version", env!("CARGO_PKG_VERSION"))?;
    cat.set_meta("last_build_elapsed_ms", &elapsed.as_millis().to_string())?;
    if let Some(exe) = &opts.exe {
        if let Some(key) = crate::keys::build_key(exe) {
            cat.set_meta("exe_key", &key)?;
        }
        if let Some(version) = crate::keys::exe_version(exe) {
            cat.set_meta("exe_version", &version)?;
            if let Some(build) = crate::keys::build_number(&version) {
                cat.set_meta("game_build", &build.to_string())?;
            }
        }
    }
    Ok(())
}

fn write_report(summary: &BuildSummary, started: i64, cat: &Catalog) -> Result<()> {
    let mut items = json::JsonValue::new_object();
    for (k, n) in &summary.items_by_kind {
        items[k.as_str()] = (*n).into();
    }
    let mut failures = json::JsonValue::new_array();
    for f in &summary.failures {
        let _ = failures.push(json::object! {
            archive: f.archive.clone(), nested: f.nested.clone(), path: f.path.clone(),
            stage: f.stage, error: f.error.clone(),
        });
    }
    let mut phases = json::JsonValue::new_object();
    for (name, d) in &summary.phases {
        phases[*name] = (d.as_millis() as u64).into();
    }
    let game_build: json::JsonValue = cat
        .meta("game_build")?
        .and_then(|b| b.parse::<u64>().ok())
        .map(Into::into)
        .unwrap_or(json::JsonValue::Null);
    let report = json::object! {
        schema: "rage-catalog-report/1",
        started: started,
        elapsed_ms: summary.elapsed.as_millis() as u64,
        phases_ms: phases,
        game_build: game_build,
        scanner_version: SCANNER_VERSION,
        archives: json::object! {
            total: summary.archives_total, scanned: summary.scanned, unchanged: summary.unchanged,
            removed: summary.removed, needs_keys: summary.needs_keys, failed: summary.failed,
        },
        items: items,
        failures: failures,
    };
    let tmp = summary.report_path.with_extension("json.tmp");
    std::fs::write(&tmp, report.pretty(2)).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &summary.report_path)
        .with_context(|| format!("failed to write {}", summary.report_path.display()))?;
    Ok(())
}

impl BuildSummary {
    /// The one-line human summary.
    pub fn line(&self) -> String {
        let counts: Vec<String> = Kind::ALL
            .iter()
            .filter_map(|k| self.items_by_kind.get(k.as_str()).map(|n| format!("{} {}", thousands(*n), plural(k.as_str(), *n))))
            .collect();
        let mut s = format!(
            "Catalogued {} archive(s) ({} unchanged) in {}: {}",
            self.archives_total,
            self.unchanged,
            human_duration(self.elapsed),
            if counts.is_empty() { "nothing".to_string() } else { counts.join(", ") }
        );
        if self.needs_keys > 0 {
            s.push_str(&format!("; {} archive(s) need keys (pass --exe or --keys)", self.needs_keys));
        }
        if self.failed > 0 {
            s.push_str(&format!("; {} archive(s) failed to open", self.failed));
        }
        if !self.failures.is_empty() {
            s.push_str(&format!("; {} entr{} failed to parse, listed in {}",
                self.failures.len(), if self.failures.len() == 1 { "y" } else { "ies" }, self.report_path.display()));
        }
        s
    }
}

fn plural(kind: &str, n: u64) -> String {
    let base = match kind {
        "dd_entry" => "dictionary entr",
        "dictionary" => "drawable dictionar",
        "txd" => "texture dictionar",
        other => other,
    };
    match (base.ends_with("entr") || base.ends_with("ar"), n == 1) {
        (true, true) => format!("{base}y"),
        (true, false) => format!("{base}ies"),
        (false, true) => base.to_string(),
        (false, false) => format!("{base}s"),
    }
}

/// One `\r`-overwritten stderr line per archive, like `index build`.
fn progress(n: usize, total: usize, rel: &str) {
    use std::io::Write;
    let label = if rel.len() > 50 { format!("...{}", &rel[rel.len() - 47..]) } else { rel.to_string() };
    eprint!("\r[{n:>3}/{total}] {label:<52}");
    let _ = std::io::stderr().flush();
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase()
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn head_sha256(path: &Path) -> Result<String> {
    let mut buf = Vec::with_capacity(HEAD_BYTES as usize);
    std::fs::File::open(path)?.take(HEAD_BYTES).read_to_end(&mut buf)?;
    Ok(hex_digest(&Sha256::digest(&buf)))
}

pub fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ─── Scanning one archive ───────────────────────────────────────────────────

struct Node {
    archive: Arc<Archive>,
    nested: Vec<String>,
}

fn scan_archive(plan: &ArchivePlan, keys: Option<&GtaKeys>, wanted: Wanted, names: &NameTable) -> ArchiveScan {
    let mut scan = ArchiveScan {
        archive_id: plan.id,
        status: ScanStatus::Ok,
        entries: 0,
        outs: Vec::new(),
        failures: Vec::new(),
        size: plan.size,
        mtime: plan.mtime,
        head: plan.head.clone(),
    };
    let archive = match Archive::open(&plan.path, keys) {
        Ok(a) => a,
        Err(e) => {
            scan.status = ScanStatus::Failed(e.to_string());
            return scan;
        }
    };
    if archive.require_keys(keys).is_err() {
        scan.status = ScanStatus::NeedsKeys;
        return scan;
    }

    // Open the whole nested tree first (cheap: TOCs only, stored archives
    // are windows onto the parent's mapping), then parse entries in parallel.
    let mut nodes: Vec<Node> = Vec::new();
    let mut stack = vec![Node { archive: Arc::new(archive), nested: Vec::new() }];
    while let Some(node) = stack.pop() {
        for file in node.archive.list_files() {
            if !file.name.to_lowercase().ends_with(".rpf") {
                continue;
            }
            let mut chain = node.nested.clone();
            chain.push(file.path.clone());
            match node.archive.open_nested(file, keys) {
                Ok(inner) => stack.push(Node { archive: Arc::new(inner), nested: chain }),
                Err(e) => scan.failures.push(Failure {
                    archive: plan.rel.clone(),
                    nested: node.nested.join("//"),
                    path: file.path.clone(),
                    stage: "open",
                    error: e.to_string(),
                }),
            }
        }
        nodes.push(node);
    }

    let mut work: Vec<(usize, FileRef)> = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        for file in node.archive.list_files() {
            scan.entries += 1;
            if wants_entry(&file.name, wanted) {
                work.push((i, file.clone()));
            }
        }
    }

    scan.outs = work
        .par_iter()
        .map(|(i, file)| {
            let node = &nodes[*i];
            scan_entry(&node.archive, file, &node.nested, &plan.rel, keys, wanted, names)
        })
        .collect();
    scan
}

fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

fn wants_entry(name: &str, wanted: Wanted) -> bool {
    let lower = name.to_ascii_lowercase();
    if TXD_RELATIONSHIP_FILES.contains(&lower.as_str()) {
        return true;
    }
    match extension(&lower).as_str() {
        "ytyp" => true,
        "ydr" => wanted.ydr,
        "ydd" => wanted.ydd,
        "yft" => wanted.yft,
        "ytd" => wanted.ytd,
        _ => false,
    }
}

fn scan_entry(
    archive: &Archive,
    file: &FileRef,
    nested: &[String],
    rel: &str,
    keys: Option<&GtaKeys>,
    wanted: Wanted,
    names: &NameTable,
) -> EntryOut {
    let lower = file.name.to_ascii_lowercase();
    let mut out = EntryOut {
        nested: nested.join("//"),
        inner_path: file.path.clone(),
        entry_name: lower.clone(),
        ..Default::default()
    };
    let fail = |stage: &'static str, e: &dyn std::fmt::Display| Failure {
        archive: rel.to_string(),
        nested: nested.join("//"),
        path: file.path.clone(),
        stage,
        error: e.to_string(),
    };
    let ext = extension(&lower);
    // Texture dictionaries only need their system section (names, sizes,
    // formats); everything else is read whole.
    let data = if ext == "ytd" {
        Vec::new()
    } else {
        match archive.extract(file, keys) {
            Ok(d) => d,
            Err(e) => {
                out.failure = Some(fail("extract", &e));
                return out;
            }
        }
    };

    if TXD_RELATIONSHIP_FILES.contains(&lower.as_str()) {
        match parse_txd_relationships(&data) {
            Ok(rels) => {
                out.txd_parents = rels
                    .iter()
                    .map(|r| (rage_joaat(&r.child.to_lowercase()), rage_joaat(&r.parent.to_lowercase())))
                    .collect();
            }
            Err(e) => out.failure = Some(fail("parse_txd", &e)),
        }
        return out;
    }

    if ext == "ytyp" {
        match parse_ytyp(&data) {
            Ok(ytyp) => {
                let ytyp_key = location(rel, nested, &file.path);
                out.archetypes = ytyp
                    .archetypes
                    .iter()
                    .map(|a| ArchetypeRow {
                        ytyp_key: ytyp_key.clone(),
                        name_hash: a.name_hash,
                        name: names.get(a.name_hash).map(str::to_string),
                        bb_min: [a.bb_min.x, a.bb_min.y, a.bb_min.z],
                        bb_max: [a.bb_max.x, a.bb_max.y, a.bb_max.z],
                        lod_dist: a.lod_dist,
                        txd_hash: a.texture_dict_hash,
                        drawable_dictionary_hash: a.drawable_dictionary_hash,
                        asset_name_hash: a.asset_name_hash,
                        asset_type: a.asset_type,
                        is_mlo: a.is_mlo,
                    })
                    .collect();
            }
            Err(e) => out.failure = Some(fail("parse_ytyp", &e)),
        }
        return out;
    }

    let stem = lower.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or_else(|| lower.clone());
    let stem_hash = rage_joaat(&stem);
    let kind = match ext.as_str() {
        "ydr" => Kind::Drawable,
        "ydd" => Kind::Dictionary,
        "yft" => Kind::Fragment,
        _ => Kind::Txd,
    };
    let mut container = ItemRow::new(kind, stem_hash, Some(stem.clone()));
    container.file_size = Some(file.size);
    container.mem_size = Some(file.mem_size);
    if let RpfEntryKind::ResourceFile { system_flags, graphics_flags, .. } = archive.entry_kind(file) {
        container.resource_version = Some(resource_version_from_flags(*system_flags, *graphics_flags));
    }
    // `source_sha256` is left for whoever first needs it (sheets, packs):
    // hashing every byte of the install would double the build's reads.
    if kind != Kind::Txd {
        container.txd_hash = Some(stem_hash);
    }
    out.items.push(container);

    let parsed: Result<()> = (|| {
        match kind {
            Kind::Drawable => {
                let d = parse_ydr(&data)?;
                fill_drawable(&mut out.items[0], &d);
                push_embedded(&mut out.items, 0, &d);
            }
            Kind::Dictionary => {
                let entries = parse_ydd(&data)?;
                let mut seen: HashMap<String, usize> = HashMap::new();
                for entry in &entries {
                    let name = if entry.name.is_empty() {
                        names.get(entry.hash).map(str::to_string)
                    } else {
                        Some(strip_rage_suffix(&entry.name).to_lowercase())
                    };
                    let mut row = ItemRow::new(Kind::DdEntry, entry.hash, name.clone());
                    row.member = Some(unique_member(&mut seen, name.unwrap_or_else(|| hex8(entry.hash))));
                    row.parent = Some(0);
                    row.txd_hash = Some(stem_hash);
                    fill_drawable(&mut row, &entry.drawable);
                    out.items.push(row);
                    let idx = out.items.len() - 1;
                    push_embedded(&mut out.items, idx, &entry.drawable);
                }
            }
            Kind::Fragment => {
                let frag = parse_yft(&data)?;
                if let Some(body) = &frag.drawable {
                    fill_drawable(&mut out.items[0], body);
                    push_embedded(&mut out.items, 0, body);
                }
                let mut seen: HashMap<String, usize> = HashMap::new();
                for extra in &frag.extra_drawables {
                    let name = if extra.name.is_empty() {
                        names.get(extra.hash).map(str::to_string)
                    } else {
                        Some(strip_rage_suffix(&extra.name).to_lowercase())
                    };
                    let mut row = ItemRow::new(Kind::DdEntry, extra.hash, name.clone());
                    row.member = Some(unique_member(&mut seen, name.unwrap_or_else(|| hex8(extra.hash))));
                    row.parent = Some(0);
                    row.txd_hash = Some(stem_hash);
                    fill_drawable(&mut row, &extra.drawable);
                    out.items.push(row);
                    let idx = out.items.len() - 1;
                    push_embedded(&mut out.items, idx, &extra.drawable);
                }
            }
            _ => {
                let system = archive.resource_system(file, keys)?;
                let textures = parse_ytd_system(&system)?;
                push_textures(&mut out.items, 0, stem_hash, &textures, false);
            }
        }
        Ok(())
    })();
    if let Err(e) = parsed {
        let stage = match kind {
            Kind::Drawable => "parse_ydr",
            Kind::Dictionary => "parse_ydd",
            Kind::Fragment => "parse_yft",
            _ => "parse_ytd",
        };
        out.items.truncate(1);
        out.items[0].parse_error = Some(format!("{e:#}"));
        out.failure = Some(fail(stage, &format!("{e:#}")));
    }
    let _ = wanted;
    out
}

/// `prop_x.#dr` -> `prop_x`: names inside resources can carry RAGE's
/// type suffix.
fn strip_rage_suffix(name: &str) -> &str {
    for suffix in [".#dr", ".#dd", ".#ft", ".#td"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            return stripped;
        }
    }
    name
}

fn unique_member(seen: &mut HashMap<String, usize>, name: String) -> String {
    let n = seen.entry(name.clone()).or_insert(0);
    *n += 1;
    if *n == 1 { name } else { format!("{name}~{n}") }
}

fn fill_drawable(row: &mut ItemRow, d: &Drawable) {
    let lod = d.best_lod();
    let (bounds, computed) = match lod {
        Some(l) => d.bounds_or_computed(l),
        None => (d.bounds.clone(), false),
    };
    let diffuse = (0..d.shader_count() as u16).find_map(|i| d.diffuse_texture_name(i).map(str::to_lowercase));
    let mut refs = Vec::new();
    if let Some(group) = &d.shader_group {
        for shader in &group.shaders {
            for p in &shader.parameters {
                if let rage_formats::ShaderParameterValue::Texture { name, name_hash } = &p.value {
                    let lower = name.to_lowercase();
                    let hash = if lower.is_empty() { *name_hash } else { rage_joaat(&lower) };
                    refs.push((hash, lower, shader.name_hash, p.name_hash));
                }
            }
        }
    }
    refs.sort();
    refs.dedup_by(|a, b| a.0 == b.0 && a.2 == b.2 && a.3 == b.3);
    row.refs = refs;
    row.drawable = Some(DrawableStats {
        bb_min: [bounds.box_min.x, bounds.box_min.y, bounds.box_min.z],
        bb_max: [bounds.box_max.x, bounds.box_max.y, bounds.box_max.z],
        radius: bounds.sphere_radius,
        computed,
        lods: d.lod_distances,
        triangles: lod.map(|l| d.triangle_count(l) as u64).unwrap_or(0),
        lod_count: d.lods.len() as u32,
        shaders: d.shader_count() as u32,
        embedded: d.embedded_texture_count() as u32,
        diffuse,
    });
}

fn push_embedded(items: &mut Vec<ItemRow>, parent: usize, d: &Drawable) {
    if let Some(group) = &d.shader_group
        && !group.textures.is_empty()
    {
        let owner = items[parent].hash;
        push_textures(items, parent, owner, &group.textures, true);
    }
}

fn push_textures(items: &mut Vec<ItemRow>, parent: usize, txd_hash: u32, textures: &[YtdTexture], embedded: bool) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for tex in textures {
        let lower = tex.name.to_lowercase();
        let hash = if lower.is_empty() { tex.name_hash } else { rage_joaat(&lower) };
        let name = if lower.is_empty() { None } else { Some(lower.clone()) };
        let mut row = ItemRow::new(Kind::Texture, hash, name);
        row.asset_key = asset_key(Kind::Texture, hash, Some(txd_hash));
        row.member = Some(unique_member(&mut seen, if lower.is_empty() { hex8(hash) } else { lower }));
        row.parent = Some(parent);
        row.txd_hash = Some(txd_hash);
        row.texture = Some(TexStats {
            width: tex.width,
            height: tex.height,
            depth: tex.depth,
            levels: tex.levels,
            format: tex.format.to_string(),
            bytes: (ytd_chain_size(tex.format, tex.width, tex.height, tex.levels) * tex.depth.max(1) as usize) as u64,
            embedded,
        });
        items.push(row);
    }
}

/// `<rel archive>[//<nested>...]/<inner path>`: where an entry lives,
/// readable and unique.
pub fn location(rel: &str, nested: &[String], inner: &str) -> String {
    let mut s = rel.to_string();
    for n in nested {
        s.push_str("//");
        s.push_str(n);
    }
    s.push_str("//");
    s.push_str(inner);
    s
}

// ─── Writing ────────────────────────────────────────────────────────────────

fn write_archive(tx: &Transaction, scan: &ArchiveScan) -> Result<()> {
    let id = scan.archive_id;
    tx.execute("DELETE FROM items WHERE archive_id = ?1", [id])?;
    tx.execute("DELETE FROM archetypes WHERE archive_id = ?1", [id])?;
    tx.execute("DELETE FROM txd_parents WHERE archive_id = ?1", [id])?;

    let (rel, tier, dlc): (String, String, Option<String>) = tx.query_row(
        "SELECT rel_path, tier, dlc_pack FROM archives WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let tier_tokens = match &dlc {
        Some(pack) => format!("{tier} {}", tokenize_name(pack)),
        None => tier.clone(),
    };

    {
        let mut insert_item = tx.prepare_cached(
            "INSERT INTO items(key, asset_key, kind, hash, name, archive_id, nested, inner_path, entry_name, member, parent_id, root_id,
               file_size, mem_size, resource_version, source_sha256,
               bb_min_x, bb_min_y, bb_min_z, bb_max_x, bb_max_y, bb_max_z, size_x, size_y, size_z, radius, bounds_computed,
               lod_high, lod_med, lod_low, lod_vlow, triangles, lods, shaders, embedded_textures, diffuse_texture, txd_hash,
               width, height, depth, levels, format, bytes, embedded, parse_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25,
               ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45)",
        )?;
        let mut set_root = tx.prepare_cached("UPDATE items SET root_id = id WHERE id = ?1")?;
        let mut insert_doc = tx.prepare_cached("INSERT INTO docs(item_id, names, path) VALUES (?1, ?2, ?3)")?;
        let mut insert_ref = tx.prepare_cached(
            "INSERT OR IGNORE INTO texture_refs(item_id, texture_hash, texture_name, shader_hash, parameter_hash) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;

        for out in &scan.outs {
            if out.items.is_empty() {
                continue;
            }
            let nested: Vec<String> = if out.nested.is_empty() { Vec::new() } else { out.nested.split("//").map(str::to_string).collect() };
            let loc = location(&rel, &nested, &out.inner_path);
            let mut nested_segments: Vec<&str> = vec![rel.as_str()];
            nested_segments.extend(nested.iter().map(String::as_str));
            nested_segments.push(out.inner_path.as_str());
            let path_tokens = format!("{} {}", tokenize_path(&nested_segments), tier_tokens);
            let container_tokens = tokenize_name(out.entry_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&out.entry_name));

            let mut ids: Vec<i64> = Vec::with_capacity(out.items.len());
            for row in &out.items {
                // Where the row lives is unique on its own: the container is
                // the location, members hang off it (a texture embedded in a
                // dictionary entry also names that entry).
                let key = match &row.member {
                    Some(m) => match row.parent.and_then(|p| out.items[p].member.as_ref()) {
                        Some(pm) => format!("{loc}#{pm}#{m}"),
                        None => format!("{loc}#{m}"),
                    },
                    None => loc.clone(),
                };
                let parent_id = row.parent.map(|p| ids[p]);
                let root_id = if row.parent.is_some() { Some(ids[0]) } else { None };
                let d = row.drawable.as_ref();
                let t = row.texture.as_ref();
                let size = d.map(|d| [d.bb_max[0] - d.bb_min[0], d.bb_max[1] - d.bb_min[1], d.bb_max[2] - d.bb_min[2]]);
                insert_item.execute(params![
                    key,
                    row.asset_key,
                    row.kind.as_str(),
                    row.hash as i64,
                    row.name,
                    id,
                    out.nested,
                    out.inner_path,
                    out.entry_name,
                    row.member,
                    parent_id,
                    root_id,
                    row.file_size.map(|v| v as i64),
                    row.mem_size.map(|v| v as i64),
                    row.resource_version.map(|v| v as i64),
                    row.source_sha256,
                    d.map(|d| d.bb_min[0] as f64),
                    d.map(|d| d.bb_min[1] as f64),
                    d.map(|d| d.bb_min[2] as f64),
                    d.map(|d| d.bb_max[0] as f64),
                    d.map(|d| d.bb_max[1] as f64),
                    d.map(|d| d.bb_max[2] as f64),
                    size.map(|s| s[0] as f64),
                    size.map(|s| s[1] as f64),
                    size.map(|s| s[2] as f64),
                    d.map(|d| d.radius as f64),
                    d.map(|d| d.computed as i64),
                    d.map(|d| d.lods[0] as f64),
                    d.map(|d| d.lods[1] as f64),
                    d.map(|d| d.lods[2] as f64),
                    d.map(|d| d.lods[3] as f64),
                    d.map(|d| d.triangles as i64),
                    d.map(|d| d.lod_count as i64),
                    d.map(|d| d.shaders as i64),
                    d.map(|d| d.embedded as i64),
                    d.and_then(|d| d.diffuse.clone()),
                    row.txd_hash.map(|v| v as i64),
                    t.map(|t| t.width as i64),
                    t.map(|t| t.height as i64),
                    t.map(|t| t.depth as i64),
                    t.map(|t| t.levels as i64),
                    t.map(|t| t.format.clone()),
                    t.map(|t| t.bytes as i64),
                    t.map(|t| t.embedded as i64),
                    row.parse_error,
                ])?;
                let item_id = tx.last_insert_rowid();
                if row.parent.is_none() {
                    set_root.execute([item_id])?;
                }
                ids.push(item_id);

                let label = row.name.clone().or_else(|| row.member.clone()).unwrap_or_else(|| hex8(row.hash));
                let mut names_text = tokenize_name(&label);
                if let Some(diffuse) = d.and_then(|d| d.diffuse.as_ref()) {
                    names_text.push(' ');
                    names_text.push_str(&tokenize_name(diffuse));
                }
                // Members carry their container's name instead of the whole
                // path: it is what tells `dryhills` in `ch3_11_land.ytd` apart,
                // and keeps the index a fraction of the size.
                let path_text = if row.parent.is_none() {
                    path_tokens.as_str()
                } else {
                    names_text.push(' ');
                    names_text.push_str(&container_tokens);
                    ""
                };
                insert_doc.execute(params![item_id, names_text, path_text])?;
                for (hash, name, shader, param) in &row.refs {
                    insert_ref.execute(params![item_id, *hash as i64, name, *shader as i64, *param as i64])?;
                }
            }
        }
    }

    {
        let mut insert_arch = tx.prepare_cached(
            "INSERT INTO archetypes(archive_id, ytyp_key, name_hash, name, bb_min_x, bb_min_y, bb_min_z, bb_max_x, bb_max_y, bb_max_z,
               lod_dist, txd_hash, drawable_dictionary_hash, asset_name_hash, asset_type, is_mlo)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )?;
        let mut insert_parent = tx.prepare_cached("INSERT OR IGNORE INTO txd_parents(child, parent, archive_id) VALUES (?1, ?2, ?3)")?;
        for out in &scan.outs {
            for a in &out.archetypes {
                insert_arch.execute(params![
                    id, a.ytyp_key, a.name_hash as i64, a.name,
                    a.bb_min[0] as f64, a.bb_min[1] as f64, a.bb_min[2] as f64,
                    a.bb_max[0] as f64, a.bb_max[1] as f64, a.bb_max[2] as f64,
                    a.lod_dist as f64, a.txd_hash as i64, a.drawable_dictionary_hash as i64,
                    a.asset_name_hash as i64, a.asset_type as i64, a.is_mlo as i64,
                ])?;
            }
            for (child, parent) in &out.txd_parents {
                insert_parent.execute(params![*child as i64, *parent as i64, id])?;
            }
        }
    }

    let (status, error) = match &scan.status {
        ScanStatus::Ok => ("ok", None),
        ScanStatus::NeedsKeys => ("needs_keys", Some("needs keys: pass --exe or --keys".to_string())),
        ScanStatus::Failed(e) => ("failed", Some(e.clone())),
    };
    tx.execute(
        "UPDATE archives SET size = ?2, mtime = ?3, head_sha256 = ?4, scanner = ?5, status = ?6, error = ?7, scanned_at = ?8, entries = ?9 WHERE id = ?1",
        params![id, scan.size as i64, scan.mtime, scan.head, SCANNER_VERSION, status, error, now_secs(), scan.entries as i64],
    )?;
    Ok(())
}

/// Marks the copy of each asset the game actually loads: the one in the
/// latest-loading archive. Entries and textures inherit their container's.
fn winner_pass(cat: &mut Catalog) -> Result<()> {
    let tx = cat.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "UPDATE items SET winner = 0 WHERE winner <> 0;
         UPDATE items SET winner = 1 WHERE id IN (
           SELECT id FROM (
             SELECT i.id, ROW_NUMBER() OVER (PARTITION BY i.kind, i.hash
                 ORDER BY a.load_rank DESC, i.nested DESC, i.inner_path DESC) AS rn
             FROM items i JOIN archives a ON a.id = i.archive_id
             WHERE i.parent_id IS NULL
           ) WHERE rn = 1);
         UPDATE items SET winner = 1 WHERE parent_id IS NOT NULL
           AND root_id IN (SELECT id FROM items WHERE parent_id IS NULL AND winner = 1);
         UPDATE archetypes SET winner = 0 WHERE winner <> 0;
         UPDATE archetypes SET winner = 1 WHERE id IN (
           SELECT id FROM (
             SELECT t.id, ROW_NUMBER() OVER (PARTITION BY t.name_hash ORDER BY a.load_rank DESC, t.id DESC) AS rn
             FROM archetypes t JOIN archives a ON a.id = t.archive_id
           ) WHERE rn = 1);",
    )?;
    let winners: i64 = tx.query_row("SELECT count(*) FROM items WHERE winner = 1", [], |r| r.get(0))?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES ('winner_count', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [winners.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Fills in names for rows whose hash only became resolvable after they
/// were scanned (a newer `names harvest`, or `--names`). Returns how many
/// rows it named.
fn names_pass(cat: &mut Catalog, names: &NameTable) -> Result<usize> {
    let pending: Vec<(i64, u32)> = {
        let mut stmt = cat.conn.prepare("SELECT id, hash FROM items WHERE name IS NULL")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let tx = cat.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut named = 0;
    {
        let mut set_name = tx.prepare_cached("UPDATE items SET name = ?2 WHERE id = ?1")?;
        let mut get_doc = tx.prepare_cached("SELECT names FROM docs WHERE item_id = ?1")?;
        let mut set_doc = tx.prepare_cached("UPDATE docs SET names = ?2 WHERE item_id = ?1")?;
        for (id, hash) in pending {
            if let Some(name) = names.get(hash) {
                set_name.execute(params![id, name])?;
                let old: Option<String> = get_doc.query_row([id], |r| r.get(0)).optional()?;
                let text = format!("{} {}", tokenize_name(name), old.unwrap_or_default());
                set_doc.execute(params![id, text.trim()])?;
                named += 1;
            }
        }
        let mut arch = tx.prepare("SELECT DISTINCT name_hash FROM archetypes WHERE name IS NULL")?;
        let hashes: Vec<u32> = arch.query_map([], |r| Ok(r.get::<_, i64>(0)? as u32))?.collect::<rusqlite::Result<_>>()?;
        let mut set_arch = tx.prepare_cached("UPDATE archetypes SET name = ?2 WHERE name_hash = ?1 AND name IS NULL")?;
        for h in hashes {
            if let Some(name) = names.get(h) {
                set_arch.execute(params![h as i64, name])?;
            }
        }
    }
    tx.commit()?;
    Ok(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wanted_follows_kinds() {
        let w = Wanted::from(&[Kind::Texture]);
        assert!(w.ytd && !w.ydr && !w.ydd && !w.yft);
        let all = Wanted::from(&[]);
        assert!(all.ytd && all.ydr && all.ydd && all.yft);
        assert!(wants_entry("gtxd.meta", w));
        assert!(wants_entry("X.YTYP", w));
        assert!(!wants_entry("a.ydr", w));
    }

    #[test]
    fn members_are_made_unique() {
        let mut seen = HashMap::new();
        assert_eq!(unique_member(&mut seen, "a".into()), "a");
        assert_eq!(unique_member(&mut seen, "a".into()), "a~2");
        assert_eq!(unique_member(&mut seen, "b".into()), "b");
    }

    #[test]
    fn locations_join_nested_archives() {
        assert_eq!(location("x64c.rpf", &["a/b.rpf".into()], "c.ydr"), "x64c.rpf//a/b.rpf//c.ydr");
        assert_eq!(location("x64c.rpf", &[], "c.ydr"), "x64c.rpf//c.ydr");
    }

    #[test]
    fn plurals_read_naturally() {
        assert_eq!(plural("dd_entry", 2), "dictionary entries");
        assert_eq!(plural("txd", 1), "texture dictionary");
        assert_eq!(plural("drawable", 3), "drawables");
    }
}
