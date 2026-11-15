use anyhow::{bail, Result};
use rpf_archive::{RpfBuilder, RpfEncryption, RpfVersion};
use std::fs;
use std::path::Path;

use rpf_archive::GtaKeys;

pub fn run(
    input_dir: &Path,
    output: &Path,
    version: u8,
    encryption: &str,
    format: &str,
    keys: Option<&GtaKeys>,
) -> Result<()> {
    if !input_dir.is_dir() {
        bail!("{} is not a directory", input_dir.display());
    }

    let rpf_version = match format {
        "img1" => RpfVersion::Img1,
        "img2" => RpfVersion::Img2,
        "img3" => RpfVersion::Img3,
        _ => match version {
        0 => RpfVersion::V0,
        2 => RpfVersion::V2,
        3 => RpfVersion::V3,
        4 => RpfVersion::V4,
        6 => RpfVersion::V6,
        7 => RpfVersion::V7,
        _ => bail!("unsupported version {}; valid: 0 2 3 4 6 7", version),
        },
    };

    let rpf_encryption = match encryption {
        "none" => RpfEncryption::None,
        "open" => RpfEncryption::Open,
        "ng"   => RpfEncryption::Ng,
        other  => bail!("unknown encryption '{}'; valid: none open ng", other),
    };

    let mut files = Vec::new();
    collect_dir(&mut files, input_dir, input_dir)?;
    write_archive(files, output, rpf_version, rpf_encryption, keys)?;
    println!("Created {}", output.display());
    Ok(())
}

fn collect_dir(files: &mut Vec<(String, Vec<u8>)>, base: &Path, dir: &Path) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        anyhow::ensure!(!entry.file_type()?.is_symlink(), "archive input contains symlink: {}", entry.path().display());
        let path = entry.path();
        if path.is_dir() {
            collect_dir(files, base, &path)?;
        } else {
            let rel = path.strip_prefix(base)?;
            let archive_path = rel.to_string_lossy().replace('\\', "/");
            let data = fs::read(&path)?;
            files.push((archive_path, data));
        }
    }
    Ok(())
}

pub fn write_archive(files: Vec<(String, Vec<u8>)>, output: &Path, version: RpfVersion, encryption: RpfEncryption, keys: Option<&GtaKeys>) -> Result<()> {
    write_archive_from(files, output, version, encryption, keys, None)
}

pub fn write_archive_from(files: Vec<(String, Vec<u8>)>, output: &Path, version: RpfVersion, encryption: RpfEncryption, keys: Option<&GtaKeys>, source: Option<&crate::rpf::Archive>) -> Result<()> {
    let img = matches!(version, RpfVersion::Img1 | RpfVersion::Img2 | RpfVersion::Img3);
    anyhow::ensure!(!img || encryption == RpfEncryption::None, "IMG archives do not support RPF encryption options");
    let mut names = std::collections::HashSet::new();
    let mut builder = RpfBuilder::for_version(version, encryption);
    for (path, data) in files {
        let name = if img { path.rsplit('/').next().unwrap_or("") } else { &path };
        anyhow::ensure!(!name.is_empty() && !name.contains('\0'), "empty or invalid archive name");
        anyhow::ensure!(names.insert(name.to_lowercase()), "duplicate archive entry '{name}'");
        if img {
            anyhow::ensure!(!name.contains(['/', '\\', ':']) && name != "." && name != "..", "invalid IMG entry name '{name}'");
            anyhow::ensure!(name.is_ascii(), "IMG entry names must be ASCII: '{name}'");
            if version != RpfVersion::Img3 {
                anyhow::ensure!(name.len() <= 24, "IMG entry name exceeds 24 bytes: '{name}'");
            }
            anyhow::ensure!(data.len().div_ceil(2048) <= u16::MAX as usize || version != RpfVersion::Img2,
                "IMG2 entry exceeds the sector-count limit: '{name}'");
        }
        builder.add_file(name, data);
    }
    let (mut bytes, mut dir) = if version == RpfVersion::Img1 {
        let (dir, img) = builder.build_img1_pair()?; (img, Some(dir))
    } else { (builder.build(keys)?, None) };
    if let Some(source) = source { source.preserve_directory_metadata(&mut bytes, dir.as_deref_mut()); }
    if let Some(ref dir_bytes) = dir {
        anyhow::ensure!(output != output.with_extension("dir"), "IMG1 output must use a different path from its paired DIR");
        let parsed = rpf_archive::RpfArchive::parse_img1(dir_bytes, "output.img")?;
        for entry in &parsed.entries { parsed.extract_entry(&bytes, entry, keys)?; }
        anyhow::ensure!(!output.with_extension("dir").exists(), "output DIR already exists: {}", output.with_extension("dir").display());
    } else {
        let parsed = rpf_archive::RpfArchive::parse(&bytes, "output", keys)?;
        for entry in parsed.entries.iter().filter(|e| !e.is_directory()) { parsed.extract_entry(&bytes, entry, keys)?; }
    }
    anyhow::ensure!(!img || !output.exists(), "output archive already exists: {}", output.display());
    if let Some(dir) = dir {
        // Create-new prevents either half of an existing pair being overwritten.
        use std::io::Write;
        let dir_path = output.with_extension("dir");
        let mut img_file = fs::OpenOptions::new().write(true).create_new(true).open(output)?;
        let mut created_dir = false;
        let result = (|| -> Result<()> {
            let mut dir_file = fs::OpenOptions::new().write(true).create_new(true).open(&dir_path)?;
            created_dir = true;
            img_file.write_all(&bytes)?;
            if let Err(error) = dir_file.write_all(&dir) {
                drop(dir_file); let _ = fs::remove_file(&dir_path); return Err(error.into());
            }
            Ok(())
        })();
        if result.is_err() {
            drop(img_file); let _ = fs::remove_file(output);
            if created_dir { let _ = fs::remove_file(&dir_path); }
        }
        result?;
    } else { fs::write(output, bytes)?; }
    Ok(())
}
