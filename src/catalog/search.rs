//! `catalog search`: FTS5 over names, paths and annotations, with structured
//! filters, and hits carrying enough provenance to act on directly.

use anyhow::{bail, Result};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, OptionalExtension};

use super::tokens::fts_query;
use super::{hex8, Catalog, Kind};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeSpec {
    /// The largest extent (metres for models, pixels for textures).
    Extent(f64),
    /// Per-axis, compared largest-to-smallest against the item's sorted extents.
    Box([f64; 3]),
}

pub fn parse_size_spec(s: &str) -> std::result::Result<SizeSpec, String> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    let nums: Vec<f64> = parts
        .iter()
        .map(|p| p.parse::<f64>().map_err(|_| format!("'{s}' is not a size: use N or X,Y,Z")))
        .collect::<std::result::Result<_, _>>()?;
    if nums.iter().any(|n| !n.is_finite() || *n < 0.0) {
        return Err(format!("'{s}': sizes must be non-negative numbers"));
    }
    match nums.as_slice() {
        [n] => Ok(SizeSpec::Extent(*n)),
        [a, b, c] => {
            let mut v = [*a, *b, *c];
            v.sort_by(|x, y| y.partial_cmp(x).unwrap());
            Ok(SizeSpec::Box(v))
        }
        _ => Err(format!("'{s}' is not a size: use N or X,Y,Z")),
    }
}

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub kinds: Vec<Kind>,
    /// Pack names, or the tiers `base`/`update`/`dlc`.
    pub dlc: Vec<String>,
    pub min_size: Option<SizeSpec>,
    pub max_size: Option<SizeSpec>,
    pub all_copies: bool,
    pub annotated_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lexical,
    Semantic,
    Hybrid,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Lexical => "lexical",
            Mode::Semantic => "semantic",
            Mode::Hybrid => "hybrid",
        }
    }
}

pub struct SearchQuery<'a> {
    pub text: &'a str,
    pub raw: bool,
    pub filters: &'a Filters,
    pub limit: usize,
    pub mode: Mode,
}

#[derive(Debug, Clone)]
pub struct ItemView {
    pub id: i64,
    pub key: String,
    pub asset_key: String,
    pub kind: Kind,
    pub hash: u32,
    pub name: Option<String>,
    pub winner: bool,
    pub archive_path: String,
    pub archive_rel: String,
    pub tier: String,
    pub dlc_pack: Option<String>,
    pub nested: Vec<String>,
    pub inner_path: String,
    pub entry_name: String,
    pub member: Option<String>,
    pub parent_member: Option<String>,
    pub bb_min: Option<[f64; 3]>,
    pub bb_max: Option<[f64; 3]>,
    pub size: Option<[f64; 3]>,
    pub bounds_computed: bool,
    pub lods: Option<[f64; 4]>,
    pub triangles: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub levels: Option<i64>,
    pub format: Option<String>,
    pub embedded: bool,
    pub txd_hash: Option<u32>,
    pub diffuse: Option<String>,
    pub source_sha256: Option<String>,
    pub parse_error: Option<String>,
}

