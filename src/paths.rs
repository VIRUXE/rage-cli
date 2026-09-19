//! Shared per-user directory layout.

use std::ffi::OsString;
use std::path::PathBuf;

/// `~/.rage-cli` — the per-user directory holding the key cache, the texture
/// index and the update-check stamp. The home directory is `HOME` first,
/// then `USERPROFILE`, so this also matches whatever shell (Git Bash/MSYS
/// included) the process is running under.
///
/// The tool was called `rpf` before 0.16 and kept its caches in
/// `~/.rpf-cli`; when that directory still exists and the new one does not,
/// it is used as is, so an upgrade never rebuilds a key or texture cache.
pub fn config_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    let current = home.join(".rage-cli");
    let legacy = home.join(".rpf-cli");
    if !current.exists() && legacy.is_dir() {
        return Some(legacy);
    }
    Some(current)
}

/// Reads `RAGE_<name>`, falling back to the pre-0.16 `RPF_<name>` spelling
/// so existing shell profiles and CI configs keep working.
pub fn env_var(name: &str) -> Option<OsString> {
    debug_assert!(name.starts_with("RAGE_"));
    std::env::var_os(name).or_else(|| std::env::var_os(name.replacen("RAGE_", "RPF_", 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_root_is_one_of_the_two_known_names() {
        let root = config_root().expect("a home directory");
        let name = root.file_name().unwrap().to_string_lossy();
        assert!(name == ".rage-cli" || name == ".rpf-cli", "{}", root.display());
    }

    #[test]
    fn env_var_falls_back_to_the_rpf_spelling() {
        // SAFETY: nothing else in the test binary reads these two variables.
        unsafe { std::env::set_var("RPF_PATHS_TEST_FALLBACK", "legacy") };
        assert_eq!(env_var("RAGE_PATHS_TEST_FALLBACK").unwrap(), "legacy");
        unsafe { std::env::set_var("RAGE_PATHS_TEST_FALLBACK", "current") };
        assert_eq!(env_var("RAGE_PATHS_TEST_FALLBACK").unwrap(), "current");
    }
}
