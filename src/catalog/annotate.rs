//! `catalog annotate`: importing a reviewer's descriptions of a packet.
//!
//! A description is only accepted for a tile the catalogue issued, quoting
//! that tile's image hash; it is stored against the asset with who wrote it
//! (an agent or a human, never silently mixed), the views it saw and the
//! evidence hash, and folded into the search index.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;

use super::scan::hex_digest;
use super::{now_secs, Catalog};

pub struct AnnotateOptions {
    pub reviewer: Option<String>,
    pub reviewer_name: Option<String>,
    pub verify_files: bool,
}

#[derive(Debug, Default)]
pub struct AnnotateSummary {
    pub imported: usize,
    pub duplicates: usize,
    pub superseded: usize,
    pub rejected: Vec<(i64, String)>,
}

/// One annotation as stored; shared by `annotate` and pack import.
#[derive(Debug, Clone, Default)]
pub struct NewAnnotation {
    pub item_id: Option<i64>,
    pub asset_key: String,
    pub source_sha256: Option<String>,
    pub game_build: Option<i64>,
    pub method: String,
    pub reviewer: String,
    pub reviewer_name: Option<String>,
    pub packet_id: Option<String>,
    pub tile: Option<i64>,
    pub evidence_sha256: Option<String>,
    pub views: Option<String>,
    pub description: String,
    pub shape: Option<String>,
    pub material: Option<String>,
    pub condition: Option<String>,
    pub likely_use: Option<String>,
    pub tags: Vec<String>,
    pub confidence: Option<f64>,
    pub orientation_doubt: bool,
    pub missing_views: Vec<String>,
    pub limitations: Option<String>,
    pub pack_id: Option<String>,
    pub pack_version: Option<String>,
    pub created: i64,
}

impl NewAnnotation {
    /// What makes two annotations the same statement about the same asset.
    pub fn content_sha256(&self) -> String {
        let mut h = Sha256::new();
        for part in [
            self.asset_key.as_str(),
            self.method.as_str(),
            self.reviewer.as_str(),
            self.reviewer_name.as_deref().unwrap_or(""),
            self.description.trim(),
            &self.tags.join(","),
            self.evidence_sha256.as_deref().unwrap_or(""),
        ] {
            h.update(part.as_bytes());
            h.update([0]);
        }
        hex_digest(&h.finalize())
    }
}

/// Inserts `a`; returns its id, or `None` when an identical one exists.
/// With `supersede`, an older active annotation of the same item by the
/// same kind of reviewer and method is marked superseded by this one.
pub fn insert(tx: &rusqlite::Transaction, a: &NewAnnotation, supersede: bool) -> Result<(Option<i64>, usize)> {
    let content = a.content_sha256();
    let changed = tx.execute(
        "INSERT OR IGNORE INTO annotations(item_id, asset_key, source_sha256, game_build, method, reviewer, reviewer_name,
           packet_id, tile, evidence_sha256, views, description, shape, material, condition, likely_use, tags, confidence,
           orientation_doubt, missing_views, limitations, pack_id, pack_version, content_sha256, created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)",
        params![
            a.item_id, a.asset_key, a.source_sha256, a.game_build, a.method, a.reviewer, a.reviewer_name, a.packet_id, a.tile,
            a.evidence_sha256, a.views, a.description.trim(), a.shape, a.material, a.condition, a.likely_use,
            (!a.tags.is_empty()).then(|| a.tags.join(",")), a.confidence, a.orientation_doubt as i64,
            (!a.missing_views.is_empty()).then(|| a.missing_views.join(",")), a.limitations, a.pack_id, a.pack_version,
            content, a.created,
        ],
    )?;
    if changed == 0 {
        return Ok((None, 0));
    }
    let id = tx.last_insert_rowid();
    let mut superseded = 0;
    if supersede && let Some(item) = a.item_id {
        superseded = tx.execute(
            "UPDATE annotations SET superseded_by = ?1 WHERE item_id = ?2 AND reviewer = ?3 AND method = ?4 AND id <> ?1 AND superseded_by IS NULL",
            params![id, item, a.reviewer, a.method],
        )?;
    }
    Ok((Some(id), superseded))
}