impl ItemView {
    /// The name to show: the resolved name, else the member, else the hash.
    pub fn label(&self) -> String {
        let own = self.name.clone().or_else(|| self.member.clone()).unwrap_or_else(|| format!("0x{}", hex8(self.hash)));
        match self.kind {
            // A member means little on its own (`damaged`, `diffuse`): say whose it is.
            Kind::DdEntry | Kind::Texture => {
                let stem = self.entry_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&self.entry_name);
                format!("{stem}#{own}")
            }
            _ => own,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArchetypeView {
    pub ytyp: String,
    pub bb_min: [f64; 3],
    pub bb_max: [f64; 3],
    pub lod_dist: f64,
    pub asset_type: i64,
    pub txd_hash: u32,
    pub is_mlo: bool,
}

#[derive(Debug, Clone)]
pub struct AnnotationView {
    pub method: String,
    pub reviewer: String,
    pub reviewer_name: Option<String>,
    pub views: Option<String>,
    pub description: String,
    pub tags: Vec<String>,
    pub confidence: Option<f64>,
    pub created: i64,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub item: ItemView,
    pub score: f64,
    pub via: &'static str,
    pub archetype: Option<ArchetypeView>,
    pub referenced: Vec<String>,
    pub txd_chain: Vec<(u32, Option<String>)>,
    pub annotations: Vec<AnnotationView>,
}

#[derive(Debug, Clone, Default)]
pub struct Coverage {
    pub results: usize,
    pub results_annotated: usize,
    pub catalogue_items: u64,
    pub catalogue_annotated: u64,
    pub embedded: Option<u64>,
}

/// SQL `WHERE` fragments and their parameters for `filters`, over `items i`
/// joined to `archives a`.
pub fn filter_sql(filters: &Filters) -> (Vec<String>, Vec<Value>) {
    let mut clauses = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    if !filters.kinds.is_empty() {
        clauses.push(format!("i.kind IN ({})", vec!["?"; filters.kinds.len()].join(", ")));
        values.extend(filters.kinds.iter().map(|k| Value::Text(k.as_str().to_string())));
    }
    if !filters.dlc.is_empty() {
        let mut ors = Vec::new();
        for d in &filters.dlc {
            let d = d.to_lowercase();
            if matches!(d.as_str(), "base" | "update" | "dlc") {
                ors.push("a.tier = ?");
            } else {
                ors.push("a.dlc_pack = ?");
            }
            values.push(Value::Text(d));
        }
        clauses.push(format!("({})", ors.join(" OR ")));
    }
    if !filters.all_copies {
        clauses.push("i.winner = 1".to_string());
    }
    let big = "(CASE WHEN i.kind = 'texture' THEN MAX(i.width, i.height) ELSE MAX(i.size_x, i.size_y, i.size_z) END)";
    let small = "(CASE WHEN i.kind = 'texture' THEN MIN(i.width, i.height) ELSE MIN(i.size_x, i.size_y, i.size_z) END)";
    let mid = "(CASE WHEN i.kind = 'texture' THEN MIN(i.width, i.height) ELSE (i.size_x + i.size_y + i.size_z - MAX(i.size_x, i.size_y, i.size_z) - MIN(i.size_x, i.size_y, i.size_z)) END)";
    let mut size = |spec: &SizeSpec, op: &str| match spec {
        SizeSpec::Extent(v) => {
            clauses.push(format!("{big} {op} ?"));
            values.push(Value::Real(*v));
        }
        SizeSpec::Box([a, b, c]) => {
            clauses.push(format!("{big} {op} ? AND {mid} {op} ? AND {small} {op} ?"));
            values.extend([Value::Real(*a), Value::Real(*b), Value::Real(*c)]);
        }
    };
    if let Some(s) = &filters.min_size {
        size(s, ">=");
    }
    if let Some(s) = &filters.max_size {
        size(s, "<=");
    }
    if filters.annotated_only {
        clauses.push(
            "EXISTS (SELECT 1 FROM annotations n WHERE n.item_id = i.id AND n.superseded_by IS NULL AND n.method IN ('visual','shared-visual'))"
                .to_string(),
        );
    }
    (clauses, values)
}

/// Lexical candidates: `(item id, bm25 score)`, best first.
pub fn lexical_ids(cat: &Catalog, text: &str, raw: bool, filters: &Filters, limit: usize) -> Result<Vec<(i64, f64)>> {
    let (mut clauses, mut values) = filter_sql(filters);
    let sql = match fts_query(text, raw) {
        Some(q) => {
            clauses.insert(0, "docs_fts MATCH ?".to_string());
            values.insert(0, Value::Text(q));
            values.push(Value::Integer(limit as i64));
            format!(
                "SELECT i.id, bm25(docs_fts, 10.0, 1.0, 6.0) AS score
                 FROM docs_fts JOIN items i ON i.id = docs_fts.rowid JOIN archives a ON a.id = i.archive_id
                 WHERE {} ORDER BY score, i.winner DESC, i.id LIMIT ?",
                clauses.join(" AND ")
            )
        }
        None => {
            if !text.trim().is_empty() {
                bail!("nothing searchable in '{text}'");
            }
            // Listing by name: one index walk per kind (kind, winner, name),
            // merged, instead of sorting every row of the kind.
            let kinds: Vec<Kind> = if filters.kinds.is_empty() { Kind::ALL.to_vec() } else { filters.kinds.clone() };
            let mut per_kind = Vec::new();
            let mut all_values = Vec::new();
            let mut plain = filters.clone();
            plain.kinds.clear();
            let (rest, rest_values) = filter_sql(&plain);
            for k in kinds {
                let mut clauses = vec!["i.kind = ?".to_string()];
                clauses.extend(rest.iter().cloned());
                all_values.push(Value::Text(k.as_str().to_string()));
                all_values.extend(rest_values.iter().cloned());
                all_values.push(Value::Integer(limit as i64));
                per_kind.push(format!(
                    "SELECT * FROM (SELECT i.id, 0.0 AS score, i.name AS name FROM items i JOIN archives a ON a.id = i.archive_id
                     WHERE {} ORDER BY i.name, i.id LIMIT ?)",
                    clauses.join(" AND ")
                ));
            }
            values = all_values;
            values.push(Value::Integer(limit as i64));
            format!("SELECT id, score FROM ({}) ORDER BY name, id LIMIT ?", per_kind.join(" UNION ALL "))
        }
    };
    let mut stmt = cat.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?;
    let out = rows.collect::<rusqlite::Result<Vec<_>>>();
    match out {
        Ok(v) => Ok(v),
        Err(e) if raw => bail!("invalid FTS5 query '{text}': {e}"),
        Err(e) => Err(e.into()),
    }
}

/// Reciprocal-rank fusion: each list contributes `1 / (k + rank)` for every
/// id it holds (rank from 1). Highest fused score first; ties by first
/// appearance.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
pub fn rrf(lists: &[Vec<i64>], k: f64) -> Vec<(i64, f64)> {
    let mut scores: Vec<(i64, f64)> = Vec::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            let add = 1.0 / (k + rank as f64 + 1.0);
            match scores.iter_mut().find(|(i, _)| i == id) {
                Some((_, s)) => *s += add,
                None => scores.push((*id, add)),
            }
        }
    }
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scores
}

