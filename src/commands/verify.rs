use anyhow::Result;
use std::path::Path;
use crate::rpf::{Archive, GtaKeys};

pub fn run(archive_path: &Path, keys: Option<&GtaKeys>) -> Result<()> {
    println!("Verifying: {}", archive_path.display());

    let archive = match Archive::open(archive_path, keys) {
        Ok(a)  => { println!("✓ Archive opened and header parsed"); a }
        Err(e) => { return Err(e.context("failed to open archive")); }
    };
    archive.require_keys(keys)?;
    archive.verify_ranges()?;

    let files = archive.list_files();
    println!("  {} entries ({} files, {} dirs)", archive.entry_count, files.len(), archive.dir_count);

    let mut errors = 0usize;
    for (i, f) in files.iter().enumerate() {
        if f.size == 0 && f.mem_size == 0 {
            eprintln!("  ✗ {} has zero size", f.path);
            errors += 1;
        }
        if i % 1000 == 999 { print!("\r  Checked {}/{}...", i + 1, files.len()); }
    }

    if errors == 0 {
        println!("✓ All {} file entries valid", files.len());
    } else {
        println!("✗ {} error(s) found", errors);
        anyhow::bail!("{errors} archive error(s)");
    }

    Ok(())
}
