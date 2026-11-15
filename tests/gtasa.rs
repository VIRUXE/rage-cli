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

#[test]
fn gtasa_dff_parse_and_noop_rebuild() {
    use rage_formats::renderware::dff::Dff;
    let Some(root) = std::env::var_os("GTASA_PATH").map(PathBuf::from) else {
        eprintln!("skipped: set GTASA_PATH for private DFF fixtures");
        return;
    };
    let (mut files, mut geometries, mut skinned) = (0, 0, 0);
    let mut failures = Vec::new();
    let mut json_samples = Vec::new();
    for archive_name in ["gta3.img", "gta_int.img", "player.img", "cutscene.img"] {
        let mut sampled_plain = false;
        let mut sampled_skin = false;
        let path = root.join("models").join(archive_name);
        let file = fs::File::open(&path).unwrap();
        let data = unsafe { memmap2::Mmap::map(&file) }.unwrap();
        let archive = rpf_archive::RpfArchive::parse(&data, archive_name, None).unwrap();
        for entry in archive
            .entries
            .iter()
            .filter(|e| e.name_lower.ends_with(".dff"))
        {
            files += 1;
            let result = (|| -> anyhow::Result<()> {
                let bytes = archive.extract_entry(&data, entry, None)?;
                let dff = Dff::parse(&bytes)?;
                let rebuilt = dff.build()?;
                anyhow::ensure!(
                    rebuilt == bytes,
                    "no-op bytes differ at {:?}",
                    rebuilt.iter().zip(&bytes).position(|(a, b)| a != b)
                );
                for geometry in &dff.geometries {
                    for material in &geometry.materials {
                        if let Some(effect) = material.effects()? {
                            let source = &material
                                .plugins
                                .iter()
                                .find(|p| p.kind == 0x120)
                                .unwrap()
                                .payload;
                            anyhow::ensure!(effect.build()? == *source, "MatFX view changed bytes");
                        }
                        if let Some(reflection) = material.reflection()? {
                            let source = &material
                                .plugins
                                .iter()
                                .find(|p| p.kind == 0x253f2fc)
                                .unwrap()
                                .payload;
                            anyhow::ensure!(
                                reflection.build()? == *source,
                                "reflection view changed bytes"
                            );
                        }
                        if let Some(specular) = material.specular()? {
                            let source = &material
                                .plugins
                                .iter()
                                .find(|p| p.kind == 0x253f2f6)
                                .unwrap()
                                .payload;
                            anyhow::ensure!(
                                specular.build()? == *source,
                                "specular view changed bytes"
                            );
                        }
                    }
                    if let Some(effects) = geometry.effects2d()? {
                        let source = &geometry
                            .plugins
                            .iter()
                            .find(|p| p.kind == 0x253f2f8)
                            .unwrap()
                            .payload;
                        anyhow::ensure!(effects.build()? == *source, "2DFX view changed bytes");
                    }
                }
                let has_skin = dff.geometries.iter().any(|g| g.skin.is_some());
                if !sampled_plain || has_skin && !sampled_skin {
                    let document = rage_formats::renderware::Document::parse(
                        &bytes,
                        rage_formats::renderware::Format::Dff,
                    )?;
                    let json = document.to_json()?;
                    let rebuilt = rage_formats::renderware::Document::from_json(&json)?.build()?;
                    anyhow::ensure!(
                        rebuilt == bytes,
                        "editable scene JSON roundtrip changed bytes"
                    );
                    use sha2::{Digest, Sha256};
                    json_samples.push(json::object!{archive:archive_name,name:entry.name.clone(),sha256:format!("{:x}",Sha256::digest(&bytes)),skinned:has_skin});
                    sampled_plain = true;
                    if has_skin {
                        sampled_skin = true;
                    }
                }
                geometries += dff.geometries.len();
                skinned += dff.geometries.iter().filter(|g| g.skin.is_some()).count();
                Ok(())
            })();
            if let Err(error) = result {
                failures.push(format!("{archive_name}/{}: {error:#}", entry.name));
            }
        }
    }
    let report = json::object! {files:files,geometries:geometries,skinned_geometries:skinned,json_roundtrip_samples:json::JsonValue::Array(json_samples),failures:json::JsonValue::Array(failures.iter().cloned().map(Into::into).collect())};
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/gtasa");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("dff-validation.json"), report.pretty(2)).unwrap();
    println!(
        "{files} DFFs, {geometries} geometries, {skinned} skinned geometries, {} failures",
        failures.len()
    );
    assert!(files > 0);
    assert!(
        failures.is_empty(),
        "{} failures, first {:?}; see target/gtasa/dff-validation.json",
        failures.len(),
        failures.first()
    );
}