pub fn search(cat: &Catalog, q: &SearchQuery) -> Result<(Vec<Hit>, Coverage)> {
    let ranked: Vec<(i64, f64, &'static str)> = match q.mode {
        Mode::Lexical => lexical_ids(cat, q.text, q.raw, q.filters, q.limit)?
            .into_iter()
            .map(|(id, s)| (id, s, "lexical"))
            .collect(),
        Mode::Semantic | Mode::Hybrid => semantic_ranked(cat, q)?,
    };
    let mut hits = Vec::with_capacity(ranked.len());
    for (id, score, via) in ranked {
        let item = load_item(cat, id)?;
        hits.push(hydrate(cat, item, score, via)?);
    }
    let coverage = coverage(cat, &hits)?;
    Ok((hits, coverage))
}

#[cfg(feature = "semantic")]
fn semantic_ranked(cat: &Catalog, q: &SearchQuery) -> Result<Vec<(i64, f64, &'static str)>> {
    super::embed::ranked(cat, q)
}

#[cfg(not(feature = "semantic"))]
fn semantic_ranked(_cat: &Catalog, _q: &SearchQuery) -> Result<Vec<(i64, f64, &'static str)>> {
    bail!("this binary was built without the semantic feature (cargo install rage-cli --features semantic)")
}

