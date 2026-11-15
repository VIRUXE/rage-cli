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
    let mut value = json::object! {
        schema: "rage-renderware-info/1", file: name, format: format.name(),
        container: "RenderWare", chunks: json::JsonValue::Array(chunks),
        padding_bytes: doc.padding.len()
    };
    if format==Format::Txd {
        let txd=rage_formats::renderware::txd::Txd::parse(data)?;
        value["textures"]=json::JsonValue::Array(txd.native_textures.iter().map(|t|json::object!{
            name:t.name.clone(),mask:t.mask.clone(),platform:t.platform,width:t.width,height:t.height,
            depth:t.depth,mip_levels:t.mips.len(),available_mips:t.available_mips(),
            empty_mip_records:t.mips.len()-t.available_mips(),raster_format:t.raster_format,d3d_format:t.d3d_format
        }).collect());
    }
    if format==Format::Dff {
        let dff=rage_formats::renderware::dff::Dff::parse(data)?;
        value["frames"]=json::JsonValue::Array(dff.frames.iter().enumerate().map(|(i,f)|json::object!{
            index:i,name:f.name.clone(),parent:f.parent,flags:f.flags,
            hanim_id:f.hanim.as_ref().map(|h|h.id),hanim_nodes:f.hanim.as_ref().map_or(0,|h|h.nodes.len())
        }).collect());
        value["geometries"]=json::JsonValue::Array(dff.geometries.iter().enumerate().map(|(i,g)|json::object!{
            index:i,vertices:g.vertex_count(),triangles:g.effective_triangles().len(),morph_targets:g.morph_targets.len(),uv_sets:g.uv_sets.len(),
            skin_bones:g.skin.as_ref().map_or(0,|s|s.inverse_bind_matrices.len()),
            non_finite_uv_components:g.uv_sets.iter().flatten().flatten().filter(|v|!v.is_finite()).count(),
            plugins:json::JsonValue::Array(g.plugins.iter().map(|p|p.kind.into()).collect()),
            materials:json::JsonValue::Array(g.materials.iter().map(|m|json::object!{
                color:json::JsonValue::Array(m.color.iter().map(|c|(*c).into()).collect()),
                diffuse:m.texture.as_ref().map(|t|t.name.clone()),
                plugins:json::JsonValue::Array(m.plugins.iter().map(|p|p.kind.into()).collect())
            }).collect())
        }).collect());
        value["atomics"]=json::JsonValue::Array(dff.atomics.iter().map(|a|json::object!{frame:a.frame,geometry:a.geometry,flags:a.flags}).collect());
    }
    if json { return Ok(value.pretty(2) + "\n"); }
    let mut out = format!("{} — {} (RenderWare)\n", name, format.name().to_uppercase());
    for c in &doc.chunks {
        out.push_str(&format!("  chunk 0x{:08X}, version 0x{:08X}: {} payload bytes\n", c.kind, c.version, c.payload.len()));
    }
    if format==Format::Dff {
        out.push_str(&format!("  {} frames, {} geometries, {} atomics\n",value["frames"].len(),value["geometries"].len(),value["atomics"].len()));
        for g in value["geometries"].members(){out.push_str(&format!("  geometry {}: {} vertices, {} triangles, {} UV sets, {} skin bones\n",g["index"],g["vertices"],g["triangles"],g["uv_sets"],g["skin_bones"]));}
    }
    for texture in value["textures"].members() {
        out.push_str(&format!("  {} — {}x{}, {} mip(s), platform {}\n",texture["name"],texture["width"],texture["height"],texture["mip_levels"],texture["platform"]));
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
