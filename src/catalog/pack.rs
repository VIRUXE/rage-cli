//! `catalog pack`: sharing reviews as text-only packs.
//!
//! A pack holds descriptions and the identities needed to find the same
//! asset elsewhere (asset key, name, hash, source bytes hash, game build),
//! never images, archive paths or game data. Imported reviews are always
//! stored as `shared-visual`: someone looked, but not on this machine. When
//! the source bytes match exactly the limitations say so; otherwise the
//! match is by asset identity only and says that instead. Local reviews are
//! never superseded by imported ones.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;

use super::annotate::{insert, refresh_docs, NewAnnotation};
use super::loader::EntryLoader;
use super::scan::hex_digest;
use super::search::load_item;
use super::{now_secs, Catalog};
use crate::rpf::GtaKeys;

pub struct ExportOptions {
    pub reviewer: Option<String>,
    pub since: Option<i64>,
    pub include_shared: bool,
}

pub struct ExportSummary {
    pub pack_id: String,
    pub count: usize,
}

fn opt(v: Option<String>) -> json::JsonValue {
    v.map(Into::into).unwrap_or(json::JsonValue::Null)
}

pub fn export(cat: &Catalog, out: &Path, opts: &ExportOptions) -> Result<ExportSummary> {
    let mut sql = String::from(
        "SELECT n.asset_key, i.kind, i.name, i.hash, i.txd_hash, n.source_sha256, n.game_build, n.method, n.reviewer, n.reviewer_name,
                n.views, n.evidence_sha256, n.description, n.shape, n.material, n.condition, n.likely_use, n.tags, n.confidence,
                n.orientation_doubt, n.missing_views, n.limitations, n.created, n.content_sha256
         FROM annotations n LEFT JOIN items i ON i.id = n.item_id
         WHERE n.superseded_by IS NULL",
    );
    if !opts.include_shared {
        sql.push_str(" AND n.method = 'visual'");
    }
    if let Some(r) = &opts.reviewer {
        if r != "agent" && r != "human" {
            bail!("--reviewer must be agent or human");
        }
        sql.push_str(&format!(" AND n.reviewer = '{r}'"));
    }
    if let Some(since) = opts.since {
        sql.push_str(&format!(" AND n.created >= {since}"));
    }
    sql.push_str(" ORDER BY n.asset_key, n.created");
    let mut stmt = cat.conn.prepare(&sql)?;
    let mut entries = json::JsonValue::new_array();
    let mut builds: Vec<i64> = Vec::new();
    let mut digest = Sha256::new();
    let mut count = 0;
    let rows = stmt.query_map([], |r| {
        let s = |i: usize| r.get::<_, Option<String>>(i);
        let list = |v: Option<String>| -> json::JsonValue {
            v.map(|t| t.split(',').map(|x| x.to_string()).collect::<Vec<_>>().into()).unwrap_or_else(|| json::array![])
        };
        let build: Option<i64> = r.get(6)?;
        let o = json::object! {
            asset_key: r.get::<_, String>(0)?,
            kind: opt(s(1)?),
            name: opt(s(2)?),
            hash: r.get::<_, Option<i64>>(3)?.map(|h| format!("0x{:08X}", h as u32)),
            txd_hash: r.get::<_, Option<i64>>(4)?.map(|h| format!("0x{:08X}", h as u32)),
            source_sha256: opt(s(5)?),
            game_build: build,
            method: r.get::<_, String>(7)?,
            reviewer: r.get::<_, String>(8)?,
            reviewer_name: opt(s(9)?),
            views: opt(s(10)?),
            evidence_sha256: opt(s(11)?),
            description: r.get::<_, String>(12)?,
            shape: opt(s(13)?), material: opt(s(14)?), condition: opt(s(15)?), likely_use: opt(s(16)?),
            tags: list(s(17)?),
            confidence: r.get::<_, Option<f64>>(18)?,
            orientation_doubt: r.get::<_, i64>(19)? != 0,
            missing_views: list(s(20)?),
            limitations: opt(s(21)?),
            created: r.get::<_, i64>(22)?,
        };
        Ok((o, build, r.get::<_, String>(23)?))
    })?;
    for row in rows {
        let (o, build, content) = row?;
        if let Some(b) = build
            && !builds.contains(&b)
        {
            builds.push(b);
        }
        digest.update(content.as_bytes());
        let _ = entries.push(o);
        count += 1;
    }
    builds.sort_unstable();
    let pack_id = format!("gta5-annotations-{}", &hex_digest(&digest.finalize())[..8]);
    let pack = json::object! {
        schema: "rage-catalog-pack/1",
        pack_id: pack_id.clone(),
        version: crate::names::today(),
        game: "gta5",
        game_builds: builds,
        generator: format!("rage {}", env!("CARGO_PKG_VERSION")),
        license: "CC0-1.0",
        count: count,
        annotations: entries,
    };
    let tmp = out.with_extension("json.tmp");
    std::fs::write(&tmp, pack.pretty(2)).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, out).with_context(|| format!("failed to write {}", out.display()))?;
    Ok(ExportSummary { pack_id, count })
}

