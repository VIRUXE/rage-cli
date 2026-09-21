//! `rage names`: the hash → name list that `resource info` and `resource
//! dump` print through. `harvest` builds it from the game's own files —
//! every archive entry's stem (archetype, map, dictionary and collision
//! names) and every element, attribute and value of the plain-text XML
//! metadata the game ships — so no outside list is needed; `lookup` answers
//! what a hash or a name is; `info` says what is on disk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rage_formats::{rage_joaat, NameTable};

use crate::index::ranked_archives;
use crate::rpf::{Archive, GtaKeys};

#[derive(clap::Args)]
pub struct NamesArgs {
    #[command(subcommand)]
    pub command: NamesCommand,
}

#[derive(clap::Subcommand)]
pub enum NamesCommand {
    /// Scan the game's archives for names and write the list (--exe required)
    Harvest(HarvestArgs),
    /// Where the list is and how many names it holds
    Info,
    /// Print the hash of each name given, or the name of each 0x hash
    Lookup {
        /// Names, or hashes as 0x hex or decimal
        #[arg(required = true, num_args = 1..)]
        terms: Vec<String>,
        /// Extra name lists to look hashes up in; repeatable
        #[arg(long, value_name = "FILE")]
        names: Vec<PathBuf>,
    },
}

#[derive(clap::Args)]
pub struct HarvestArgs {
    /// Write the list here instead of ~/.rage-cli/names.txt (or RAGE_NAMES)
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

pub fn run(args: &NamesArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    match &args.command {
        NamesCommand::Harvest(h) => harvest(h, keys, exe),
        NamesCommand::Info => info(),
        NamesCommand::Lookup { terms, names } => lookup(terms, names),
    }
}

fn info() -> Result<()> {
    let Some(path) = crate::names::harvest_path() else {
        println!("No names directory available (no HOME/USERPROFILE?)");
        return Ok(());
    };
    println!("Built-in names: {}", NameTable::core().len());
    println!("Harvested list: {}", path.display());
    if path.is_file() {
        let text = std::fs::read_to_string(&path)?;
        let mut table = NameTable::empty();
        let count = table.add_list(&text);
        println!("Harvested names: {count}");
    } else {
        println!("Not harvested yet — run `rage names harvest --exe PATH`.");
    }
    Ok(())
}

fn lookup(terms: &[String], extra: &[PathBuf]) -> Result<()> {
    let table = crate::names::load(extra, None)?;
    for term in terms {
        let hash = term
            .strip_prefix("0x")
            .or_else(|| term.strip_prefix("0X"))
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .or_else(|| term.parse::<u32>().ok());
        match hash {
            Some(h) => println!("0x{h:08X}  {}", table.get(h).unwrap_or("(unknown)")),
            None => println!("0x{:08X}  {term}", rage_joaat(term)),
        }
    }
    Ok(())
}

fn harvest(args: &HarvestArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    let exe = exe.context("--exe or GTAV_PATH is required for `rage names harvest`")?;
    let exe_path = crate::keys::resolve_exe(exe)?;
    let game_root = exe_path.parent().context("--exe has no parent directory")?.to_path_buf();
    let output = match &args.output {
        Some(p) => p.clone(),
        None => crate::names::harvest_path().context("no names directory available (no HOME/USERPROFILE?)")?,
    };

    println!("Harvesting names from {}...", game_root.display());
    let mut names = BTreeSet::new();
    let archives = ranked_archives(&game_root, keys)?;
    for archive_path in &archives {
        let archive = match Archive::open(archive_path, keys) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("names: skipping {}: {e}", archive_path.display());
                continue;
            }
        };
        if archive.require_keys(keys).is_err() {
            eprintln!("names: skipping {} (needs keys)", archive_path.display());
            continue;
        }
        harvest_archive(&archive, keys, &mut names);
    }
    if names.is_empty() {
        bail!("no names found under {}", game_root.display());
    }

    let mut text = String::with_capacity(names.len() * 24);
    text.push_str("# Names harvested from the game's own files by `rage names harvest`.\n");
    for n in &names {
        text.push_str(n);
        text.push('\n');
    }
    if let Some(dir) = output.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&output, text).with_context(|| format!("writing {}", output.display()))?;
    println!("Wrote {} names to {}", names.len(), output.display());
    Ok(())
}

