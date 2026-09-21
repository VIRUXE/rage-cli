//! The name table `resource info` and `resource dump` print hashes through:
//! rage-formats' built-in structure and member names, then the names
//! harvested from the game with `rage names harvest` (in `~/.rage-cli/
//! names.txt`, or wherever `RAGE_NAMES` points), then any `--names` files,
//! then the stems of the files next to the one being inspected — which is
//! how a FiveM resource's own props and maps get their names back.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rage_formats::NameTable;

/// Where the harvested list lives: `RAGE_NAMES`, else `~/.rage-cli/names.txt`.
pub fn harvest_path() -> Option<PathBuf> {
    if let Some(p) = crate::paths::env_var("RAGE_NAMES") {
        return Some(PathBuf::from(p));
    }
    Some(crate::paths::config_root()?.join("names.txt"))
}

/// The built-in names plus everything listed above. `near` is the file
/// being inspected: its siblings' stems join the table.
pub fn load(extra: &[PathBuf], near: Option<&Path>) -> Result<NameTable> {
    let mut table = NameTable::core();
    if let Some(path) = harvest_path()
        && path.is_file()
    {
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        table.add_list(&text);
    }
    for path in extra {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        table.add_list(&text);
    }
    if let Some(file) = near {
        add_siblings(&mut table, file);
    }
    Ok(table)
}

/// Adds the stems of every file in `file`'s folder (and its `stream`
/// subfolders, walked once) in both the spelling on disk and lowercase:
/// map names keep their case in `CMapData.name`, archetype names are hashed
/// lowercase.
pub fn add_siblings(table: &mut NameTable, file: &Path) {
    let Some(dir) = file.parent() else { return };
    let Ok(files) = crate::utils::walkdir(dir) else { return };
    for path in files {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            table.add(stem);
            table.add(&stem.to_lowercase());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rage_formats::rage_joaat;

    #[test]
    fn sibling_stems_join_in_both_spellings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bombaPALETO.ymap"), b"").unwrap();
        std::fs::write(dir.path().join("prop_gas_pump.ydr"), b"").unwrap();
        let mut table = NameTable::empty();
        add_siblings(&mut table, &dir.path().join("bombaPALETO.ymap"));
        assert_eq!(table.get(rage_joaat("bombaPALETO")), Some("bombaPALETO"));
        assert_eq!(table.get(rage_joaat("bombapaleto")), Some("bombapaleto"));
        assert_eq!(table.get(rage_joaat("prop_gas_pump")), Some("prop_gas_pump"));
    }
}
