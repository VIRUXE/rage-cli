// `rpf resource info`: inspect a loose RSC7 file (or an entry inside an
// archive) — header, then a summary of what the resource holds.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use rage_formats::{parse_drawables, parse_ytd, prepare_rsc7, resource_size_from_flags,
                   resource_version_from_flags, DrawableEntry, DrawableKind, YtdTexture,
                   RSC7_MAGIC, RSC8_MAGIC};

use crate::resources::load_resource_bytes;
use crate::rpf::GtaKeys;
use crate::utils::json_string;

#[derive(clap::Args)]
pub struct ResourceArgs {
    #[command(subcommand)]
    pub command: ResourceCommand,
}

#[derive(clap::Subcommand)]
pub enum ResourceCommand {
    /// Print the RSC7 header and a summary of a .ydr/.ydd/.yft/.ytd
    Info(InfoArgs),
}

#[derive(clap::Args)]
pub struct InfoArgs {
    /// A loose resource file on disk, or (with --archive) a name inside the archive
    pub file: String,

    /// Look `FILE` up inside this RPF archive instead of on disk
    #[arg(short, long, value_name = "RPF")]
    pub archive: Option<PathBuf>,

    /// Print one JSON object instead of text
    #[arg(long)]
    pub json: bool,
}

/// "FXAP": the header Cfx.re asset escrow puts on encrypted stream files.
const FXAP_MAGIC: u32 = 0x5041_5846;

/// The 16-byte RSC7 header plus what could be learnt about the body.
#[derive(Debug, PartialEq)]
pub struct Rsc7Header {
    pub version: u32,
    pub system_flags: u32,
    pub graphics_flags: u32,
    pub system_size: usize,
    pub graphics_size: usize,
    /// Body is deflated (false when it is stored raw).
    pub compressed: bool,
    /// Size of the body as read, in bytes.
    pub body_len: usize,
}

pub fn parse_header(data: &[u8]) -> Result<Rsc7Header> {
    if data.len() < 16 {
        anyhow::bail!("file is {} bytes; an RSC7 header needs 16", data.len());
    }

    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if magic == FXAP_MAGIC {
        anyhow::bail!("FiveM escrow-encrypted asset (magic 'FXAP'); only the server that bought it can decrypt it");
    }
    if magic == RSC8_MAGIC {
        anyhow::bail!("Gen9 RSC8 resources are not supported (magic 0x{magic:08X})");
    }
    if magic != RSC7_MAGIC {
        anyhow::bail!("not an RSC7 resource (magic 0x{magic:08X}, expected 0x{RSC7_MAGIC:08X})");
    }

    let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let system_flags = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let graphics_flags = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let system_size = resource_size_from_flags(system_flags);
    let graphics_size = resource_size_from_flags(graphics_flags);

    // prepare_rsc7 inflates when it can and otherwise hands the body back as
    // stored; the two cases are told apart by whether the system section it
    // returned is literally the start of the body.
    let body = &data[16..];
    let (system, _) = prepare_rsc7(data)?;
    let compressed = !(body.len() >= system_size + graphics_size && body[..system_size] == system[..]);

    Ok(Rsc7Header {
        version, system_flags, graphics_flags, system_size, graphics_size,
        compressed, body_len: body.len(),
    })
}

/// What the body was parsed as, keyed off the file extension.
enum Contents {
    Textures(Vec<YtdTexture>),
    Drawables(Vec<DrawableEntry>),
    Other,
}

fn parse_contents(name: &str, data: &[u8]) -> Result<Contents> {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("");

    if ext.eq_ignore_ascii_case("ytd") {
        return Ok(Contents::Textures(parse_ytd(data).context("failed to parse texture dictionary")?));
    }
    if let Some(kind) = DrawableKind::from_extension(ext) {
        return Ok(Contents::Drawables(parse_drawables(data, kind).context("failed to parse drawable")?));
    }
    Ok(Contents::Other)
}

pub fn run(args: &ResourceArgs, keys: Option<&GtaKeys>, verbose: bool) -> Result<()> {
    match &args.command {
        ResourceCommand::Info(info) => run_info(info, keys, verbose),
    }
}

