use anyhow::{bail, Context, Result};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rage_formats::{prepare_rsc7, rage_joaat};

use crate::rpf::{Archive, FileRef, GtaKeys};
use crate::utils::{json_string, matches_pattern};

#[derive(clap::Args)]
#[command(group = clap::ArgGroup::new("filter").required(true).multiple(true))]
pub struct SearchArgs {
    /// An .rpf archive, or a directory to scan for .rpf archives
    pub path: PathBuf,

    /// Name/path pattern (e.g. "*.ydr", "*icons.rpf/*", "prop_mk_arrow_3d.ydr")
    #[arg(group = "filter")]
    pub pattern: Option<String>,

    /// Text that must appear in the file contents
    #[arg(short, long, value_name = "TEXT", group = "filter")]
    pub content: Option<String>,

    /// Bytes that must appear in the file contents (e.g. "DE AD BE EF" or "deadbeef")
    #[arg(short = 'x', long, value_name = "BYTES", group = "filter")]
    pub hex: Option<String>,

    /// JOAAT hash of the file name or stem (0x1A2B3C4D or decimal)
    #[arg(long, value_name = "JOAAT", group = "filter")]
    pub hash: Option<String>,

    /// Case-insensitive --content matching
    #[arg(short, long)]
    pub ignore_case: bool,

    /// Emit a JSON array instead of plain lines
    #[arg(short, long)]
    pub json: bool,

    /// Show size, type, hash and match offset
    #[arg(short, long)]
    pub detailed: bool,

    /// Maximum nesting depth of archives to descend into
    #[arg(long, default_value = "16", value_name = "N")]
    pub max_depth: usize,

    /// Stop after this many hits
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
}

struct Filters {
    pattern: Option<String>,
    content: Option<Vec<u8>>,
    hex: Option<Vec<u8>>,
    hash: Option<u32>,
    ignore_case: bool,
}

impl Filters {
    fn needs_data(&self) -> bool {
        self.content.is_some() || self.hex.is_some()
    }
}

struct Hit {
    archive: PathBuf,
    path: String,
    size: u32,
    mem_size: u32,
    is_resource: bool,
    hash_name: u32,
    hash_stem: u32,
    offset: Option<usize>,
}

struct Sink {
    json: bool,
    detailed: bool,
    limit: Option<usize>,
    hits: usize,
    nested: usize,
    out: io::BufWriter<io::Stdout>,
}

impl Sink {
    fn full(&self) -> bool {
        self.limit.is_some_and(|n| self.hits >= n)
    }

    fn push(&mut self, hit: &Hit) -> Result<()> {
        if self.json {
            let sep = if self.hits == 0 { "\n  " } else { ",\n  " };
            write!(self.out, "{sep}{}", json_hit(hit))?;
        } else if self.detailed {
            if self.hits == 0 {
                writeln!(self.out, "{:<80} {:>12} {:>12} {:<8} {:<10} Offset", "Path", "Size", "MemSize", "Type", "Hash")?;
                writeln!(self.out, "{}", "-".repeat(134))?;
            }
            let kind = if hit.is_resource { "Resource" } else { "Binary" };
            let offset = hit.offset.map(|o| o.to_string()).unwrap_or_default();
            writeln!(self.out, "{:<80} {:>12} {:>12} {:<8} 0x{:08X} {}",
                format!("{}:{}", hit.archive.display(), hit.path), hit.size, hit.mem_size, kind, hit.hash_name, offset)?;
        } else {
            writeln!(self.out, "{}:{}", hit.archive.display(), hit.path)?;
        }
        self.hits += 1;
        Ok(())
    }
}

