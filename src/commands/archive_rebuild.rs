use anyhow::{Context, Result};
use std::path::PathBuf;
use crate::rpf::{Archive, GtaKeys};

#[derive(clap::Args)]
pub struct ArchiveArgs {
    #[command(subcommand)]
    command: ArchiveCommand,
}
#[derive(clap::Subcommand)]
enum ArchiveCommand {
    /// Write a new IMG archive, preserving entries except explicitly requested changes
    Rebuild {
        input: PathBuf,
        #[arg(short, long)] output: PathBuf,
        /// Add a file using its basename (must not already exist)
        #[arg(long)] add: Vec<PathBuf>,
        /// Replace an existing entry with a file of the same basename
        #[arg(long)] replace: Vec<PathBuf>,
        /// Remove an exact entry name/path (case insensitive)
        #[arg(long)] remove: Vec<String>,
    },
}

pub fn run(args: &ArchiveArgs, keys: Option<&GtaKeys>) -> Result<()> {
    let ArchiveCommand::Rebuild { input, output, add, replace, remove } = &args.command;
    anyhow::ensure!(!output.exists(), "output must be a new archive: {}", output.display());
    let archive = Archive::open(input, keys)?;
    anyhow::ensure!(matches!(archive.version(), crate::rpf::RpfVersion::Img1 | crate::rpf::RpfVersion::Img2 | crate::rpf::RpfVersion::Img3),
        "archive rebuild currently supports IMG1/2/3 only; preserving RPF resource metadata is not implemented");
    archive.require_keys(keys)?;
    archive.verify_ranges()?;
    let mut files = archive.list_files().into_iter()
        .map(|entry| archive.rebuild_entry_bytes(entry, keys).map(|bytes| (entry.path.clone(), bytes)))
        .collect::<Result<Vec<_>>>()?;
    let mut changed = std::collections::HashSet::new();
    for name in remove {
        anyhow::ensure!(changed.insert(name.to_lowercase()), "duplicate operation for '{name}'");
        let index = files.iter().position(|(path, _)| path.eq_ignore_ascii_case(name)).with_context(|| format!("cannot remove missing entry '{name}'"))?;
        files.remove(index);
    }
    for (paths, replacing) in [(replace, true), (add, false)] {
        for path in paths {
            let name = path.file_name().and_then(|n| n.to_str()).context("input has no UTF-8 filename")?.to_string();
            anyhow::ensure!(changed.insert(name.to_lowercase()), "duplicate operation for '{name}'");
            let index = files.iter().position(|(p, _)| p.eq_ignore_ascii_case(&name));
            anyhow::ensure!(index.is_some() == replacing, "entry '{name}' {}", if replacing { "does not exist for replacement" } else { "already exists; use --replace" });
            let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            if replacing && archive.version() == crate::rpf::RpfVersion::Img3 {
                let entry = archive.find_file(&name).context("replacement entry missing")?;
                if let rpf_archive::RpfEntryKind::BinaryFile { file_size, .. } = archive.entry_kind(entry) {
                    anyhow::ensure!(*file_size == 0, "'{name}' is an IMG3 native resource: raw copying is supported, replacement requires a resource-aware writer");
                }
            }
            if let Some(index) = index { files[index].1 = bytes; } else { files.push((name, bytes)); }
        }
    }
    super::create::write_archive_from(files, output, archive.version(), archive.encryption, keys, Some(&archive))?;
    println!("Rebuilt {} -> {}", input.display(), output.display());
    Ok(())
}