fn run_info(args: &InfoArgs, keys: Option<&GtaKeys>, verbose: bool) -> Result<()> {
    let data = load_resource_bytes(&args.file, args.archive.as_deref(), keys)?;
    let header = parse_header(&data).with_context(|| format!("'{}'", args.file))?;
    let contents = parse_contents(&args.file, &data)?;

    let mut out = String::new();
    if args.json {
        write_json(&mut out, args, &header, &contents, verbose);
    } else {
        write_text(&mut out, args, &header, &contents, verbose);
    }
    print!("{out}");
    Ok(())
}

// ─── text ────────────────────────────────────────────────────────────────────

fn write_text(out: &mut String, args: &InfoArgs, h: &Rsc7Header, contents: &Contents, verbose: bool) {
    use std::fmt::Write;

    match &args.archive {
        Some(archive) => writeln!(out, "File:      {} : {}", archive.display(), args.file).unwrap(),
        None => writeln!(out, "File:      {}", args.file).unwrap(),
    }
    writeln!(out, "Format:    RSC7  version {}", h.version).unwrap();
    let from_flags = resource_version_from_flags(h.system_flags, h.graphics_flags);
    if from_flags != h.version {
        writeln!(out, "           (warning: flags encode version {from_flags})").unwrap();
    }
    writeln!(out, "System:    0x{:08X} flags -> {} bytes", h.system_flags, h.system_size).unwrap();
    writeln!(out, "Graphics:  0x{:08X} flags -> {} bytes", h.graphics_flags, h.graphics_size).unwrap();
    writeln!(out, "Body:      {} bytes ({})", h.body_len, if h.compressed { "deflated" } else { "stored" }).unwrap();

    match contents {
        Contents::Other => writeln!(out, "Summary:   not a drawable or texture dictionary").unwrap(),
        Contents::Textures(textures) => {
            writeln!(out, "Textures:  {}", textures.len()).unwrap();
            for tex in textures {
                write_texture_line(out, tex);
            }
        }
        Contents::Drawables(entries) => {
            writeln!(out, "Drawables: {}", entries.len()).unwrap();
            for entry in entries {
                write_drawable(out, entry, verbose);
            }
        }
    }
}

/// Same line format as `rpf textures`, so the two commands agree.
fn write_texture_line(out: &mut String, tex: &YtdTexture) {
    use std::fmt::Write;
    let name = if tex.name.is_empty() { format!("0x{:08X}", tex.name_hash) } else { tex.name.clone() };
    writeln!(
        out, "  {} — {}x{}x{} {} {} mip(s) ({} bytes)",
        name, tex.width, tex.height, tex.depth, tex.format, tex.levels, tex.pixel_data.len(),
    ).unwrap();
}

