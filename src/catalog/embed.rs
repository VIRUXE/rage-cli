//! `catalog embed` and `search --semantic/--hybrid`: local text embeddings.
//!
//! Only built with the `semantic` feature (fastembed / ONNX Runtime; the
//! runtime and model weights are downloaded on first use into
//! `~/.rage-cli/models`). Vectors are L2-normalised f32 stored in SQLite and
//! compared with a dot product over the rows the filters leave, so no
//! vector index is needed at the sizes a review campaign reaches.
//!
//! What gets embedded: every item with a review (that is where meaning is
//! beyond the name), and with `--models` every model the game loads. Names
//! alone are already covered well by the lexical index.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, params_from_iter, types::Value};
use sha2::{Digest, Sha256};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use super::scan::hex_digest;
use super::search::{filter_sql, lexical_ids, rrf, Mode, SearchQuery};
use super::Catalog;

pub const ENCODER_ID: &str = "fastembed/intfloat-multilingual-e5-small";
const DIMS: usize = 384;

pub struct Encoder {
    model: TextEmbedding,
}

impl Encoder {
    pub fn new(quiet: bool) -> Result<Self> {
        let cache = crate::paths::config_root().context("no home directory for the model cache")?.join("models");
        std::fs::create_dir_all(&cache)?;
        let opts = TextInitOptions::new(EmbeddingModel::MultilingualE5Small)
            .with_cache_dir(cache)
            .with_show_download_progress(!quiet);
        let model = TextEmbedding::try_new(opts).map_err(|e| anyhow!("failed to load the embedding model: {e}"))?;
        Ok(Encoder { model })
    }

    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = self.model.embed(texts, Some(64)).map_err(|e| anyhow!("embedding failed: {e}"))?;
        for v in &mut out {
            normalise(v);
        }
        Ok(out)
    }
}

