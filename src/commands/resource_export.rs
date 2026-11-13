//! Batch metadata export; continue after per-file errors and return failure
//! if any file failed, retaining every successful export for inspection.
use crate::rpf::{Archive, GtaKeys};
use anyhow::{Context, Result, bail};
use rage_formats::{MetaDump, NameTable, dump_metadata, to_json, to_xml};
use std::path::{Path, PathBuf};

#[derive(clap::Args)]
pub struct ExportArgs {
    /// .ymt files, folders searched recursively, or .rpf archives (.ymt entries)
    #[arg(required = true, value_name = "FILE|FOLDER|RPF")]
    pub paths: Vec<PathBuf>,
    #[arg(short, long, value_name = "DIR")]
    pub output: PathBuf,
    /// Write JSON instead of XML
    #[arg(long)]
    pub json: bool,
    /// Parse every input and report results without writing files
    #[arg(long)]
    pub dry_run: bool,
    /// Extra name lists for resolving hashes
    #[arg(long, value_name = "FILE")]
    pub names: Vec<PathBuf>,
}

pub fn dump(data: &[u8]) -> Result<MetaDump> {
    if crate::rdr2::metadata::is_rsc8(data) {
        crate::rdr2::metadata::dump(data)
    } else {
        Ok(dump_metadata(data)?.1)
    }
}

struct Exporter<'a> {
    args: &'a ExportArgs,
    names: NameTable,
    converted: usize,
    failed: usize,
}
impl Exporter<'_> {
    fn write(&mut self, label: &str, rel: &Path, bytes: Result<Vec<u8>>) {
        let result = bytes.and_then(|bytes| dump(&bytes)).and_then(|dump| {
            for warning in &dump.warnings {
                eprintln!("warning: {label}: {warning}");
            }
            let text = if self.args.json {
                to_json(&dump.root, &self.names).pretty(2)
            } else {
                to_xml(&dump.root, &self.names)
            };
            let output = self
                .args
                .output
                .join(rel)
                .with_extension(if self.args.json {
                    "ymt.json"
                } else {
                    "ymt.xml"
                });
            if !self.args.dry_run {
                if let Some(parent) = output.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&output, text)
                    .with_context(|| format!("writing {}", output.display()))?;
            }
            eprintln!(
                "{} {label} -> {}",
                if self.args.dry_run {
                    "would export"
                } else {
                    "exported"
                },
                output.display()
            );
            Ok(())
        });
        match result {
            Ok(()) => self.converted += 1,
            Err(e) => {
                self.failed += 1;
                eprintln!("FAILED {label}: {e:#}");
            }
        }
    }

    fn archive(
        &mut self,
        archive: &Archive,
        prefix: &Path,
        keys: Option<&GtaKeys>,
        depth: usize,
    ) -> Result<()> {
        if depth > 32 {
            bail!("nested archive depth exceeds 32");
        }
        archive.require_keys(keys)?;
        for file in archive.list_files() {
            // Even name lists in foreign archives must never escape -o.
            let rel = Path::new(&file.path);
            if rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
                || file.path.contains('\\')
            {
                bail!("unsafe archive entry path {}", file.path);
            }
            let ext = rel.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext.eq_ignore_ascii_case("ymt") {
                self.write(&file.path, &prefix.join(rel), archive.extract(file, keys));
            } else if ext.eq_ignore_ascii_case("rpf") {
                let nested = archive
                    .open_nested(file, keys)
                    .and_then(|nested| self.archive(&nested, &prefix.join(rel), keys, depth + 1));
                if let Err(e) = nested {
                    self.failed += 1;
                    eprintln!("FAILED {}: {e:#}", file.path);
                }
            }
        }
        Ok(())
    }
}

pub fn run(args: &ExportArgs, keys: Option<&GtaKeys>) -> Result<()> {
    let mut exporter = Exporter {
        args,
        names: crate::names::load(&args.names, None)?,
        converted: 0,
        failed: 0,
    };
    for path in &args.paths {
        let result = (|| -> Result<()> {
            if path.is_dir() {
                let mut files = crate::utils::walkdir(path)?;
                files.sort();
                for file in files {
                    if file
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("ymt"))
                    {
                        exporter.write(
                            &file.display().to_string(),
                            file.strip_prefix(path)?,
                            std::fs::read(&file).map_err(Into::into),
                        );
                    }
                }
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("rpf"))
            {
                let archive = Archive::open(path, keys)?;
                // Preserve the input filename (including .rpf) to avoid two
                // archive exports with the same entry name overwriting each other.
                exporter.archive(
                    &archive,
                    Path::new(path.file_name().context("archive has no filename")?),
                    keys,
                    0,
                )?;
            } else {
                exporter.write(
                    &path.display().to_string(),
                    Path::new(path.file_name().context("input has no filename")?),
                    std::fs::read(path).map_err(Into::into),
                );
            }
            Ok(())
        })();
        if let Err(e) = result {
            exporter.failed += 1;
            eprintln!("FAILED {}: {e:#}", path.display());
        }
    }
    eprintln!(
        "{} {}, failed {}",
        if args.dry_run {
            "Would export"
        } else {
            "Exported"
        },
        exporter.converted,
        exporter.failed
    );
    if exporter.failed != 0 {
        bail!("{} metadata export(s) failed", exporter.failed);
    }
    if exporter.converted == 0 {
        bail!("no .ymt metadata files found");
    }
    Ok(())
}