fn write_drawable(out: &mut String, entry: &DrawableEntry, verbose: bool) {
    use std::fmt::Write;
    let d = &entry.drawable;

    writeln!(out).unwrap();
    writeln!(out, "  {} (0x{:08X})", if d.name.is_empty() { &entry.name } else { &d.name }, entry.hash).unwrap();

    let (bounds, computed) = match d.best_lod() {
        Some(lod) => d.bounds_or_computed(lod),
        None => (d.bounds.clone(), false),
    };
    let c = bounds.center;
    writeln!(out, "    bounds:   center ({:.3}, {:.3}, {:.3}) radius {:.3}{}",
             c.x, c.y, c.z, bounds.sphere_radius, if computed { " (computed)" } else { "" }).unwrap();
    writeln!(out, "              min ({:.3}, {:.3}, {:.3}) max ({:.3}, {:.3}, {:.3})",
             bounds.box_min.x, bounds.box_min.y, bounds.box_min.z,
             bounds.box_max.x, bounds.box_max.y, bounds.box_max.z).unwrap();
    writeln!(out, "    lod dist: {:.1} / {:.1} / {:.1} / {:.1}",
             d.lod_distances[0], d.lod_distances[1], d.lod_distances[2], d.lod_distances[3]).unwrap();

    if verbose {
        if let Some(lod) = d.best_lod() {
            let geoms = d.geometry_bounds(lod);
            if !geoms.is_empty() {
                writeln!(out, "    geometry bounds:").unwrap();
                for g in &geoms {
                    writeln!(out, "      #{} model {} shader {}   {} tris, {} verts",
                             g.geometry, g.model, g.shader_id, g.triangles, g.vertices).unwrap();
                    writeln!(out, "           min ({:.3}, {:.3}, {:.3}) max ({:.3}, {:.3}, {:.3}) centroid ({:.3}, {:.3}, {:.3})",
                             g.min.x, g.min.y, g.min.z, g.max.x, g.max.y, g.max.z,
                             g.centroid.x, g.centroid.y, g.centroid.z).unwrap();
                }
            }
        }
    }

    for lod in &d.lods {
        let geometries: usize = lod.models.iter().map(|m| m.geometries.len()).sum();
        writeln!(out, "    {:<8} {} models, {} geometries, {} triangles",
                 format!("{}:", lod.level), lod.models.len(), geometries, d.triangle_count(lod)).unwrap();
    }

    if let Some(group) = &d.shader_group {
        writeln!(out, "    shaders:  {}", group.shaders.len()).unwrap();
        for (id, shader) in group.shaders.iter().enumerate() {
            let diffuse = d.diffuse_texture_name(id as u16).unwrap_or("-");
            writeln!(out, "      #{:<3} name 0x{:08X}  file 0x{:08X}  bucket {}  diffuse {}",
                     id, shader.name_hash, shader.file_name_hash, shader.render_bucket, diffuse).unwrap();
        }
        writeln!(out, "    embedded textures: {}", group.textures.len()).unwrap();
        for tex in &group.textures {
            out.push_str("  ");
            write_texture_line(out, tex);
        }
    } else {
        writeln!(out, "    shaders:  none").unwrap();
    }
}

// ─── json ────────────────────────────────────────────────────────────────────

fn write_json(out: &mut String, args: &InfoArgs, h: &Rsc7Header, contents: &Contents, verbose: bool) {
    use std::fmt::Write;

    let (kind, body) = match contents {
        Contents::Other => ("other", String::new()),
        Contents::Textures(textures) => ("textures", format!(",\"textures\":{}", json_textures(textures))),
        Contents::Drawables(entries) => {
            let items: Vec<String> = entries.iter().map(|e| json_drawable(e, verbose)).collect();
            ("drawables", format!(",\"drawables\":[{}]", items.join(",")))
        }
    };

    write!(
        out,
        "{{\"file\":{},\"archive\":{},\"format\":\"RSC7\",\"version\":{},\"system_flags\":\"0x{:08X}\",\"system_size\":{},\"graphics_flags\":\"0x{:08X}\",\"graphics_size\":{},\"body_bytes\":{},\"compressed\":{},\"kind\":\"{}\"{}}}\n",
        json_string(&args.file),
        args.archive.as_ref().map_or("null".to_string(), |a| json_string(&a.to_string_lossy())),
        h.version, h.system_flags, h.system_size, h.graphics_flags, h.graphics_size,
        h.body_len, h.compressed, kind, body,
    ).unwrap();
}

fn json_textures(textures: &[YtdTexture]) -> String {
    let items: Vec<String> = textures.iter().map(|tex| format!(
        "{{\"name\":{},\"hash\":\"0x{:08X}\",\"width\":{},\"height\":{},\"depth\":{},\"format\":\"{}\",\"mips\":{},\"bytes\":{}}}",
        json_string(&tex.name), tex.name_hash, tex.width, tex.height, tex.depth,
        tex.format, tex.levels, tex.pixel_data.len(),
    )).collect();
    format!("[{}]", items.join(","))
}

fn json_vec3(v: &rage_formats::Vec3) -> String {
    format!("[{},{},{}]", v.x, v.y, v.z)
}

fn json_geometry_bounds(geoms: &[rage_formats::GeometryBounds]) -> String {
    let items: Vec<String> = geoms.iter().map(|g| format!(
        "{{\"model\":{},\"geometry\":{},\"shader\":{},\"vertices\":{},\"triangles\":{},\"min\":{},\"max\":{},\"centroid\":{}}}",
        g.model, g.geometry, g.shader_id, g.vertices, g.triangles,
        json_vec3(&g.min), json_vec3(&g.max), json_vec3(&g.centroid),
    )).collect();
    format!("[{}]", items.join(","))
}

