//! The catalogue's SQLite schema. One `items` row per asset occurrence, the
//! archives they came from, archetype and texture-parent data joined at
//! query time, and an external-content FTS5 index over `docs`.

use anyhow::{bail, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// Bumped whenever the DDL below changes shape. An older catalogue is
/// rebuilt from scratch; a newer one is refused.
pub const SCHEMA_VERSION: i64 = 1;

pub fn configure(conn: &Connection) -> Result<()> {
    // Several rage processes may write one catalogue at once (parallel
    // `annotate` runs from a review batch); wait for the lock instead of
    // failing with "database is locked".
    conn.busy_timeout(std::time::Duration::from_secs(60))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -65536;",
    )?;
    Ok(())
}

/// Indexes added after schema 1 shipped: cheap to create on an empty
/// table, and created on first open of an older catalogue.
const EXTRA_INDEXES: &str = "CREATE INDEX IF NOT EXISTS items_kind_name ON items(kind, winner, name);";

pub fn migrate(conn: &Connection, path: &Path) -> Result<()> {
    migrate_tables(conn, path)?;
    conn.execute_batch(EXTRA_INDEXES)?;
    Ok(())
}

fn migrate_tables(conn: &Connection, path: &Path) -> Result<()> {
    let has_meta: bool = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'", [], |_| Ok(true))
        .optional()?
        .unwrap_or(false);
    if has_meta {
        let version: Option<i64> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get::<_, String>(0))
            .optional()?
            .and_then(|v| v.parse().ok());
        match version {
            Some(v) if v == SCHEMA_VERSION => return Ok(()),
            Some(v) if v > SCHEMA_VERSION => bail!(
                "catalogue {} has schema version {v}, newer than this rage understands ({SCHEMA_VERSION}); update rage or use another --db",
                path.display()
            ),
            other => {
                eprintln!(
                    "Catalogue {} is an older format (schema {}); rebuilding it from scratch.",
                    path.display(),
                    other.map(|v| v.to_string()).unwrap_or_else(|| "unknown".into())
                );
                drop_all(conn)?;
            }
        }
    }
    conn.execute_batch(DDL)?;
    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