pub fn coverage(cat: &Catalog, hits: &[Hit]) -> Result<Coverage> {
    let results_annotated = hits
        .iter()
        .filter(|h| h.annotations.iter().any(|a| a.method == "visual" || a.method == "shared-visual"))
        .count();
    // Counted at build time: a full count of winners costs tens of ms per search.
    let catalogue_items: i64 = match cat.meta("winner_count")?.and_then(|v| v.parse().ok()) {
        Some(n) => n,
        None => cat.conn.query_row("SELECT count(*) FROM items WHERE winner = 1", [], |r| r.get(0))?,
    };
    // Driven from annotations (CROSS JOIN fixes the order): it is the small side.
    let catalogue_annotated: i64 = cat.conn.query_row(
        "SELECT count(DISTINCT n.item_id) FROM annotations n CROSS JOIN items i ON i.id = n.item_id
         WHERE i.winner = 1 AND n.superseded_by IS NULL AND n.method IN ('visual','shared-visual')",
        [],
        |r| r.get(0),
    )?;
    let embedded: Option<i64> = match cat.meta("embed_encoder")? {
        Some(enc) => Some(cat.conn.query_row(
            "SELECT count(DISTINCT e.item_id) FROM embeddings e JOIN items i ON i.id = e.item_id WHERE i.winner = 1 AND e.encoder = ?1",
            [enc],
            |r| r.get(0),
        )?),
        None => None,
    };
    Ok(Coverage {
        results: hits.len(),
        results_annotated,
        catalogue_items: catalogue_items as u64,
        catalogue_annotated: catalogue_annotated as u64,
        embedded: embedded.map(|n| n as u64),
    })
}

const ITEM_COLUMNS: &str = "i.id, i.key, i.asset_key, i.kind, i.hash, i.name, i.winner, a.path, a.rel_path, a.tier, a.dlc_pack,
    i.nested, i.inner_path, i.entry_name, i.member, p.member,
    i.bb_min_x, i.bb_min_y, i.bb_min_z, i.bb_max_x, i.bb_max_y, i.bb_max_z, i.size_x, i.size_y, i.size_z, i.bounds_computed,
    i.lod_high, i.lod_med, i.lod_low, i.lod_vlow, i.triangles, i.width, i.height, i.levels, i.format, i.embedded, i.txd_hash,
    i.diffuse_texture, i.source_sha256, i.parse_error";

fn row_to_item(r: &rusqlite::Row) -> rusqlite::Result<ItemView> {
    let f3 = |a: usize| -> rusqlite::Result<Option<[f64; 3]>> {
        let x: Option<f64> = r.get(a)?;
        let y: Option<f64> = r.get(a + 1)?;
        let z: Option<f64> = r.get(a + 2)?;
        Ok(match (x, y, z) {
            (Some(x), Some(y), Some(z)) => Some([x, y, z]),
            _ => None,
        })
    };
    let kind: String = r.get(3)?;
    let nested: String = r.get(11)?;
    let lod: Option<f64> = r.get(26)?;
    Ok(ItemView {
        id: r.get(0)?,
        key: r.get(1)?,
        asset_key: r.get(2)?,
        kind: Kind::parse(&kind).unwrap_or(Kind::Drawable),
        hash: r.get::<_, i64>(4)? as u32,
        name: r.get(5)?,
        winner: r.get::<_, i64>(6)? != 0,
        archive_path: r.get(7)?,
        archive_rel: r.get(8)?,
        tier: r.get(9)?,
        dlc_pack: r.get(10)?,
        nested: if nested.is_empty() { Vec::new() } else { nested.split("//").map(str::to_string).collect() },
        inner_path: r.get(12)?,
        entry_name: r.get(13)?,
        member: r.get(14)?,
        parent_member: r.get(15)?,
        bb_min: f3(16)?,
        bb_max: f3(19)?,
        size: f3(22)?,
        bounds_computed: r.get::<_, Option<i64>>(25)?.unwrap_or(0) != 0,
        lods: match lod {
            Some(h) => Some([h, r.get::<_, Option<f64>>(27)?.unwrap_or(0.0), r.get::<_, Option<f64>>(28)?.unwrap_or(0.0), r.get::<_, Option<f64>>(29)?.unwrap_or(0.0)]),
            None => None,
        },
        triangles: r.get(30)?,
        width: r.get(31)?,
        height: r.get(32)?,
        levels: r.get(33)?,
        format: r.get(34)?,
        embedded: r.get::<_, Option<i64>>(35)?.unwrap_or(0) != 0,
        txd_hash: r.get::<_, Option<i64>>(36)?.map(|v| v as u32),
        diffuse: r.get(37)?,
        source_sha256: r.get(38)?,
        parse_error: r.get(39)?,
    })
}

