//! The name table `resource info` and `resource dump` print hashes through:
//! rage-formats' built-in structure and member names, then the names
//! harvested from the game with `rage names harvest` (in `~/.rage-cli/
//! names.txt`, or wherever `RAGE_NAMES` points), then any `--names` files,
//! then the stems of the files next to the one being inspected — which is
//! how a FiveM resource's own props and maps get their names back.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rage_formats::NameTable;

/// What the list on disk says about itself, from its `#` header: where the
/// names came from and the newest game build any of them is known to
/// cover. A name absent from a list older than the file being read may be
/// newer than the list rather than missing from the game.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ListHeader {
    /// One line per `harvest`/`fetch` that contributed, oldest first.
    pub sources: Vec<String>,
    /// The game build the list covers, when any source stated one.
    pub build: Option<u32>,
}

impl ListHeader {
    /// The header lines of `text` (`# source: ...`, `# build: N`).
    pub fn parse(text: &str) -> Self {
        let mut header = Self::default();
        for line in text.lines().take_while(|l| l.trim().is_empty() || l.starts_with('#')) {
            if let Some(source) = line.strip_prefix("# source:") {
                header.sources.push(source.trim().to_owned());
            } else if line.starts_with("# Names harvested from the game's own files") {
                // The one-line header lists before 0.19 were written with.
                header.sources.push("harvest by an earlier rage (game build not recorded)".to_owned());
            } else if let Some(build) = line.strip_prefix("# build:") {
                header.build = build.trim().parse().ok();
            }
        }
        header
    }

    /// Records one more contribution; the build only ever moves forward.
    pub fn add_source(&mut self, source: String, build: Option<u32>) {
        self.sources.push(source);
        self.build = match (self.build, build) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }

    /// The header as written at the top of the list.
    pub fn render(&self) -> String {
        let mut out = String::from("# Names for `rage`: one per line, hashed with JOAAT when loaded.\n");
        for s in &self.sources {
            out.push_str("# source: ");
            out.push_str(s);
            out.push('\n');
        }
        if let Some(b) = self.build {
            out.push_str(&format!("# build: {b}\n"));
        }
        out
    }

    /// One line saying what the list covers, for `names info` and the
    /// note under an inspection that left hashes unresolved.
    pub fn coverage(&self) -> String {
        match self.build {
            Some(b) => format!("covers game build {b}"),
            None if self.sources.is_empty() => "of unknown origin".to_owned(),
            None => "of unstated game build (a `names harvest` records the game's build; `names fetch --build N` states a downloaded list's)".to_owned(),
        }
    }
}

/// The header and the names of a list file.
pub fn read_list(path: &Path) -> Result<(ListHeader, BTreeSet<String>)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let header = ListHeader::parse(&text);
    let names = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect();
    Ok((header, names))
}

/// Writes a list with its header, creating the directory if needed.
pub fn write_list(path: &Path, header: &ListHeader, names: &BTreeSet<String>) -> Result<()> {
    let mut text = header.render();
    text.reserve(names.len() * 24);
    for n in names {
        text.push_str(n);
        text.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Today's date as `YYYY-MM-DD` (UTC), for the source lines.
pub fn today() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The header of the harvested list, if there is one on disk.
pub fn harvested_header() -> Option<ListHeader> {
    use std::io::Read;
    let path = harvest_path()?;
    let mut head = Vec::with_capacity(4096);
    std::fs::File::open(path).ok()?.take(4096).read_to_end(&mut head).ok()?;
    Some(ListHeader::parse(&String::from_utf8_lossy(&head)))
}

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
    fn header_round_trips_and_the_build_only_moves_forward() {
        let mut h = ListHeader::default();
        assert_eq!(h.coverage(), "of unknown origin");
        h.add_source("fetch https://example/ObjectList.ini on 2026-09-23".into(), None);
        assert!(h.coverage().starts_with("of unstated game build"));
        h.add_source("harvest GTA5.exe 1.0.3889.0 on 2026-09-23".into(), Some(3889));
        h.add_source("fetch old list".into(), Some(3717));
        assert_eq!(h.build, Some(3889));
        assert_eq!(h.coverage(), "covers game build 3889");
        let parsed = ListHeader::parse(&(h.render() + "prop_a\n# not a header line\n"));
        assert_eq!(parsed, h);
        assert_eq!(ListHeader::parse("prop_a\n"), ListHeader::default());
    }

    #[test]
    fn lists_are_written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("names.txt");
        let mut header = ListHeader::default();
        header.add_source("test".into(), Some(3889));
        let names: BTreeSet<String> = ["prop_b", "prop_a"].iter().map(|s| s.to_string()).collect();
        write_list(&path, &header, &names).unwrap();
        let (h, n) = read_list(&path).unwrap();
        assert_eq!(h, header);
        assert_eq!(n.into_iter().collect::<Vec<_>>(), ["prop_a", "prop_b"]);
    }

    #[test]
    fn days_convert_to_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_719), (2026, 9, 23));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(today().len(), 10);
    }

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