pub fn run(args: &SearchArgs, keys: Option<&GtaKeys>) -> Result<()> {
    let filters = Filters {
        pattern: args.pattern.as_ref().map(|p| p.replace('\\', "/").to_lowercase()),
        content: args.content.as_ref().map(|c| {
            if args.ignore_case { c.to_ascii_lowercase().into_bytes() } else { c.clone().into_bytes() }
        }),
        hex: args.hex.as_deref().map(parse_hex).transpose()?,
        hash: args.hash.as_deref().map(parse_hash).transpose()?,
        ignore_case: args.ignore_case,
    };

    let archives = collect_archives(&args.path)?;
    if archives.is_empty() {
        bail!("no .rpf archives found under {}", args.path.display());
    }

    let mut sink = Sink {
        json: args.json,
        detailed: args.detailed,
        limit: args.limit,
        hits: 0,
        nested: 0,
        out: io::BufWriter::new(io::stdout()),
    };

    if sink.json { write!(sink.out, "[")?; }

    let mut searched = 0usize;
    for archive_path in &archives {
        if sink.full() { break; }

        let archive = match Archive::open(archive_path, keys) {
            Ok(a) => a,
            Err(e) => { eprintln!("skipping {}: {}", archive_path.display(), e); continue; }
        };
        if let Err(e) = archive.require_keys(keys) {
            eprintln!("skipping {}: {}", archive_path.display(), e);
            continue;
        }
        searched += 1;

        let root = archive_path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
        search_recursive(&archive, archive_path, &root, &filters, keys, args.max_depth, 0, &mut sink)?;
    }

    if sink.json {
        writeln!(sink.out, "{}]", if sink.hits > 0 { "\n" } else { "" })?;
    } else if sink.hits == 0 {
        writeln!(sink.out, "No files found")?;
    }
    sink.out.flush()?;

    eprintln!("{} hit(s) in {} archive(s), {} nested", sink.hits, searched, sink.nested);
    Ok(())
}

/// Finds every `.rpf` under `path` (or just `path` itself, if it's a file).
/// Shared with `index::build`, which walks the same top-level tree to build
/// the texture-resolution index.
pub(crate) fn collect_archives(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() { return Ok(vec![path.to_path_buf()]); }
    if !path.is_dir() { bail!("{} does not exist", path.display()); }

    let mut found = Vec::new();
    walk_dir(path, &mut found)?;
    found.sort();
    Ok(found)
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            walk_dir(&path, out)?;
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("rpf")) {
            out.push(path);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn search_recursive(
    archive: &Archive,
    archive_path: &Path,
    prefix: &str,
    filters: &Filters,
    keys: Option<&GtaKeys>,
    max_depth: usize,
    depth: usize,
    sink: &mut Sink,
) -> Result<()> {
    if depth > max_depth { return Ok(()); }

    let files: Vec<FileRef> = archive.list_files().into_iter().cloned().collect();

    for file in &files {
        if sink.full() { return Ok(()); }

        let full = format!("{}/{}", prefix, file.path);
        let name_lower = file.name.to_lowercase();
        let is_rpf = name_lower.ends_with(".rpf");

        if let Some(hit) = match_file(archive, file, &full, &name_lower, filters, keys, is_rpf) {
            sink.push(&Hit { archive: archive_path.to_path_buf(), path: full.clone(), ..hit })?;
        }

        if is_rpf {
            sink.nested += 1;
            match archive.extract(file, keys).and_then(|d| Archive::from_bytes(d, &file.name, keys)) {
                Ok(nested) => search_recursive(&nested, archive_path, &full, filters, keys, max_depth, depth + 1, sink)?,
                Err(e) => eprintln!("failed to open nested {}: {}", full, e),
            }
        }
    }

    Ok(())
}

fn match_file(
    archive: &Archive,
    file: &FileRef,
    full: &str,
    name_lower: &str,
    filters: &Filters,
    keys: Option<&GtaKeys>,
    is_rpf: bool,
) -> Option<Hit> {
    if let Some(pat) = &filters.pattern
        && !matches_pattern(full, pat) && !matches_pattern(name_lower, pat)
    {
        return None;
    }

    let stem = name_lower.rsplit_once('.').map_or(name_lower, |(s, _)| s);
    let hash_name = rage_joaat(name_lower);
    let hash_stem = rage_joaat(stem);
    if let Some(h) = filters.hash
        && h != hash_name && h != hash_stem
    {
        return None;
    }

    let mut offset = None;
    if filters.needs_data() {
        // Nested archives are containers; their contents are searched as files.
        if is_rpf { return None; }
        let data = match archive.extract(file, keys) {
            Ok(d) => d,
            Err(e) => { eprintln!("failed to read {}: {}", full, e); return None; }
        };
        offset = Some(find_in_data(&data, file.is_resource, filters)?);
    }

    Some(Hit {
        archive: PathBuf::new(),
        path: String::new(),
        size: file.size,
        mem_size: file.mem_size,
        is_resource: file.is_resource,
        hash_name,
        hash_stem,
        offset,
    })
}

/// Returns the offset of the first content/hex match, searching the raw bytes and,
/// for resources, the inflated system and graphics sections as well.
fn find_in_data(data: &[u8], is_resource: bool, filters: &Filters) -> Option<usize> {
    let mut haystacks: Vec<Vec<u8>> = vec![data.to_vec()];
    if is_resource && let Ok((sys, gfx)) = prepare_rsc7(data) {
        haystacks.push(sys);
        haystacks.push(gfx);
    }

    for hay in &haystacks {
        let mut found = None;
        if let Some(text) = &filters.content {
            let pos = if filters.ignore_case {
                find_bytes(&hay.to_ascii_lowercase(), text)
            } else {
                find_bytes(hay, text)
            };
            match pos { Some(p) => found = Some(p), None => continue }
        }
        if let Some(bytes) = &filters.hex {
            match find_bytes(hay, bytes) {
                Some(p) => found = Some(found.map_or(p, |f: usize| f.min(p))),
                None => continue,
            }
        }
        if found.is_some() { return found; }
    }
    None
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() { return None; }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_hex(s: &str) -> Result<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !matches!(c, ' ' | ':' | ',' | '-')).collect();
    let cleaned = cleaned.strip_prefix("0x").or_else(|| cleaned.strip_prefix("0X")).unwrap_or(&cleaned);
    if cleaned.is_empty() || !cleaned.len().is_multiple_of(2) {
        bail!("--hex needs an even number of hex digits, got '{}'", s);
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).with_context(|| format!("invalid hex in '{}'", s)))
        .collect()
}

