//! Private, opt-in validation against a user's own San Andreas install.
use rage_formats::renderware::txd::Txd;
use std::{fs, path::PathBuf};

#[test]
fn gtasa_all_txd_parse_decode_and_noop_rebuild() {
    let Some(root) = std::env::var_os("GTASA_PATH").map(PathBuf::from) else {
        eprintln!("skipped: set GTASA_PATH for private real-game fixtures");
        return;
    };
    let path = root.join("models/gta3.img");
    let file = fs::File::open(&path).unwrap();
    // Read-only fixture; the opt-in suite never modifies the game install.
    let data = unsafe { memmap2::Mmap::map(&file) }.unwrap();
    let archive = rpf_archive::RpfArchive::parse(&data, "gta3.img", None).unwrap();
    let (mut dictionaries, mut textures, mut mip_levels) = (0, 0, 0);
    let mut failures = Vec::new();
    let mut conversion_layout_failures = Vec::new();
    let mut empty_mip_records = Vec::new();
    for entry in archive
        .entries
        .iter()
        .filter(|entry| entry.name_lower.ends_with(".txd"))
    {
        dictionaries += 1;
        let result = (|| -> anyhow::Result<()> {
            let bytes = archive.extract_entry(&data, entry, None)?;
            let txd = Txd::parse(&bytes)?;
            anyhow::ensure!(txd.build()? == bytes, "no-op rebuilding changed bytes");
            for (index, texture) in txd.native_textures.iter().enumerate() {
                textures += 1;
                for level in 0..texture.mips.len() {
                    if texture.mips[level].is_empty() {
                        empty_mip_records
                            .push(format!("{}:{}:mip{level}", entry.name, texture.name));
                        continue;
                    }
                    let (w, h) = texture.mip_dimensions(level)?;
                    anyhow::ensure!(
                        txd.decode_mip(index, level)?.len() == w as usize * h as usize * 4,
                        "decoded mip size differs"
                    );
                    mip_levels += 1;
                }
            }
            if let Err(error) = txd.to_ytd() {
                conversion_layout_failures.push(format!("{}: {error:#}", entry.name));
            }
            Ok(())
        })();
        if let Err(error) = result {
            failures.push(format!("{}: {error:#}", entry.name));
        }
    }
    let report = json::object! {
        archive:path.to_string_lossy().as_ref(),dictionaries:dictionaries,textures:textures,mip_levels:mip_levels,
        failures:json::JsonValue::Array(failures.iter().cloned().map(Into::into).collect()),
        conversion_layout_failures:json::JsonValue::Array(conversion_layout_failures.iter().cloned().map(Into::into).collect())
    };
    let mut report = report;
    report["empty_mip_records"] =
        json::JsonValue::Array(empty_mip_records.iter().cloned().map(Into::into).collect());
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/gtasa");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("txd-validation.json"), report.pretty(2)).unwrap();
    println!(
        "{dictionaries} dictionaries, {textures} textures, {mip_levels} decoded mips, {} conversion layout failures",
        conversion_layout_failures.len()
    );
    assert!(dictionaries > 0, "no TXD fixtures found");
    assert!(
        failures.is_empty(),
        "{} TXD failures; first: {:?}; see target/gtasa/txd-validation.json",
        failures.len(),
        failures.first()
    );
}
