//! Finding the game executable that the keys are recovered from.
//!
//! The recovery itself lives in rpf-archive, which carries the magic data the
//! NG keys come out of.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use crate::rpf::GtaKeys;

/// Derive the keys straight from a game executable, ready to use.
pub fn from_exe(exe_path: &Path) -> Result<GtaKeys> {
    GtaKeys::extract_from_exe(exe_path, None)
}

/// Recover the keys for `exe_path` and write them into `out_dir` as the
/// `gtav_*.dat` files that `--keys` reads back.
pub fn extract(exe_path: &Path, out_dir: &Path) -> Result<()> {
    GtaKeys::extract_from_exe(exe_path, Some(out_dir))?;
    Ok(())
}

/// Keys for `exe_path`, served from the per-user cache when this build of the
/// game has been seen before and extracted (and cached) otherwise. Recovery
/// costs seconds per run, so this is what `--exe`/`GTAV_PATH` goes through.
pub fn from_exe_cached_default(exe_path: &Path) -> Result<GtaKeys> {
    match default_cache_root() {
        Some(root) => from_exe_cached(exe_path, &root).map(|(keys, _)| keys),
        None => from_exe(exe_path),
    }
}

/// Like [`from_exe_cached_default`] with an explicit cache root; the flag says
/// whether the keys came out of the cache. A cache that cannot be written is
/// not an error: the keys are simply extracted every time.
pub fn from_exe_cached(exe_path: &Path, cache_root: &Path) -> Result<(GtaKeys, bool)> {
    let Some(entry) = cache_entry_for(exe_path, cache_root) else {
        return from_exe(exe_path).map(|keys| (keys, false));
    };

    if let Ok(keys) = GtaKeys::load_from_path(&entry) {
        log::debug!("keys loaded from cache {}", entry.display());
        return Ok((keys, true));
    }

    if std::fs::create_dir_all(&entry).is_err() {
        log::debug!("key cache {} is not writable; extracting without caching", entry.display());
        return from_exe(exe_path).map(|keys| (keys, false));
    }

    let keys = GtaKeys::extract_from_exe(exe_path, Some(&entry))?;
    Ok((keys, false))
}

/// `<cache_root>/<size>-<mtime>` for the executable, or None when its
/// metadata cannot be read (in which case nothing can be cached safely).
fn cache_entry_for(exe_path: &Path, cache_root: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(exe_path).ok()?;
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(cache_root.join(cache_entry_name(meta.len(), modified)))
}

/// `~/.rage-cli/keys` (the home directory is `HOME` first, then
/// `USERPROFILE`); `RAGE_KEYS_CACHE` (or the older `RPF_KEYS_CACHE`)
/// overrides it.
fn default_cache_root() -> Option<PathBuf> {
    if let Some(dir) = crate::paths::env_var("RAGE_KEYS_CACHE") {
        return Some(PathBuf::from(dir));
    }
    Some(crate::paths::config_root()?.join("keys"))
}

/// Where a cache miss for this executable is stored under the cache root:
/// a folder named after the executable's size and modification time, so a
/// game update can never be served stale keys.
pub fn cache_entry_name(size: u64, modified_secs: u64) -> String {
    format!("{size}-{modified_secs}")
}

/// Accept either the executable itself or the folder holding it.
pub fn resolve_exe(path: &Path) -> Result<PathBuf> {
    if path.is_dir() {
        for name in ["GTA5.exe", "GTA5_Enhanced.exe"] {
            let candidate = path.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        bail!("no GTA5.exe found in {}", path.display());
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_entry_name_is_size_and_mtime() {
        assert_eq!(cache_entry_name(123, 456), "123-456");
    }

    #[test]
    fn default_cache_lives_in_a_dot_folder_under_home() {
        // `.rpf-cli` is the pre-0.16 name, still used when only it exists.
        let root = default_cache_root().expect("a home directory");
        assert!(
            root.ends_with(Path::new(".rage-cli").join("keys"))
                || root.ends_with(Path::new(".rpf-cli").join("keys")),
            "{}", root.display()
        );
    }

    #[test]
    fn keys_are_extracted_once_then_served_from_cache() {
        let Ok(gtav) = std::env::var("GTAV_PATH") else {
            println!("GTAV_PATH not set; skipping key cache test");
            return;
        };
        let exe = resolve_exe(Path::new(&gtav)).unwrap();
        let cache = tempfile::tempdir().unwrap();

        let (_, hit) = from_exe_cached(&exe, cache.path()).unwrap();
        assert!(!hit, "first load must extract");

        let entries: Vec<_> = std::fs::read_dir(cache.path()).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "one cache entry expected");
        assert!(entries[0].path().join("gtav_ng_decrypt_tables.dat").is_file());

        let (_, hit) = from_exe_cached(&exe, cache.path()).unwrap();
        assert!(hit, "second load must come from the cache");
    }

    #[test]
    fn unwritable_cache_falls_back_to_extraction() {
        let Ok(gtav) = std::env::var("GTAV_PATH") else { return };
        let exe = resolve_exe(Path::new(&gtav)).unwrap();
        // A file where the cache root should be: nothing can be created under it.
        let tmp = tempfile::tempdir().unwrap();
        let not_a_dir = tmp.path().join("blocker");
        std::fs::write(&not_a_dir, b"x").unwrap();

        let (_, hit) = from_exe_cached(&exe, &not_a_dir).unwrap();
        assert!(!hit);
    }
}