fn parse_hash(s: &str) -> Result<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).with_context(|| format!("invalid hash '{}'", s))
    } else {
        s.parse::<u32>().with_context(|| format!("invalid hash '{}' (use 0x... or decimal)", s))
    }
}

fn json_hit(hit: &Hit) -> String {
    let name = hit.path.rsplit('/').next().unwrap_or(&hit.path);
    format!(
        "{{\"archive\":{},\"path\":{},\"name\":{},\"size\":{},\"mem_size\":{},\"kind\":\"{}\",\"hash\":\"0x{:08X}\",\"stem_hash\":\"0x{:08X}\",\"offset\":{}}}",
        json_string(&hit.archive.to_string_lossy()),
        json_string(&hit.path),
        json_string(name),
        hit.size,
        hit.mem_size,
        if hit.is_resource { "resource" } else { "binary" },
        hit.hash_name,
        hit.hash_stem,
        hit.offset.map_or("null".to_string(), |o| o.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_accepts_common_spellings() {
        assert_eq!(parse_hex("DE AD BE EF").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(parse_hex("deadbeef").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(parse_hex("de:ad").unwrap(), vec![0xDE, 0xAD]);
        assert_eq!(parse_hex("0xdead").unwrap(), vec![0xDE, 0xAD]);
        assert!(parse_hex("abc").is_err());
        assert!(parse_hex("zz").is_err());
        assert!(parse_hex("").is_err());
    }

    #[test]
    fn parse_hash_takes_hex_or_decimal() {
        assert_eq!(parse_hash("0x1A2B3C4D").unwrap(), 0x1A2B3C4D);
        assert_eq!(parse_hash("439041101").unwrap(), 0x1A2B3C4D);
        assert!(parse_hash("nope").is_err());
    }

    #[test]
    fn find_bytes_returns_first_offset() {
        assert_eq!(find_bytes(b"xxneedlexxneedle", b"needle"), Some(2));
        assert_eq!(find_bytes(b"short", b"longer needle"), None);
        assert_eq!(find_bytes(b"abc", b""), None);
    }

    #[test]
    fn json_string_escapes() {
        assert_eq!(json_string("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
    }
}