pub fn load_item(cat: &Catalog, id: i64) -> Result<ItemView> {
    let sql = format!(
        "SELECT {ITEM_COLUMNS} FROM items i JOIN archives a ON a.id = i.archive_id LEFT JOIN items p ON p.id = i.parent_id WHERE i.id = ?1"
    );
    Ok(cat.conn.query_row(&sql, [id], row_to_item)?)
}

/// An item by its catalogue key or numeric id.
pub fn find_item(cat: &Catalog, key_or_id: &str) -> Result<ItemView> {
    let sql = format!(
        "SELECT {ITEM_COLUMNS} FROM items i JOIN archives a ON a.id = i.archive_id LEFT JOIN items p ON p.id = i.parent_id WHERE i.key = ?1"
    );
    if let Some(item) = cat.conn.query_row(&sql, [key_or_id], row_to_item).optional()? {
        return Ok(item);
    }
    if let Ok(id) = key_or_id.parse::<i64>()
        && let Ok(item) = load_item(cat, id)
    {
        return Ok(item);
    }
    bail!("no catalogue item with key or id '{key_or_id}'")
}

pub fn hydrate(cat: &Catalog, item: ItemView, score: f64, via: &'static str) -> Result<Hit> {
    let archetype = if item.kind.is_model() {
        cat.conn
            .query_row(
                "SELECT ytyp_key, bb_min_x, bb_min_y, bb_min_z, bb_max_x, bb_max_y, bb_max_z, lod_dist, asset_type, txd_hash, is_mlo
                 FROM archetypes WHERE name_hash = ?1 AND winner = 1 LIMIT 1",
                [item.hash as i64],
                |r| {
                    Ok(ArchetypeView {
                        ytyp: r.get(0)?,
                        bb_min: [r.get(1)?, r.get(2)?, r.get(3)?],
                        bb_max: [r.get(4)?, r.get(5)?, r.get(6)?],
                        lod_dist: r.get(7)?,
                        asset_type: r.get(8)?,
                        txd_hash: r.get::<_, i64>(9)? as u32,
                        is_mlo: r.get::<_, i64>(10)? != 0,
                    })
                },
            )
            .optional()?
    } else {
        None
    };
    let referenced: Vec<String> = {
        let mut stmt = cat.conn.prepare_cached(
            "SELECT DISTINCT texture_name FROM texture_refs WHERE item_id = ?1 AND texture_name <> '' ORDER BY texture_name",
        )?;
        stmt.query_map([item.id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let start = archetype.as_ref().map(|a| a.txd_hash).filter(|h| *h != 0).or(item.txd_hash);
    let txd_chain = match (item.kind, start) {
        (Kind::Texture | Kind::Txd, _) | (_, None) => Vec::new(),
        (_, Some(h)) => txd_chain(cat, h)?,
    };
    let annotations = annotations_for(cat, item.id)?;
    Ok(Hit { item, score, via, archetype, referenced, txd_chain, annotations })
}

/// A texture dictionary and its parents, first-wins by earliest load order
/// (as `gtxd.meta` merges), capped at 64 hops and cycle-safe.
pub fn txd_chain(cat: &Catalog, start: u32) -> Result<Vec<(u32, Option<String>)>> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut parent_of = cat.conn.prepare_cached(
        "SELECT t.parent FROM txd_parents t JOIN archives a ON a.id = t.archive_id WHERE t.child = ?1 ORDER BY a.load_rank, a.id LIMIT 1",
    )?;
    let mut name_of = cat.conn.prepare_cached("SELECT name FROM items WHERE kind = 'txd' AND hash = ?1 ORDER BY winner DESC LIMIT 1")?;
    let mut cur = Some(start);
    while let Some(h) = cur {
        if !seen.insert(h) || chain.len() >= 64 {
            break;
        }
        let name: Option<String> = name_of.query_row([h as i64], |r| r.get(0)).optional()?.flatten();
        chain.push((h, name));
        cur = parent_of.query_row([h as i64], |r| r.get::<_, i64>(0)).optional()?.map(|v| v as u32);
    }
    Ok(chain)
}

pub fn annotations_for(cat: &Catalog, item_id: i64) -> Result<Vec<AnnotationView>> {
    let mut stmt = cat.conn.prepare_cached(
        "SELECT method, reviewer, reviewer_name, views, description, tags, confidence, created FROM annotations
         WHERE item_id = ?1 AND superseded_by IS NULL
         ORDER BY reviewer = 'human' DESC, CASE method WHEN 'visual' THEN 0 WHEN 'shared-visual' THEN 1 ELSE 2 END, created DESC",
    )?;
    let rows = stmt.query_map(params![item_id], |r| {
        let tags: Option<String> = r.get(5)?;
        Ok(AnnotationView {
            method: r.get(0)?,
            reviewer: r.get(1)?,
            reviewer_name: r.get(2)?,
            views: r.get(3)?,
            description: r.get(4)?,
            tags: tags.map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default(),
            confidence: r.get(6)?,
            created: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ─── Output ────────────────────────────────────────────────────────────────

/// Quotes `s` for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:=@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Ready-to-run follow-up commands for a hit. Commands that cannot reach
/// an entry inside a nested archive are left out; `get` always works.
pub fn follow_up(item: &ItemView, db: &str) -> json::JsonValue {
    let mut cmds = json::JsonValue::new_object();
    let direct = item.nested.is_empty();
    let archive = sh_quote(&item.archive_path);
    let inner = sh_quote(&item.inner_path);
    if direct && item.kind.is_model() {
        let entry = match (item.kind, &item.member) {
            (Kind::DdEntry, Some(m)) => format!(" --entry {}", sh_quote(m.split('~').next().unwrap_or(m))),
            _ => String::new(),
        };
        cmds["screenshot"] = format!("rage screenshot {archive} {inner}{entry} --views front,iso,top --grid").into();
    }
    if direct && (item.kind == Kind::Texture || item.kind == Kind::Txd || item.kind.is_model()) {
        cmds["textures"] = format!("rage textures {archive} {inner}").into();
    }
    if direct {
        cmds["info"] = format!("rage resource info {inner} --archive {archive}").into();
    }
    let rpf = if direct { "" } else { " --rpf" };
    cmds["get"] = format!("rage catalog --db {} get {} -o DIR{rpf}", sh_quote(db), sh_quote(&item.key)).into();
    cmds
}

fn f3(v: [f64; 3]) -> json::JsonValue {
    json::array![round3(v[0]), round3(v[1]), round3(v[2])]
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

pub fn hit_json(hit: &Hit, db: &str) -> json::JsonValue {
    let i = &hit.item;
    let mut o = json::object! {
        id: i.id,
        key: i.key.clone(),
        asset_key: i.asset_key.clone(),
        kind: i.kind.as_str(),
        hash: format!("0x{:08X}", i.hash),
        name: i.name.clone(),
        winner: i.winner,
        archive: json::object! {
            path: i.archive_path.clone(),
            rel: i.archive_rel.clone(),
            tier: i.tier.clone(),
            dlc_pack: i.dlc_pack.clone(),
            nested: i.nested.clone(),
        },
        inner_path: i.inner_path.clone(),
        member: i.member.clone(),
    };
    if let Some(pm) = &i.parent_member {
        o["parent_member"] = pm.clone().into();
    }
    if let (Some(min), Some(max), Some(size)) = (i.bb_min, i.bb_max, i.size) {
        o["bounds"] = json::object! { min: f3(min), max: f3(max), size: f3(size), computed: i.bounds_computed };
    }
    if let Some(l) = i.lods {
        o["lod_distances"] = json::array![round3(l[0]), round3(l[1]), round3(l[2]), round3(l[3])];
    }
    if let Some(t) = i.triangles {
        o["triangles"] = t.into();
    }
    if i.kind == Kind::Texture {
        o["texture"] = json::object! {
            width: i.width, height: i.height, levels: i.levels, format: i.format.clone(), embedded: i.embedded,
        };
    }
    if i.kind.is_model() {
        let chain: Vec<json::JsonValue> = hit
            .txd_chain
            .iter()
            .map(|(h, n)| json::object! { hash: format!("0x{h:08X}"), name: n.clone() })
            .collect();
        o["textures"] = json::object! {
            diffuse: i.diffuse.clone(),
            referenced: hit.referenced.clone(),
            txd_chain: chain,
        };
    }
    if let Some(a) = &hit.archetype {
        o["archetype"] = json::object! {
            ytyp: a.ytyp.clone(), bb_min: f3(a.bb_min), bb_max: f3(a.bb_max), lod_dist: round3(a.lod_dist),
            asset_type: a.asset_type, txd: format!("0x{:08X}", a.txd_hash), is_mlo: a.is_mlo,
        };
    }
    if let Some(sha) = &i.source_sha256 {
        o["source_sha256"] = sha.clone().into();
    }
    if let Some(e) = &i.parse_error {
        o["parse_error"] = e.clone().into();
    }
    let anns: Vec<json::JsonValue> = hit
        .annotations
        .iter()
        .map(|a| json::object! {
            method: a.method.clone(), reviewer: a.reviewer.clone(), reviewer_name: a.reviewer_name.clone(),
            views: a.views.clone(), description: a.description.clone(), tags: a.tags.clone(),
            confidence: a.confidence, created: a.created,
        })
        .collect();
    o["annotations"] = anns.into();
    o["score"] = round3(hit.score).into();
    o["via"] = hit.via.into();
    o["commands"] = follow_up(i, db);
    o
}

pub fn coverage_json(c: &Coverage) -> json::JsonValue {
    json::object! {
        results: c.results,
        results_annotated: c.results_annotated,
        catalogue_items: c.catalogue_items,
        catalogue_annotated: c.catalogue_annotated,
        embedded: c.embedded,
    }
}

/// `3 of 12 results have a visual description; 6,500 of 770,000 catalogue items are annotated`.
pub fn coverage_line(c: &Coverage) -> String {
    let mut s = format!(
        "{} of {} result(s) have a visual description; {} of {} catalogue items are annotated",
        c.results_annotated,
        c.results,
        super::thousands(c.catalogue_annotated),
        super::thousands(c.catalogue_items)
    );
    if let Some(e) = c.embedded {
        s.push_str(&format!(", {} embedded", super::thousands(e)));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size_spec("2.5").unwrap(), SizeSpec::Extent(2.5));
        assert_eq!(parse_size_spec("1, 3,2").unwrap(), SizeSpec::Box([3.0, 2.0, 1.0]));
        assert!(parse_size_spec("1,2").is_err());
        assert!(parse_size_spec("-1").is_err());
        assert!(parse_size_spec("big").is_err());
    }

    #[test]
    fn rrf_orders_shared_hits_first() {
        let fused = rrf(&[vec![1, 2, 3], vec![3, 4, 1]], 60.0);
        let ids: Vec<i64> = fused.iter().map(|(i, _)| *i).collect();
        // 1 is ranked 1st and 3rd, 3 is 3rd and 1st: tied on score, ahead of singles.
        assert_eq!(&ids[..2], &[1, 3]);
        assert!(fused[0].1 > fused[2].1);
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn filters_build_sql() {
        let f = Filters { kinds: vec![Kind::Texture], dlc: vec!["base".into(), "mpheist".into()], min_size: Some(SizeSpec::Extent(512.0)), ..Default::default() };
        let (clauses, values) = filter_sql(&f);
        let sql = clauses.join(" AND ");
        assert!(sql.contains("i.kind IN (?)"));
        assert!(sql.contains("(a.tier = ? OR a.dlc_pack = ?)"));
        assert!(sql.contains("i.winner = 1"));
        assert_eq!(values.len(), 4);
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("x64c.rpf"), "x64c.rpf");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote("gta5:drawable:1@x.rpf//a.ydr#m"), "'gta5:drawable:1@x.rpf//a.ydr#m'");
    }
}
