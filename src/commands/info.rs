use anyhow::Result;
use std::path::Path;
use crate::rpf::{Archive, GtaKeys};

pub fn run(archive_path: &Path, keys: Option<&GtaKeys>) -> Result<()> {
    let archive = Archive::open(archive_path, keys)?;

    println!("RPF Archive Information");
    println!("======================");
    println!("Path:        {}", archive_path.display());
    println!("Entries:     {} ({} dirs, {} files)", archive.entry_count, archive.dir_count, archive.entry_count - archive.dir_count);
    println!("Encryption:  {}", match archive.encryption {
        crate::rpf::RpfEncryption::Open => "OPEN (no encryption)",
        crate::rpf::RpfEncryption::None => "NONE",
        crate::rpf::RpfEncryption::Aes => "AES",
        crate::rpf::RpfEncryption::Ng => "NG",
        crate::rpf::RpfEncryption::Tfit => "TFIT2 (RDR2)",
    });

    let files = archive.list_files();
    let total_size: u64 = files.iter().map(|f| f.mem_size as u64).sum();
    println!("Files:       {}", files.len());
    println!("Total size:  {} bytes ({:.2} MB)", total_size, total_size as f64 / (1024.0 * 1024.0));

    Ok(())
}
