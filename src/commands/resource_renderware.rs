//! CLI presentation for independently implemented RenderWare codecs.
use anyhow::{Context, Result};
use std::path::Path;
use rage_formats::renderware::{Document, Format};

pub fn format(name: &str) -> Option<Format> {
    Path::new(name).extension().and_then(|e| e.to_str()).and_then(Format::from_extension)
}

pub fn info(name: &str, data: &[u8], json: bool) -> Result<String> {
    let format = format(name).context("unrecognized classic GTA file extension")?;
    let doc = Document::parse(data, format)?;
    let chunks: Vec<_> = doc.chunks.iter().map(|c| json::object! {
        kind: c.kind, version: c.version, payload_bytes: c.payload.len()
    }).collect();
    let value = json::object! {
        schema: "rage-renderware-info/1", file: name, format: format.name(),
        container: "RenderWare", chunks: json::JsonValue::Array(chunks),
        padding_bytes: doc.padding.len()
    };
    if json { return Ok(value.pretty(2) + "\n"); }
    let mut out = format!("{} — {} (RenderWare)\n", name, format.name().to_uppercase());
    for c in &doc.chunks {
        out.push_str(&format!("  chunk 0x{:08X}, version 0x{:08X}: {} payload bytes\n", c.kind, c.version, c.payload.len()));
    }
    if !doc.padding.is_empty() { out.push_str(&format!("  {} bytes of archive padding\n", doc.padding.len())); }
    Ok(out)
}

pub fn dump(name: &str, data: &[u8]) -> Result<String> {
    Document::parse(data, format(name).context("unknown classic GTA format")?)?.to_json()
}

pub fn build(text: &str, format: Format, output: &Path) -> Result<()> {
    let doc = Document::from_json(text)?;
    anyhow::ensure!(doc.format == format, "document is {}, output requests {}", doc.format.name(), format.name());
    let bytes = doc.build()?;
    Document::parse(&bytes, format).context("built classic GTA file failed validation")?;
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) { std::fs::create_dir_all(parent)?; }
    std::fs::write(output, &bytes).with_context(|| format!("writing {}", output.display()))?;
    println!("Built {} ({} bytes)", output.display(), bytes.len());
    Ok(())
}