fn json_drawable(entry: &DrawableEntry, verbose: bool) -> String {
    let d = &entry.drawable;
    let (bounds, computed) = match d.best_lod() {
        Some(lod) => d.bounds_or_computed(lod),
        None => (d.bounds.clone(), false),
    };

    let geometry_bounds = if verbose {
        d.best_lod().map(|lod| format!(",\"geometry_bounds\":{}", json_geometry_bounds(&d.geometry_bounds(lod))))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let lods: Vec<String> = d.lods.iter().map(|lod| {
        let geometries: usize = lod.models.iter().map(|m| m.geometries.len()).sum();
        format!("{{\"level\":\"{}\",\"models\":{},\"geometries\":{},\"triangles\":{}}}",
                lod.level, lod.models.len(), geometries, d.triangle_count(lod))
    }).collect();

    let (shaders, textures) = match &d.shader_group {
        Some(group) => {
            let shaders: Vec<String> = group.shaders.iter().enumerate().map(|(id, s)| format!(
                "{{\"id\":{},\"name_hash\":\"0x{:08X}\",\"file_name_hash\":\"0x{:08X}\",\"render_bucket\":{},\"diffuse\":{}}}",
                id, s.name_hash, s.file_name_hash, s.render_bucket,
                d.diffuse_texture_name(id as u16).map_or("null".to_string(), json_string),
            )).collect();
            (format!("[{}]", shaders.join(",")), json_textures(&group.textures))
        }
        None => ("[]".to_string(), "[]".to_string()),
    };

    format!(
        "{{\"name\":{},\"hash\":\"0x{:08X}\",\"bounds\":{{\"center\":{},\"radius\":{},\"min\":{},\"max\":{},\"computed\":{}}}{},\"lod_distances\":[{},{},{},{}],\"lods\":[{}],\"shaders\":{},\"textures\":{}}}",
        json_string(if d.name.is_empty() { &entry.name } else { &d.name }), entry.hash,
        json_vec3(&bounds.center), bounds.sphere_radius, json_vec3(&bounds.box_min), json_vec3(&bounds.box_max), computed,
        geometry_bounds,
        d.lod_distances[0], d.lod_distances[1], d.lod_distances[2], d.lod_distances[3],
        lods.join(","), shaders, textures,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(magic: &[u8; 4], version: u32, sys: u32, gfx: u32, body: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(magic);
        data.extend_from_slice(&version.to_le_bytes());
        data.extend_from_slice(&sys.to_le_bytes());
        data.extend_from_slice(&gfx.to_le_bytes());
        data.extend_from_slice(body);
        data
    }

    #[test]
    fn parses_a_stored_header() {
        let data = header(b"RSC7", 165, 0xA800_0004, 0x5000_0000, &[0u8; 8192]);
        let h = parse_header(&data).unwrap();
        assert_eq!(h, Rsc7Header {
            version: 165, system_flags: 0xA800_0004, graphics_flags: 0x5000_0000,
            system_size: 8192, graphics_size: 0, compressed: false, body_len: 8192,
        });
    }

    #[test]
    fn rejects_bad_magic() {
        let err = parse_header(&header(b"XXXX", 0, 0, 0, &[])).unwrap_err().to_string();
        assert!(err.contains("0x58585858"), "{err}");
    }

    #[test]
    fn names_fivem_escrow_files() {
        let err = parse_header(&header(b"FXAP", 0, 0, 0, &[0u8; 32])).unwrap_err().to_string();
        assert!(err.contains("FiveM escrow"), "{err}");
    }

    #[test]
    fn rejects_rsc8() {
        let err = parse_header(&header(b"RSC8", 0, 0, 0, &[])).unwrap_err().to_string();
        assert!(err.contains("RSC8"), "{err}");
    }

    #[test]
    fn rejects_short_input() {
        assert!(parse_header(b"RSC7").is_err());
    }
}
