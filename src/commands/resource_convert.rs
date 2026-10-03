//! `rage resource convert`: turn GTA V Enhanced (Gen9) resource files into the legacy PC layout FiveM
//! loads. Every file is read by `rage-formats`, which refuses what the legacy game cannot hold instead of
//! writing a file that crashes it; the files that do convert are written under `--output`, never over the
//! input.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use rage_formats::Edition;

/// Resource types whose layout depends on the edition (`rage_formats::Edition::of`).
const EDITION_TYPES: [&str; 5] = ["ytd", "ydr", "ydd", "yft", "ypt"];

#[derive(clap::Args)]
pub struct ConvertArgs {
    /// Resource files, or folders searched (recursively) for .ytd/.ydr/.ydd/.yft/.ypt files
    #[arg(required = true, value_name = "FILE|FOLDER")]
    pub paths: Vec<PathBuf>,

    /// Write the converted files into this folder (a folder given as input keeps its layout inside it)
    #[arg(short, long, value_name = "DIR")]
    pub output: PathBuf,

    /// The edition to convert to; only `legacy` (the PC game FiveM runs) exists
    #[arg(long, default_value = "legacy", value_name = "legacy")]
    pub to: String,

    /// Report what would be converted and write nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// What became of one input file.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Gen9 in, legacy out.
    Converted(Vec<u8>),
    /// Already a legacy file: nothing to do.
    AlreadyLegacy,
    /// Gen9, but the conversion for this type is not written yet.
    Unsupported(&'static str),
    /// Neither edition's version for this type.
    Unknown(u32),
}

/// Converts one file's bytes; `ext` is the file's extension without the dot, any case.
/// Errors name why the legacy game cannot hold the file.
pub fn convert_bytes(ext: &str, data: &[u8]) -> Result<Outcome> {
    let ext = ext.to_ascii_lowercase();
    match Edition::of_file(&ext, data)? {
        Some(Edition::Legacy) => Ok(Outcome::AlreadyLegacy),
        Some(Edition::Gen9) => match ext.as_str() {
            "ytd" => Ok(Outcome::Converted(rage_formats::convert_ytd_to_legacy(data)?)),
            "ydr" | "ydd" => Ok(Outcome::Unsupported("drawable conversion is not written yet (VIRUXE/rage-formats#13)")),
            "yft" => Ok(Outcome::Unsupported("fragment conversion is not written yet (VIRUXE/rage-formats#13)")),
            _ => Ok(Outcome::Unsupported("particle conversion is not written yet (VIRUXE/rage-formats#13)")),
        },
        None => {
            let sys = u32::from_le_bytes(data[8..12].try_into().unwrap());
            let gfx = u32::from_le_bytes(data[12..16].try_into().unwrap());
            Ok(Outcome::Unknown(rage_formats::resource_version_from_flags(sys, gfx)))
        }
    }
}

fn is_edition_type(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EDITION_TYPES.contains(&e.to_ascii_lowercase().as_str()))
}

/// The files to convert and where each goes, relative to the output folder: a file given directly keeps
/// its name, a file found under a folder keeps its path inside that folder.
fn gather(paths: &[PathBuf]) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut files: Vec<PathBuf> = crate::utils::walkdir(p)?.into_iter().filter(|f| is_edition_type(f)).collect();
            files.sort();
            for f in files {
                let rel = f.strip_prefix(p).unwrap_or(&f).to_path_buf();
                out.push((f, rel));
            }
        } else if p.is_file() {
            out.push((p.clone(), PathBuf::from(p.file_name().context("a file with no name")?)));
        } else {
            bail!("'{}' is neither a file nor a folder", p.display());
        }
    }
    Ok(out)
}

pub fn run(args: &ConvertArgs) -> Result<()> {
    if !args.to.eq_ignore_ascii_case("legacy") {
        bail!("--to {}: only `legacy` is supported (Enhanced -> the PC game FiveM runs)", args.to);
    }
    let files = gather(&args.paths)?;
    if files.is_empty() { bail!("no .ytd/.ydr/.ydd/.yft/.ypt files found"); }

    let (mut converted, mut skipped, mut failed) = (0usize, 0usize, 0usize);
    for (path, rel) in &files {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let result = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))
            .and_then(|data| convert_bytes(ext, &data));
        match result {
            Ok(Outcome::Converted(bytes)) => {
                let to = args.output.join(rel);
                if args.dry_run {
                    eprintln!("would convert {} ({} bytes)", path.display(), bytes.len());
                } else {
                    if let Some(parent) = to.parent() { std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?; }
                    std::fs::write(&to, &bytes).with_context(|| format!("writing {}", to.display()))?;
                    eprintln!("converted {} -> {} ({} bytes)", path.display(), to.display(), bytes.len());
                }
                converted += 1;
            }
            Ok(Outcome::AlreadyLegacy) => { eprintln!("skipped {}: already a legacy file", path.display()); skipped += 1; }
            Ok(Outcome::Unsupported(why)) => { eprintln!("skipped {}: {why}", path.display()); skipped += 1; }
            Ok(Outcome::Unknown(v)) => { eprintln!("skipped {}: resource version {v} is neither edition's", path.display()); skipped += 1; }
            Err(e) => { eprintln!("FAILED  {}: {e:#}", path.display()); failed += 1; }
        }
    }
    eprintln!("{} {converted}, skipped {skipped}, failed {failed}", if args.dry_run { "Would convert" } else { "Converted" });
    if failed > 0 { bail!("{failed} of {} file(s) could not be converted", files.len()); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(version_sys: u32, version_gfx: u32) -> Vec<u8> {
        let mut h = Vec::new();
        for v in [rage_formats::RSC7_MAGIC, 0, (version_sys << 28) | 0x0002_0000, (version_gfx << 28) | 0x0004_0090] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h
    }

    #[test]
    fn a_legacy_file_is_left_alone_and_an_unknown_version_is_reported() {
        // ytd version 13 = 0x0D: system nibble 0, graphics nibble 0xD
        assert_eq!(convert_bytes("ytd", &header(0, 13)).unwrap(), Outcome::AlreadyLegacy);
        assert_eq!(convert_bytes("YTD", &header(0, 13)).unwrap(), Outcome::AlreadyLegacy);
        assert_eq!(convert_bytes("ytd", &header(0, 9)).unwrap(), Outcome::Unknown(9));
    }

    #[test]
    fn gen9_types_without_a_conversion_are_skipped_with_a_reason() {
        // ydr 159 = 0x9F: system nibble 9, graphics nibble 0xF
        match convert_bytes("ydr", &header(9, 15)).unwrap() {
            Outcome::Unsupported(why) => assert!(why.contains("rage-formats#13"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
}
