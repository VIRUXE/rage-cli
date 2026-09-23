//! `rage names`: the hash → name list that `resource info` and `resource
//! dump` print through. `harvest` builds it from the game's own files —
//! every archive entry's stem (archetype, map, dictionary and collision
//! names) and every element, attribute and value of the plain-text XML
//! metadata the game ships — so no outside list is needed; `fetch` pulls a
//! public list into the same place for machines with no game install;
//! `lookup` answers what a hash or a name is; `info` says what is on disk
//! and which game build it covers.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rage_formats::{rage_joaat, NameTable};

use crate::index::ranked_archives;
use crate::names::{read_list, today, write_list, ListHeader};
use crate::rpf::{Archive, GtaKeys};

/// A public, one-name-per-line dump of the game's archetype names,
/// maintained by the community; what `fetch` reads without `--url`.
pub const DEFAULT_LIST_URL: &str = "https://raw.githubusercontent.com/DurtyFree/gta-v-data-dumps/master/ObjectList.ini";

#[derive(clap::Args)]
pub struct NamesArgs {
    #[command(subcommand)]
    pub command: NamesCommand,
}

#[derive(clap::Subcommand)]
pub enum NamesCommand {
    /// Scan the game's archives for names and write the list (--exe required)
    Harvest(HarvestArgs),
    /// Download a public name list into the same place, no game install needed
    Fetch(FetchArgs),
    /// Where the list is, how many names it holds and which game build it covers
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

#[derive(clap::Args)]
pub struct FetchArgs {
    /// Where to download the list from (one name per line, `#` comments allowed)
    #[arg(long, value_name = "URL", default_value = DEFAULT_LIST_URL)]
    pub url: String,

    /// The game build the list is current to, recorded so `names info` and
    /// `resource info` can say what the list covers
    #[arg(long, value_name = "N")]
    pub build: Option<u32>,

    /// Write the list here instead of ~/.rage-cli/names.txt (or RAGE_NAMES)
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Replace the list on disk instead of adding to it
    #[arg(long)]
    pub replace: bool,
}

pub fn run(args: &NamesArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    match &args.command {
        NamesCommand::Harvest(h) => harvest(h, keys, exe),
        NamesCommand::Fetch(f) => fetch(f),
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
    println!("Name list:      {}", path.display());
    if path.is_file() {
        let (header, names) = read_list(&path)?;
        println!("Names:          {}", names.len());
        println!("Coverage:       {}", header.coverage());
        if !header.sources.is_empty() {
            println!("Sources:");
            for s in &header.sources {
                println!("  {s}");
            }
        }
    } else {
        println!("None yet — `rage names harvest --exe PATH` scans the game, `rage names fetch` downloads a public list.");
    }
    Ok(())
}

/// Where a new list goes, and what is already there to merge with.
fn list_target(output: Option<&PathBuf>, replace: bool) -> Result<(PathBuf, ListHeader, BTreeSet<String>)> {
    let path = match output {
        Some(p) => p.clone(),
        None => crate::names::harvest_path().context("no names directory available (no HOME/USERPROFILE?)")?,
    };
    if !replace && path.is_file() {
        let (header, names) = read_list(&path)?;
        Ok((path, header, names))
    } else {
        Ok((path, ListHeader::default(), BTreeSet::new()))
    }
}

fn fetch(args: &FetchArgs) -> Result<()> {
    let (path, mut header, mut names) = list_target(args.output.as_ref(), args.replace)?;
    let before = names.len();

    println!("Fetching {}...", args.url);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .user_agent(concat!("rage-cli/", env!("CARGO_PKG_VERSION"), " (+https://github.com/VIRUXE/rage-cli)"))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(300)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut res = agent.get(&args.url).call().with_context(|| format!("downloading {}", args.url))?;
    if !res.status().is_success() {
        bail!("downloading {} failed: HTTP {}", args.url, res.status());
    }
    let body = res.body_mut().with_config().limit(256 << 20).read_to_string().context("reading the list")?;
    let fetched = add_fetched(&mut names, &body);
    if fetched == 0 {
        bail!("{} holds no names (expected one per line)", args.url);
    }

    let build = match args.build {
        Some(b) => format!(" (build {b})"),
        None => String::new(),
    };
    header.add_source(format!("fetch {} on {}{build}", args.url, today()), args.build);
    write_list(&path, &header, &names)?;
    println!("Wrote {} names to {} ({} new; the list {})", names.len(), path.display(), names.len() - before, header.coverage());
    Ok(())
}

/// Adds the names of a downloaded list: one per line, trimmed, `#` and `;`
/// comments and `[section]` lines skipped. Returns how many lines held a name.
fn add_fetched(names: &mut BTreeSet<String>, body: &str) -> usize {
    let mut count = 0;
    for line in body.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with(['#', ';', '[']) {
            continue;
        }
        count += 1;
        if !names.contains(line) {
            names.insert(line.to_owned());
        }
    }
    count
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
    // A harvest supersedes whatever was fetched: the game's own files are
    // the source of truth, and a fetched list may name things this build
    // has not got. The previous sources stay in the header for the record.
    let (output, mut header, _) = list_target(args.output.as_ref(), true)?;
    if output.is_file() {
        header = read_list(&output)?.0;
    }
    let version = crate::keys::exe_version(&exe_path);
    let build = version.as_deref().and_then(crate::keys::build_number);

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

    header.add_source(
        format!("harvest {} {} on {}", exe_path.display(), version.as_deref().unwrap_or("(version unknown)"), today()),
        build,
    );
    write_list(&output, &header, &names)?;
    println!("Wrote {} names to {} (the list {})", names.len(), output.display(), header.coverage());
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
    fn fetched_lists_skip_comments_and_ini_sections() {
        let mut names = BTreeSet::new();
        let n = add_fetched(&mut names, "\u{feff}# a comment\n[Objects]\nprop_a\n prop_b \n; another\n\nprop_a\n");
        assert_eq!(n, 3);
        assert_eq!(names.into_iter().collect::<Vec<_>>(), ["prop_a", "prop_b"]);
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