fn normalise(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

pub struct EmbedOptions {
    pub models: bool,
    pub all: bool,
    pub limit: Option<usize>,
    pub quiet: bool,
}

#[derive(Default)]
pub struct EmbedSummary {
    pub candidates: usize,
    pub embedded: usize,
    pub unchanged: usize,
    pub elapsed: std::time::Duration,
}

/// The passage an item is embedded as: what it is, its name, its container,
/// and every active review.
fn passage(kind: &str, name: &str, container: &str, reviews: &str) -> String {
    let mut s = format!("passage: {kind} {} (in {})", name.replace('_', " "), container.replace('_', " "));
    if !reviews.is_empty() {
        s.push_str(". ");
        s.push_str(reviews);
    }
    s
}

pub fn embed_all(cat: &mut Catalog, opts: &EmbedOptions) -> Result<EmbedSummary> {
    let started = std::time::Instant::now();
    let mut summary = EmbedSummary::default();
    if cat.meta("embed_encoder")?.is_some_and(|e| e != ENCODER_ID) {
        eprintln!("The catalogue was embedded with another encoder; re-embedding everything.");
        cat.conn.execute("DELETE FROM embeddings", [])?;
    }
    let mut sql = String::from(
        "SELECT i.id, i.kind, COALESCE(i.name, i.member, printf('%08x', i.hash)), i.entry_name,
                COALESCE((SELECT group_concat(n.description || ' ' || COALESCE(n.tags, ''), '. ') FROM annotations n
                          WHERE n.item_id = i.id AND n.superseded_by IS NULL), '')
         FROM items i WHERE i.winner = 1 AND (EXISTS (SELECT 1 FROM annotations n WHERE n.item_id = i.id AND n.superseded_by IS NULL)",
    );
    if opts.models {
        sql.push_str(" OR i.kind IN ('drawable','fragment','dd_entry')");
    }
    sql.push_str(") ORDER BY i.id");
    if let Some(l) = opts.limit {
        sql.push_str(&format!(" LIMIT {l}"));
    }
    let rows: Vec<(i64, String)> = cat
        .conn
        .prepare(&sql)?
        .query_map([], |r| {
            let entry: String = r.get(3)?;
            let container = entry.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(entry);
            Ok((r.get(0)?, passage(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?, &container, &r.get::<_, String>(4)?)))
        })?
        .collect::<rusqlite::Result<_>>()?;
    summary.candidates = rows.len();

    let existing: std::collections::HashMap<i64, String> = cat
        .conn
        .prepare("SELECT item_id, text_sha256 FROM embeddings WHERE corpus = 'combined' AND encoder = ?1")?
        .query_map([ENCODER_ID], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let todo: Vec<(i64, String, String)> = rows
        .into_iter()
        .filter_map(|(id, text)| {
            let sha = hex_digest(&Sha256::digest(text.as_bytes()));
            if !opts.all && existing.get(&id) == Some(&sha) {
                None
            } else {
                Some((id, text, sha))
            }
        })
        .collect();
    summary.unchanged = summary.candidates - todo.len();
    if todo.is_empty() {
        summary.elapsed = started.elapsed();
        return Ok(summary);
    }

    let mut encoder = Encoder::new(opts.quiet)?;
    let total = todo.len();
    for chunk in todo.chunks(256) {
        let texts: Vec<String> = chunk.iter().map(|(_, t, _)| t.clone()).collect();
        let vectors = encoder.embed(&texts)?;
        let tx = cat.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut put = tx.prepare_cached(
                "INSERT INTO embeddings(item_id, corpus, encoder, dims, text_sha256, vector) VALUES (?1, 'combined', ?2, ?3, ?4, ?5)
                 ON CONFLICT(item_id, corpus, encoder) DO UPDATE SET text_sha256 = excluded.text_sha256, vector = excluded.vector, dims = excluded.dims",
            )?;
            for ((id, _, sha), v) in chunk.iter().zip(&vectors) {
                put.execute(params![id, ENCODER_ID, v.len() as i64, sha, to_blob(v)])?;
            }
        }
        tx.commit()?;
        summary.embedded += chunk.len();
        if !opts.quiet {
            use std::io::Write;
            eprint!("\r[{}/{total}] embedded", summary.embedded);
            let _ = std::io::stderr().flush();
        }
    }
    if !opts.quiet {
        eprint!("\r{:<40}\r", "");
    }
    cat.set_meta("embed_encoder", ENCODER_ID)?;
    cat.set_meta("embed_dims", &DIMS.to_string())?;
    summary.elapsed = started.elapsed();
    Ok(summary)
}

/// `(item id, cosine)` for the `n` nearest embedded items that pass the filters.
pub fn semantic_top(cat: &Catalog, query: &[f32], q: &SearchQuery, n: usize) -> Result<Vec<(i64, f64)>> {
    let (mut clauses, mut values) = filter_sql(q.filters);
    clauses.insert(0, "e.encoder = ?".into());
    values.insert(0, Value::Text(ENCODER_ID.into()));
    let sql = format!(
        "SELECT e.item_id, e.vector FROM embeddings e JOIN items i ON i.id = e.item_id JOIN archives a ON a.id = i.archive_id
         WHERE {}",
        clauses.join(" AND ")
    );
    let mut stmt = cat.conn.prepare(&sql)?;
    let mut scored: Vec<(i64, f64)> = stmt
        .query_map(params_from_iter(values), |r| {
            let v = from_blob(&r.get::<_, Vec<u8>>(1)?);
            let dot: f32 = v.iter().zip(query).map(|(a, b)| a * b).sum();
            Ok((r.get(0)?, dot as f64))
        })?
        .collect::<rusqlite::Result<_>>()?;
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(n);
    Ok(scored)
}

/// The ranking `search` uses for `--semantic` and `--hybrid`.
pub fn ranked(cat: &Catalog, q: &SearchQuery) -> Result<Vec<(i64, f64, &'static str)>> {
    let embedded: i64 = cat.conn.query_row("SELECT count(*) FROM embeddings WHERE encoder = ?1", [ENCODER_ID], |r| r.get(0))?;
    if embedded == 0 {
        if q.mode == Mode::Hybrid {
            // Hybrid with nothing embedded is just the word ranking; say so.
            eprintln!("note: nothing is embedded yet (run `rage catalog embed`); ranking by words only");
            return Ok(lexical_ids(cat, q.text, q.raw, q.filters, q.limit)?.into_iter().map(|(i, s)| (i, s, "lexical")).collect());
        }
        anyhow::bail!("nothing is embedded yet; run `rage catalog embed` first");
    }
    let mut encoder = Encoder::new(true)?;
    let query = encoder.embed(&[format!("query: {}", q.text)])?.remove(0);
    match q.mode {
        Mode::Semantic => Ok(semantic_top(cat, &query, q, q.limit)?.into_iter().map(|(i, s)| (i, s, "semantic")).collect()),
        _ => {
            let pool = (q.limit * 5).max(200);
            let lexical: Vec<i64> = lexical_ids(cat, q.text, q.raw, q.filters, pool)?.into_iter().map(|(i, _)| i).collect();
            let semantic: Vec<i64> = semantic_top(cat, &query, q, pool)?.into_iter().map(|(i, _)| i).collect();
            let fused = rrf(&[lexical.clone(), semantic.clone()], 60.0);
            Ok(fused
                .into_iter()
                .take(q.limit)
                .map(|(id, score)| {
                    let via = match (lexical.contains(&id), semantic.contains(&id)) {
                        (true, true) => "both",
                        (true, false) => "lexical",
                        _ => "semantic",
                    };
                    (id, score, via)
                })
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_round_trip_and_normalise() {
        let mut v = vec![3.0f32, 4.0];
        normalise(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        assert_eq!(from_blob(&to_blob(&v)), v);
    }

    #[test]
    fn passages_read_like_text() {
        assert_eq!(passage("drawable", "prop_bin_05a", "v_bins", "a dented bin"), "passage: drawable prop bin 05a (in v bins). a dented bin");
    }
}