#[derive(Debug, Default)]
pub struct ImportSummary {
    pub exact: usize,
    pub by_identity: usize,
    pub unmatched: usize,
    pub duplicates: usize,
    pub invalid: Vec<(usize, String)>,
}

impl ImportSummary {
    pub fn line(&self) -> String {
        let mut s = format!(
            "Imported {} review(s): {} with identical source bytes, {} by asset identity only, {} not in this catalogue (kept for later builds)",
            self.exact + self.by_identity + self.unmatched,
            self.exact,
            self.by_identity,
            self.unmatched
        );
        if self.duplicates > 0 {
            s.push_str(&format!("; {} duplicate(s)", self.duplicates));
        }
        if !self.invalid.is_empty() {
            s.push_str(&format!("; {} invalid, first: entry {}: {}", self.invalid.len(), self.invalid[0].0, self.invalid[0].1));
        }
        s
    }

    pub fn json(&self) -> json::JsonValue {
        let invalid: Vec<json::JsonValue> = self.invalid.iter().map(|(i, r)| json::object! { entry: *i, reason: r.clone() }).collect();
        json::object! { exact: self.exact, by_identity: self.by_identity, unmatched: self.unmatched, duplicates: self.duplicates, invalid: invalid }
    }
}

/// The container's source hash for `item_id`, reading and recording it the
/// first time it is asked for.
pub fn source_hash(cat: &Catalog, loader: &mut EntryLoader, item_id: i64) -> Result<Option<String>> {
    let root: i64 = cat.conn.query_row("SELECT COALESCE(root_id, id) FROM items WHERE id = ?1", [item_id], |r| r.get(0))?;
    let known: Option<String> = cat.conn.query_row("SELECT source_sha256 FROM items WHERE id = ?1", [root], |r| r.get(0))?;
    if known.is_some() {
        return Ok(known);
    }
    let item = load_item(cat, root)?;
    match loader.item_bytes(&item) {
        Ok(bytes) => {
            let sha = hex_digest(&Sha256::digest(&bytes));
            cat.conn.execute("UPDATE items SET source_sha256 = ?2 WHERE id = ?1", params![root, sha])?;
            Ok(Some(sha))
        }
        Err(e) => {
            log::debug!("catalog: cannot hash {}: {e}", item.key);
            Ok(None)
        }
    }
}