/// Rewrites the annotation text the search index holds for `item_ids`.
pub fn refresh_docs(tx: &rusqlite::Transaction, item_ids: &[i64]) -> Result<()> {
    let mut read = tx.prepare_cached(
        "SELECT description, tags, shape, material, condition, likely_use FROM annotations
         WHERE item_id = ?1 AND superseded_by IS NULL",
    )?;
    let mut write = tx.prepare_cached("UPDATE docs SET annotations = ?2 WHERE item_id = ?1")?;
    for id in item_ids {
        let parts: Vec<String> = read
            .query_map([id], |r| {
                let mut s: Vec<String> = vec![r.get(0)?];
                for i in 1..6 {
                    if let Some(v) = r.get::<_, Option<String>>(i)? {
                        s.push(v.replace(',', " "));
                    }
                }
                Ok(s.join(" "))
            })?
            .collect::<rusqlite::Result<_>>()?;
        write.execute(params![id, parts.join(" \n ")])?;
    }
    Ok(())
}

fn string_list(v: &json::JsonValue) -> Vec<String> {
    if let Some(s) = v.as_str() {
        return s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    }
    v.members().filter_map(|m| m.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn opt_str(v: &json::JsonValue) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

pub fn import(cat: &mut Catalog, packet_path: &Path, responses_path: &Path, opts: &AnnotateOptions) -> Result<AnnotateSummary> {
    let packet = json::parse(&std::fs::read_to_string(packet_path).with_context(|| format!("failed to read {}", packet_path.display()))?)
        .with_context(|| format!("{} is not valid JSON", packet_path.display()))?;
    let responses = json::parse(
        &std::fs::read_to_string(responses_path).with_context(|| format!("failed to read {}", responses_path.display()))?,
    )
    .with_context(|| format!("{} is not valid JSON", responses_path.display()))?;
    if packet["schema"] != "rage-catalog-packet/1" {
        bail!("{} is not a rage catalogue packet", packet_path.display());
    }
    if responses["schema"] != "rage-catalog-responses/1" {
        bail!("{} is not a responses file (schema must be rage-catalog-responses/1)", responses_path.display());
    }
    let pid = packet["packet_id"].as_str().context("packet has no packet_id")?.to_string();
    if responses["packet_id"].as_str() != Some(pid.as_str()) {
        bail!("responses are for packet {}, not {pid}", responses["packet_id"]);
    }
    let known: Option<(String,)> = cat.conn.query_row("SELECT views FROM packets WHERE packet_id = ?1", [&pid], |r| Ok((r.get(0)?,))).optional()?;
    let Some((views,)) = known else {
        bail!("packet {pid} is not in this catalogue; was it made with another --db?");
    };
    let reviewer = opts
        .reviewer
        .clone()
        .or_else(|| responses["reviewer"].as_str().map(str::to_string))
        .context("say who reviewed: --reviewer agent|human, or \"reviewer\" in the responses")?;
    if reviewer != "agent" && reviewer != "human" {
        bail!("reviewer must be 'agent' or 'human', not '{reviewer}'");
    }
    let reviewer_name = opts.reviewer_name.clone().or_else(|| opt_str(&responses["reviewer_name"]));
    let game_build: Option<i64> = cat.meta("game_build")?.and_then(|b| b.parse().ok());

    // Sheets whose files no longer hash as issued invalidate their tiles.
    let mut bad_sheets: Vec<String> = Vec::new();
    if opts.verify_files {
        let dir = packet_path.parent().unwrap_or(Path::new("."));
        let mut stmt = cat.conn.prepare("SELECT DISTINCT sheet_file, sheet_sha256 FROM packet_tiles WHERE packet_id = ?1")?;
        let sheets: Vec<(String, String)> = stmt.query_map([&pid], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (file, sha) in sheets {
            let ok = std::fs::read(dir.join(&file)).map(|b| hex_digest(&Sha256::digest(&b)) == sha).unwrap_or(false);
            if !ok {
                bad_sheets.push(file);
            }
        }
    }

    let mut summary = AnnotateSummary::default();
    let mut touched: Vec<i64> = Vec::new();
    let tx = cat.conn.transaction()?;
    for r in responses["tiles"].members() {
        let Some(tile) = r["tile"].as_i64() else {
            summary.rejected.push((-1, "a response without a tile number".into()));
            continue;
        };
        let row: Option<(Option<i64>, String, String, String)> = tx
            .query_row(
                "SELECT t.item_id, t.asset_key, t.tile_sha256, t.sheet_file FROM packet_tiles t WHERE t.packet_id = ?1 AND t.tile = ?2",
                params![pid, tile],
                |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?)),
            )
            .optional()?;
        let Some((item_id, asset_key, sha, sheet_file)) = row else {
            summary.rejected.push((tile, "no such tile in this packet".into()));
            continue;
        };
        let description = r["description"].as_str().unwrap_or("").trim().to_string();
        if description.is_empty() {
            // An empty template entry is a tile the reviewer skipped, not an error.
            if r["sha256"].as_str() == Some(sha.as_str()) {
                continue;
            }
            summary.rejected.push((tile, "empty description".into()));
            continue;
        }
        if r["sha256"].as_str() != Some(sha.as_str()) {
            summary.rejected.push((tile, "sha256 does not match the image this packet issued".into()));
            continue;
        }
        if bad_sheets.contains(&sheet_file) {
            summary.rejected.push((tile, format!("{sheet_file} has changed since it was issued")));
            continue;
        }
        let Some(item_id) = item_id else {
            summary.rejected.push((tile, "the asset is no longer in the catalogue (rebuilt since?)".into()));
            continue;
        };
        let source: Option<String> = tx
            .query_row("SELECT r.source_sha256 FROM items i JOIN items r ON r.id = COALESCE(i.root_id, i.id) WHERE i.id = ?1", [item_id], |x| x.get(0))
            .optional()?
            .flatten();
        let confidence = r["confidence"].as_f64().filter(|c| (0.0..=1.0).contains(c));
        let a = NewAnnotation {
            item_id: Some(item_id),
            asset_key,
            source_sha256: source,
            game_build,
            method: "visual".into(),
            reviewer: reviewer.clone(),
            reviewer_name: reviewer_name.clone(),
            packet_id: Some(pid.clone()),
            tile: Some(tile),
            evidence_sha256: Some(sha),
            views: Some(views.clone()),
            description,
            shape: opt_str(&r["shape"]),
            material: opt_str(&r["material"]),
            condition: opt_str(&r["condition"]),
            likely_use: opt_str(&r["likely_use"]),
            tags: string_list(&r["tags"]),
            confidence,
            orientation_doubt: r["orientation_doubt"].as_bool().unwrap_or(false),
            missing_views: string_list(&r["missing_views"]),
            limitations: opt_str(&r["limitations"]),
            pack_id: None,
            pack_version: None,
            created: now_secs(),
        };
        match insert(&tx, &a, true)? {
            (Some(_), s) => {
                summary.imported += 1;
                summary.superseded += s;
                if !touched.contains(&item_id) {
                    touched.push(item_id);
                }
            }
            (None, _) => summary.duplicates += 1,
        }
    }
    refresh_docs(&tx, &touched)?;
    tx.commit()?;
    Ok(summary)
}

impl AnnotateSummary {
    pub fn line(&self) -> String {
        let mut s = format!("Imported {} annotation(s)", self.imported);
        let mut extra = Vec::new();
        if self.duplicates > 0 {
            extra.push(format!("{} duplicate(s)", self.duplicates));
        }
        if self.superseded > 0 {
            extra.push(format!("{} earlier review(s) superseded", self.superseded));
        }
        if !self.rejected.is_empty() {
            let first: Vec<String> = self.rejected.iter().take(3).map(|(t, r)| format!("tile {t}: {r}")).collect();
            extra.push(format!("{} rejected: {}", self.rejected.len(), first.join("; ")));
        }
        if !extra.is_empty() {
            s.push_str(&format!(" ({})", extra.join(", ")));
        }
        s
    }

    pub fn json(&self) -> json::JsonValue {
        let rejected: Vec<json::JsonValue> = self.rejected.iter().map(|(t, r)| json::object! { tile: *t, reason: r.clone() }).collect();
        json::object! { imported: self.imported, duplicates: self.duplicates, superseded: self.superseded, rejected: rejected }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_ignores_surrounding_whitespace_but_not_words() {
        let mut a = NewAnnotation { asset_key: "k".into(), method: "visual".into(), reviewer: "agent".into(), description: "a red chair".into(), ..Default::default() };
        let h = a.content_sha256();
        a.description = "  a red chair ".into();
        assert_eq!(h, a.content_sha256());
        a.description = "a blue chair".into();
        assert_ne!(h, a.content_sha256());
    }

    #[test]
    fn lists_accept_arrays_or_commas() {
        assert_eq!(string_list(&json::parse(r#"["a", " b ", ""]"#).unwrap()), vec!["a", "b"]);
        assert_eq!(string_list(&json::JsonValue::from("x, y")), vec!["x", "y"]);
    }
}
