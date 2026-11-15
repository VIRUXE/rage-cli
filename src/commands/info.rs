use anyhow::Result;
use std::path::Path;
use crate::rpf::{Archive, GtaKeys};

pub fn run(archive_path: &Path, keys: Option<&GtaKeys>) -> Result<()> {
    let archive = Archive::open(archive_path, keys)?;

    println!("Archive Information");
    println!("======================");
    println!("Path:        {}", archive_path.display());
    println!("Format:      {:?}", archive.version());
    println!("Entries:     {} ({} dirs, {} files)", archive.entry_count, archive.dir_count, archive.entry_count - archive.dir_count);
    println!("Encryption:  {}", match archive.encryption.as_u32() {
        0x4E45504F => "OPEN (no encryption)",
        0x00000000 => "NONE",
        0x0FFFFFF9 => "AES",
        0x0FEFFFFF => "NG",
        v          => return Err(anyhow::anyhow!("Unknown encryption type 0x{:08X}", v)),
    });

    let files = archive.list_files();
    let total_size: u64 = files.iter().map(|f| f.mem_size as u64).sum();
    println!("Files:       {}", files.len());
    println!("Total size:  {} bytes ({:.2} MB)", total_size, total_size as f64 / (1024.0 * 1024.0));

    Ok(())
}