fn text(v: &json::JsonValue) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn list(v: &json::JsonValue) -> Vec<String> {
    v.members().filter_map(|m| m.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

pub fn import(cat: &mut Catalog, keys: Option<&GtaKeys>, path: &Path) -> Result<ImportSummary> {
    let pack = json::parse(&std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    if pack["schema"] != "rage-catalog-pack/1" {
        bail!("{} is not a rage catalogue pack (schema must be rage-catalog-pack/1)", path.display());
    }
    let pack_id = text(&pack["pack_id"]).context("pack has no pack_id")?;
    let version = text(&pack["version"]);
    let mut summary = ImportSummary::default();
    let mut loader = EntryLoader::new(keys);

    // Resolve matches first (this may read game files), then write in one go.
    let mut staged: Vec<(NewAnnotation, bool)> = Vec::new();
    for (i, e) in pack["annotations"].members().enumerate() {
        let Some(asset_key) = text(&e["asset_key"]) else {
            summary.invalid.push((i, "no asset_key".into()));
            continue;
        };
        let Some(description) = text(&e["description"]) else {
            summary.invalid.push((i, "no description".into()));
            continue;
        };
        let reviewer = text(&e["reviewer"]).unwrap_or_default();
        if reviewer != "agent" && reviewer != "human" {
            summary.invalid.push((i, format!("reviewer must be agent or human, not '{reviewer}'")));
            continue;
        }
        let method = text(&e["method"]).unwrap_or_default();
        if !matches!(method.as_str(), "visual" | "shared-visual" | "metadata") {
            summary.invalid.push((i, format!("unknown method '{method}'")));
            continue;
        }
        let pack_sha = text(&e["source_sha256"]);
        let reviewed_on = e["game_build"].as_i64();

        let candidates: Vec<i64> = cat
            .conn
            .prepare_cached("SELECT id FROM items WHERE asset_key = ?1 ORDER BY winner DESC, id")?
            .query_map([&asset_key], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut matched: Option<(i64, bool)> = None;
        if let Some(want) = &pack_sha {
            for id in &candidates {
                if source_hash(cat, &mut loader, *id)?.as_deref() == Some(want.as_str()) {
                    matched = Some((*id, true));
                    break;
                }
            }
        }
        if matched.is_none() {
            matched = candidates.first().map(|id| (*id, false));
        }
        let origin = match (&text(&e["reviewer_name"]), reviewed_on) {
            (Some(n), Some(b)) => format!("reviewed by {reviewer} {n} on build {b}"),
            (Some(n), None) => format!("reviewed by {reviewer} {n}"),
            (None, Some(b)) => format!("reviewed by {reviewer} on build {b}"),
            (None, None) => format!("reviewed by {reviewer}"),
        };
        let note = match matched {
            Some((_, true)) => format!("from pack {pack_id}, {origin}; source bytes identical, no local review"),
            Some((_, false)) => format!("from pack {pack_id}, {origin}; matched by asset identity only, source bytes differ or unverified"),
            None => format!("from pack {pack_id}, {origin}; not in this catalogue"),
        };
        let limitations = match text(&e["limitations"]) {
            Some(l) => format!("{note}. {l}"),
            None => note,
        };
        let item_id = matched.map(|m| m.0);
        let source_sha256 = match item_id {
            Some(id) => source_hash(cat, &mut loader, id)?,
            None => None,
        };
        let a = NewAnnotation {
            item_id,
            asset_key,
            source_sha256,
            game_build: reviewed_on,
            method: "shared-visual".into(),
            reviewer,
            reviewer_name: text(&e["reviewer_name"]),
            packet_id: None,
            tile: None,
            evidence_sha256: text(&e["evidence_sha256"]),
            views: text(&e["views"]),
            description,
            shape: text(&e["shape"]),
            material: text(&e["material"]),
            condition: text(&e["condition"]),
            likely_use: text(&e["likely_use"]),
            tags: list(&e["tags"]),
            confidence: e["confidence"].as_f64().filter(|c| (0.0..=1.0).contains(c)),
            orientation_doubt: e["orientation_doubt"].as_bool().unwrap_or(false),
            missing_views: list(&e["missing_views"]),
            limitations: Some(limitations),
            pack_id: Some(pack_id.clone()),
            pack_version: version.clone(),
            created: e["created"].as_i64().unwrap_or_else(now_secs),
        };
        staged.push((a, matched.map(|m| m.1).unwrap_or(false)));
    }

    let tx = cat.conn.transaction()?;
    let mut touched: Vec<i64> = Vec::new();
    for (a, exact) in &staged {
        match insert(&tx, a, false)?.0 {
            None => summary.duplicates += 1,
            Some(_) => {
                match (a.item_id, exact) {
                    (Some(id), true) => {
                        summary.exact += 1;
                        touched.push(id);
                    }
                    (Some(id), false) => {
                        summary.by_identity += 1;
                        touched.push(id);
                    }
                    (None, _) => summary.unmatched += 1,
                }
            }
        }
    }
    touched.sort_unstable();
    touched.dedup();
    refresh_docs(&tx, &touched)?;
    tx.commit()?;
    Ok(summary)
}

/// After a rebuild: reattaches annotations whose asset row was replaced (or
/// that arrived before the asset did) to the copy the game now loads. A
/// review of bytes that have since changed gets a note saying so.
pub fn rematch(cat: &mut Catalog, keys: Option<&GtaKeys>) -> Result<usize> {
    let pending: Vec<(i64, String, Option<String>)> = cat
        .conn
        .prepare("SELECT id, asset_key, source_sha256 FROM annotations WHERE item_id IS NULL AND superseded_by IS NULL")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if pending.is_empty() {
        return Ok(0);
    }
    let mut loader = EntryLoader::new(keys);
    let mut updates: Vec<(i64, i64, bool)> = Vec::new();
    for (ann, asset_key, sha) in pending {
        let item: Option<i64> = cat
            .conn
            .query_row("SELECT id FROM items WHERE asset_key = ?1 ORDER BY winner DESC, id LIMIT 1", [&asset_key], |r| r.get(0))
            .optional()?;
        let Some(item) = item else { continue };
        let changed = match &sha {
            Some(want) => source_hash(cat, &mut loader, item)?.as_deref().is_some_and(|got| got != want),
            None => false,
        };
        updates.push((ann, item, changed));
    }
    let tx = cat.conn.transaction()?;
    let mut touched = Vec::new();
    for (ann, item, changed) in &updates {
        tx.execute("UPDATE annotations SET item_id = ?2 WHERE id = ?1", params![ann, item])?;
        if *changed {
            tx.execute(
                "UPDATE annotations SET limitations = 'source changed since this review; re-review before relying on it. ' || COALESCE(limitations, '') WHERE id = ?1",
                [ann],
            )?;
        }
        touched.push(*item);
    }
    touched.sort_unstable();
    touched.dedup();
    refresh_docs(&tx, &touched)?;
    tx.commit()?;
    Ok(updates.len())
}