fn drop_all(conn: &Connection) -> Result<()> {
    let mut names: Vec<(String, String)> = conn
        .prepare("SELECT type, name FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'docs_fts_%'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    names.sort_by_key(|(t, _)| if t == "view" { 0 } else { 1 });
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    for (ty, name) in names {
        conn.execute_batch(&format!("DROP {} IF EXISTS \"{}\";", ty.to_uppercase(), name))?;
    }
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(())
}

const DDL: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE archives (
  id          INTEGER PRIMARY KEY,
  path        TEXT NOT NULL UNIQUE,
  rel_path    TEXT NOT NULL,
  tier        TEXT NOT NULL CHECK (tier IN ('base','update','dlc')),
  dlc_pack    TEXT,
  load_rank   INTEGER NOT NULL,
  size        INTEGER NOT NULL,
  mtime       INTEGER NOT NULL,
  head_sha256 TEXT NOT NULL,
  scanner     INTEGER NOT NULL,
  status      TEXT NOT NULL CHECK (status IN ('pending','ok','needs_keys','failed')),
  error       TEXT,
  scanned_at  INTEGER NOT NULL,
  entries     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE items (
  id            INTEGER PRIMARY KEY,
  key           TEXT NOT NULL UNIQUE,
  asset_key     TEXT NOT NULL,
  kind          TEXT NOT NULL CHECK (kind IN ('drawable','fragment','dictionary','dd_entry','txd','texture')),
  hash          INTEGER NOT NULL,
  name          TEXT,
  archive_id    INTEGER NOT NULL REFERENCES archives(id) ON DELETE CASCADE,
  nested        TEXT NOT NULL DEFAULT '',
  inner_path    TEXT NOT NULL,
  entry_name    TEXT NOT NULL,
  member        TEXT,
  parent_id     INTEGER REFERENCES items(id) ON DELETE CASCADE,
  root_id       INTEGER,
  winner        INTEGER NOT NULL DEFAULT 0,
  file_size     INTEGER,
  mem_size      INTEGER,
  resource_version INTEGER,
  source_sha256 TEXT,
  bb_min_x REAL, bb_min_y REAL, bb_min_z REAL,
  bb_max_x REAL, bb_max_y REAL, bb_max_z REAL,
  size_x REAL, size_y REAL, size_z REAL,
  radius REAL, bounds_computed INTEGER,
  lod_high REAL, lod_med REAL, lod_low REAL, lod_vlow REAL,
  triangles INTEGER, lods INTEGER, shaders INTEGER, embedded_textures INTEGER,
  diffuse_texture TEXT,
  txd_hash INTEGER,
  width INTEGER, height INTEGER, depth INTEGER, levels INTEGER, format TEXT, bytes INTEGER,
  embedded INTEGER,
  parse_error TEXT
);
CREATE INDEX items_archive   ON items(archive_id);
CREATE INDEX items_asset     ON items(asset_key);
CREATE INDEX items_kind_hash ON items(kind, hash);
CREATE INDEX items_winner    ON items(winner, kind);
CREATE INDEX items_parent    ON items(parent_id);
CREATE INDEX items_root      ON items(root_id);
CREATE INDEX items_sha       ON items(source_sha256);

CREATE TABLE texture_refs (
  item_id        INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
  texture_hash   INTEGER NOT NULL,
  texture_name   TEXT NOT NULL,
  shader_hash    INTEGER NOT NULL,
  parameter_hash INTEGER NOT NULL,
  PRIMARY KEY (item_id, texture_hash, shader_hash, parameter_hash)
) WITHOUT ROWID;

CREATE TABLE archetypes (
  id         INTEGER PRIMARY KEY,
  archive_id INTEGER NOT NULL REFERENCES archives(id) ON DELETE CASCADE,
  ytyp_key   TEXT NOT NULL,
  name_hash  INTEGER NOT NULL,
  name       TEXT,
  bb_min_x REAL, bb_min_y REAL, bb_min_z REAL,
  bb_max_x REAL, bb_max_y REAL, bb_max_z REAL,
  lod_dist REAL,
  txd_hash INTEGER,
  drawable_dictionary_hash INTEGER,
  asset_name_hash INTEGER,
  asset_type INTEGER,
  is_mlo INTEGER,
  winner INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX archetypes_hash    ON archetypes(name_hash, winner);
CREATE INDEX archetypes_archive ON archetypes(archive_id);

CREATE TABLE txd_parents (
  child      INTEGER NOT NULL,
  parent     INTEGER NOT NULL,
  archive_id INTEGER NOT NULL REFERENCES archives(id) ON DELETE CASCADE,
  PRIMARY KEY (child, parent, archive_id)
) WITHOUT ROWID;

CREATE TABLE docs (
  item_id     INTEGER PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE,
  names       TEXT NOT NULL,
  path        TEXT NOT NULL,
  annotations TEXT NOT NULL DEFAULT ''
);
CREATE VIRTUAL TABLE docs_fts USING fts5(
  names, path, annotations,
  content = 'docs', content_rowid = 'item_id',
  tokenize = "unicode61 remove_diacritics 2 tokenchars '_'",
  prefix = '2 3 4'
);
CREATE TRIGGER docs_ai AFTER INSERT ON docs BEGIN
  INSERT INTO docs_fts(rowid, names, path, annotations) VALUES (new.item_id, new.names, new.path, new.annotations);
END;
CREATE TRIGGER docs_ad AFTER DELETE ON docs BEGIN
  INSERT INTO docs_fts(docs_fts, rowid, names, path, annotations) VALUES ('delete', old.item_id, old.names, old.path, old.annotations);
END;
CREATE TRIGGER docs_au AFTER UPDATE ON docs BEGIN
  INSERT INTO docs_fts(docs_fts, rowid, names, path, annotations) VALUES ('delete', old.item_id, old.names, old.path, old.annotations);
  INSERT INTO docs_fts(rowid, names, path, annotations) VALUES (new.item_id, new.names, new.path, new.annotations);
END;

CREATE TABLE packets (
  packet_id  TEXT PRIMARY KEY,
  created    INTEGER NOT NULL,
  game_build INTEGER,
  query      TEXT,
  filters    TEXT,
  views      TEXT NOT NULL,
  cell       INTEGER NOT NULL,
  out_dir    TEXT NOT NULL,
  sheets     INTEGER NOT NULL,
  tiles      INTEGER NOT NULL
);
CREATE TABLE packet_tiles (
  packet_id     TEXT NOT NULL REFERENCES packets(packet_id) ON DELETE CASCADE,
  tile          INTEGER NOT NULL,
  item_id       INTEGER REFERENCES items(id) ON DELETE SET NULL,
  asset_key     TEXT NOT NULL,
  sheet_file    TEXT NOT NULL,
  col           INTEGER NOT NULL,
  row           INTEGER NOT NULL,
  tile_sha256   TEXT NOT NULL,
  sheet_sha256  TEXT NOT NULL,
  render_report TEXT,
  PRIMARY KEY (packet_id, tile)
);

CREATE TABLE annotations (
  id              INTEGER PRIMARY KEY,
  item_id         INTEGER REFERENCES items(id) ON DELETE SET NULL,
  asset_key       TEXT NOT NULL,
  source_sha256   TEXT,
  game_build      INTEGER,
  method          TEXT NOT NULL CHECK (method IN ('visual','metadata','shared-visual')),
  reviewer        TEXT NOT NULL CHECK (reviewer IN ('agent','human')),
  reviewer_name   TEXT,
  packet_id       TEXT,
  tile            INTEGER,
  evidence_sha256 TEXT,
  views           TEXT,
  description     TEXT NOT NULL,
  shape           TEXT,
  material        TEXT,
  condition       TEXT,
  likely_use      TEXT,
  tags            TEXT,
  confidence      REAL,
  orientation_doubt INTEGER NOT NULL DEFAULT 0,
  missing_views   TEXT,
  limitations     TEXT,
  pack_id         TEXT,
  pack_version    TEXT,
  content_sha256  TEXT NOT NULL,
  created         INTEGER NOT NULL,
  superseded_by   INTEGER REFERENCES annotations(id)
);
CREATE UNIQUE INDEX annotations_dedupe ON annotations(content_sha256);
CREATE INDEX annotations_item  ON annotations(item_id, superseded_by);
CREATE INDEX annotations_asset ON annotations(asset_key);

CREATE TABLE embeddings (
  item_id     INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
  corpus      TEXT NOT NULL CHECK (corpus IN ('names','annotations','combined')),
  encoder     TEXT NOT NULL,
  dims        INTEGER NOT NULL,
  text_sha256 TEXT NOT NULL,
  vector      BLOB NOT NULL,
  PRIMARY KEY (item_id, corpus, encoder)
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_an_empty_database_with_fts5() {
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate(&conn, Path::new(":memory:")).unwrap();
        // Idempotent.
        migrate(&conn, Path::new(":memory:")).unwrap();
        conn.execute("INSERT INTO archives(path, rel_path, tier, load_rank, size, mtime, head_sha256, scanner, status, scanned_at) VALUES ('a','a','base',0,0,0,'',1,'ok',0)", []).unwrap();
        conn.execute("INSERT INTO items(key, asset_key, kind, hash, archive_id, inner_path, entry_name) VALUES ('k','a','drawable',1,1,'x.ydr','x.ydr')", []).unwrap();
        conn.execute("INSERT INTO docs(item_id, names, path) VALUES (1, 'prop_chair_a prop chair a', 'x64a')", []).unwrap();
        let hit: i64 = conn.query_row("SELECT rowid FROM docs_fts WHERE docs_fts MATCH '\"chair\"*'", [], |r| r.get(0)).unwrap();
        assert_eq!(hit, 1);
        // Whole underscore names are single tokens too.
        let hit: i64 = conn.query_row("SELECT rowid FROM docs_fts WHERE docs_fts MATCH '\"prop_chair\"*'", [], |r| r.get(0)).unwrap();
        assert_eq!(hit, 1);
        // Cascade keeps FTS in step.
        conn.execute("DELETE FROM archives", []).unwrap();
        let n: i64 = conn.query_row("SELECT count(*) FROM docs_fts WHERE docs_fts MATCH 'chair'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn refuses_a_newer_schema() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn, Path::new(":memory:")).unwrap();
        conn.execute("UPDATE meta SET value = '999' WHERE key = 'schema_version'", []).unwrap();
        assert!(migrate(&conn, Path::new(":memory:")).is_err());
    }
}