/// Every entry's stem, and the names inside every plain-text XML entry,
/// descending into nested archives.
fn harvest_archive(archive: &Archive, keys: Option<&GtaKeys>, out: &mut BTreeSet<String>) {
    for file in archive.list_files() {
        let name_lower = file.name.to_lowercase();
        if name_lower.ends_with(".rpf") {
            let Ok(data) = archive.extract(file, keys) else { continue };
            let Ok(nested) = Archive::from_bytes(data, &file.name, keys) else { continue };
            harvest_archive(&nested, keys, out);
            continue;
        }
        let stem = crate::resources::file_stem(&file.name);
        add_identifier(out, &stem);
        add_identifier(out, &stem.to_lowercase());
        if matches!(crate::resources::extension_of(&name_lower), "meta" | "xml" | "ymt" | "ymf" | "dat")
            && let Ok(data) = archive.extract(file, keys)
            && data.first() == Some(&b'<')
            && let Ok(text) = std::str::from_utf8(&data)
        {
            harvest_xml(text, out);
        }
    }
}

/// Element names, attribute names, and short identifier-like attribute
/// values and text (type names, hash names, model names).
pub fn harvest_xml(text: &str, out: &mut BTreeSet<String>) {
    let Ok(doc) = roxmltree::Document::parse(text) else { return };
    for node in doc.descendants() {
        if !node.is_element() {
            continue;
        }
        add_identifier(out, node.tag_name().name());
        for attr in node.attributes() {
            add_identifier(out, attr.name());
            add_identifier(out, attr.value());
        }
        if node.children().all(|c| !c.is_element())
            && let Some(t) = node.text()
        {
            for token in t.split([' ', ',', '\n', '\r', '\t']) {
                add_identifier(out, token);
            }
        }
    }
}

/// Keeps a token when it could be a name: something a hash was made of.
/// Numbers, GUIDs, hex blobs and punctuation are not.
fn add_identifier(out: &mut BTreeSet<String>, s: &str) {
    let s = s.trim();
    if s.len() < 2 || s.len() > 64 || s.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.' || c == '+') {
        return;
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.' | '@')) {
        return;
    }
    if !s.chars().any(|c| c.is_ascii_alphabetic()) || s.parse::<f64>().is_ok() || matches!(s, "true" | "false") {
        return;
    }
    // A GUID (8-4-4-4-12 hex) or a bare hex blob names nothing.
    let hexish = s.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if hexish && (s.len() == 36 || s.len() >= 16) {
        return;
    }
    if !out.contains(s) {
        out.insert(s.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_yields_tags_attributes_and_identifier_text() {
        let mut out = BTreeSet::new();
        harvest_xml(
            "<CPackFileMetaData><imapDependencies_2><Item><imapName>bombapaleto</imapName><itypDepArray><Item>v_construction</Item></itypDepArray><lodDist value=\"200\"/><flags value=\"true\"/><pos x=\"1.5\" y=\"-2\"/></Item></imapDependencies_2></CPackFileMetaData>",
            &mut out,
        );
        for expected in ["CPackFileMetaData", "imapDependencies_2", "Item", "imapName", "bombapaleto", "v_construction", "lodDist", "value", "flags", "pos"] {
            assert!(out.contains(expected), "missing {expected}: {out:?}");
        }
        assert!(!out.contains("200"), "numbers are not names");
        assert!(!out.contains("true"));
        assert!(!out.contains("-2"));
        assert!(!out.contains("x"), "one-letter attribute names are not worth a line");
    }

    #[test]
    fn identifiers_are_filtered() {
        let mut out = BTreeSet::new();
        for s in ["prop_a", "Hello World", "", "9lives", "ok-name", "1.5", "a".repeat(70).as_str(), "x64a", "+", "+ve", ":", "A0011940-7438-4232-BBF6-5119F9A8C9D1", "DEADBEEFDEADBEEF", "ab"] {
            add_identifier(&mut out, s);
        }
        assert_eq!(out.into_iter().collect::<Vec<_>>(), ["ab", "ok-name", "prop_a", "x64a"]);
    }
}
