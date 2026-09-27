//! A searchable catalogue of every drawable and texture in a game install,
//! kept in SQLite so agents (and people) can find assets by what they are
//! called, where they live, how big they are and, once reviewed, what they
//! look like. See `commands/catalog.rs` for the CLI and AGENTS.md for the
//! review loop.

pub mod annotate;
#[cfg(feature = "semantic")]
pub mod embed;
pub mod loader;
pub mod pack;
pub mod schema;
pub mod scan;
pub mod search;
pub mod sheet;
pub mod tokens;

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// What a catalogue row is. Containers (`drawable`, `fragment`,
/// `dictionary`, `txd`) are files in an archive; `dd_entry` is one drawable
/// inside a `.ydd` (or an extra drawable of a `.yft`), and `texture` is one
/// texture inside a `.ytd` or embedded in a drawable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Drawable,
    Fragment,
    Dictionary,
    DdEntry,
    Txd,
    Texture,
}

impl Kind {
    pub const ALL: [Kind; 6] = [Kind::Drawable, Kind::Fragment, Kind::Dictionary, Kind::DdEntry, Kind::Txd, Kind::Texture];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Drawable => "drawable",
            Kind::Fragment => "fragment",
            Kind::Dictionary => "dictionary",
            Kind::DdEntry => "dd_entry",
            Kind::Txd => "txd",
            Kind::Texture => "texture",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Kinds `screenshot` can draw.
    pub fn is_model(self) -> bool {
        matches!(self, Kind::Drawable | Kind::Fragment | Kind::Dictionary | Kind::DdEntry)
    }

    /// Expands a user-facing kind name or file-type alias (`ydr`, `model`,
    /// `tex`, ...) to the kinds it covers.
    pub fn expand(alias: &str) -> Result<Vec<Kind>> {
        let a = alias.trim().to_ascii_lowercase();
        let kinds = match a.as_str() {
            "ydr" => vec![Kind::Drawable],
            "ydd" => vec![Kind::Dictionary, Kind::DdEntry],
            "yft" => vec![Kind::Fragment],
            "ytd" => vec![Kind::Txd],
            "tex" | "textures" => vec![Kind::Texture],
            "model" | "models" => vec![Kind::Drawable, Kind::Fragment, Kind::DdEntry],
            other => match Kind::parse(other) {
                Some(k) => vec![k],
                None => bail!(
                    "unknown kind '{alias}' (expected drawable, fragment, dictionary, dd_entry, txd, texture, or ydr/ydd/yft/ytd/model/tex)"
                ),
            },
        };
        Ok(kinds)
    }
}

/// Lowercase 8-digit hex, the form hashes take inside keys.
pub fn hex8(hash: u32) -> String {
    format!("{hash:08x}")
}

/// The identity shared by every copy of one asset, whichever archive it is in.
/// Textures are identified by their dictionary and their own name hash.
pub fn asset_key(kind: Kind, hash: u32, txd_hash: Option<u32>) -> String {
    match (kind, txd_hash) {
        (Kind::Texture, Some(txd)) => format!("gta5:texture:{}:{}", hex8(txd), hex8(hash)),
        _ => format!("gta5:{}:{}", kind.as_str(), hex8(hash)),
    }
}

/// An open catalogue database.
pub struct Catalog {
    pub conn: Connection,
    pub path: PathBuf,
}

impl Catalog {
    /// Opens (creating when missing) the catalogue at `path` and brings its
    /// schema up to date.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("failed to open catalogue {}", path.display()))?;
        schema::configure(&conn)?;
        schema::migrate(&conn, path)?;
        Ok(Catalog { conn, path: path.to_path_buf() })
    }

    /// Opens a catalogue that must already have been built.
    pub fn open_existing(path: &Path) -> Result<Self> {
        if !path.is_file() {
            bail!("no catalogue at {}; run `rage catalog build` first", path.display());
        }
        Self::open(path)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }
}

/// Where the catalogue lives: `--db`/`RAGE_CATALOG` when given, else
/// `~/.rage-cli/catalog/<game build>/catalog.sqlite` for this executable.
pub fn resolve_path(db: Option<&Path>, exe: Option<&Path>) -> Result<PathBuf> {
    if let Some(db) = db {
        return Ok(db.to_path_buf());
    }
    let exe = exe.context("--db (or RAGE_CATALOG) or --exe (or GTAV_PATH) is required to find the catalogue")?;
    let exe = crate::keys::resolve_exe(exe)?;
    let build = crate::keys::build_key(&exe).with_context(|| format!("failed to stat {}", exe.display()))?;
    let root = crate::paths::config_root().context("no home directory to keep the catalogue in; pass --db")?;
    Ok(root.join("catalog").join(build).join("catalog.sqlite"))
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `3m12s`, `41.2s`, `850ms`.
pub fn human_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 60.0 {
        format!("{}m{:02}s", (secs / 60.0) as u64, (secs % 60.0) as u64)
    } else if secs >= 1.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// `1,234,567`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_aliases_expand() {
        assert_eq!(Kind::expand("ydd").unwrap(), vec![Kind::Dictionary, Kind::DdEntry]);
        assert_eq!(Kind::expand("model").unwrap(), vec![Kind::Drawable, Kind::Fragment, Kind::DdEntry]);
        assert_eq!(Kind::expand("texture").unwrap(), vec![Kind::Texture]);
        assert!(Kind::expand("ymap").is_err());
    }

    #[test]
    fn asset_keys_are_stable() {
        assert_eq!(asset_key(Kind::Drawable, 0x3A1F09E2, None), "gta5:drawable:3a1f09e2");
        assert_eq!(asset_key(Kind::Texture, 1, Some(2)), "gta5:texture:00000002:00000001");
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(12), "12");
        assert_eq!(human_duration(std::time::Duration::from_secs(192)), "3m12s");
    }
}
