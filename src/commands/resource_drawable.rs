//! Drawables and bounds for `rage resource`: `dump` to XML (with the DDS files), `build` from it,
//! and the skeleton, lights and bound lines of `info`.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use rage_formats::blocks::bounds::{BoundBlock, BoundKind};
use rage_formats::blocks::drawable::{Drawable, DrawableModel, DrawableModelsBlock};
use rage_formats::blocks::shader::ShaderGroup;
use rage_formats::blocks::skeleton::Skeleton;
use rage_formats::blocks::texture::TextureDictionary;
use rage_formats::{
    build_ybn_from_xml_checked, build_ydr_from_xml_checked, dump_ybn_xml, dump_ydr_xml, read_ybn, read_ydr, BlockId, Graph, NameTable,
};

use super::resource::BuildArgs;

/// A bound's kind and, for a composite, how many children it holds.
#[derive(Debug, PartialEq)]
pub struct BoundInfo {
    pub kind: BoundKind,
    pub children: Option<usize>,
}

impl BoundInfo {
    fn of(g: &Graph, id: BlockId) -> Self {
        let b = g.get::<BoundBlock>(id);
        let children = match b {
            BoundBlock::Composite(c) => Some(c.children.iter().flatten().count()),
            _ => None,
        };
        BoundInfo { kind: b.common().kind, children }
    }

    /// `Composite (2 children)`.
    pub fn label(&self) -> String {
        match self.children {
            Some(n) => format!("{} ({n} children)", self.kind.name()),
            None => self.kind.name().to_owned(),
        }
    }
}

/// What the block reader knows about a `.ydr` that the older parser does not.
#[derive(Debug, Default, PartialEq)]
pub struct DrawableExtras {
    pub bones: Option<usize>,
    pub lights: usize,
    pub bound: Option<BoundInfo>,
}

impl DrawableExtras {
    /// `None` when the block reader rejects the file (`info` then prints what it always did).
    pub fn read(data: &[u8]) -> Option<Self> {
        let (g, root) = read_ydr(data).ok()?;
        let d = g.get::<Drawable>(root);
        Some(DrawableExtras {
            bones: d.skeleton.map(|s| g.get::<Skeleton>(s).bones_count as usize),
            lights: d.lights_count,
            bound: d.bound.map(|b| BoundInfo::of(&g, b)),
        })
    }

    pub fn write_text(&self, out: &mut String) {
        use std::fmt::Write;
        if let Some(n) = self.bones {
            writeln!(out, "    skeleton: {n} bones").unwrap();
        }
        if self.lights > 0 {
            writeln!(out, "    lights:   {}", self.lights).unwrap();
        }
        if let Some(b) = &self.bound {
            writeln!(out, "    bound:    {}", b.label()).unwrap();
        }
    }

    /// `,"skeleton_bones":N,"lights":N,"bound":{"kind":..}` members to append to a drawable's JSON object.
    pub fn json_members(&self) -> String {
        let mut s = String::new();
        if let Some(n) = self.bones {
            s.push_str(&format!(",\"skeleton_bones\":{n}"));
        }
        if self.lights > 0 {
            s.push_str(&format!(",\"lights\":{}", self.lights));
        }
        if let Some(b) = &self.bound {
            s.push_str(&format!(",\"bound\":{{\"kind\":\"{}\"", b.kind.name()));
            if let Some(n) = b.children {
                s.push_str(&format!(",\"children\":{n}"));
            }
            s.push('}');
        }
        s
    }
}

/// The bound of a `.ybn`, for `info`; `None` when the block reader rejects the file.
pub fn read_bound_info(data: &[u8]) -> Option<BoundInfo> {
    let (g, root) = read_ybn(data).ok()?;
    Some(BoundInfo::of(&g, root))
}

/// Which of the two formats a name says, by its extension.
pub fn kind_of_name(name: &str) -> Option<&'static str> {
    match Path::new(name).extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref() {
        Some("ydr") => Some("ydr"),
        Some("ybn") => Some("ybn"),
        Some("ynd") => Some("ynd"),
        _ => None,
    }
}

/// `resource dump` of a `.ydr` or `.ybn`; embedded textures are saved as `.dds` beside `output`
/// (or in the current directory) unless `no_dds`.
pub fn dump(kind: &str, data: &[u8], names: &NameTable, output: Option<&Path>, no_dds: bool, json: bool) -> Result<String> {
    if json {
        bail!("drawables and bounds dump as XML only");
    }
    if kind == "ybn" {
        return dump_ybn_xml(data);
    }
    if kind == "ynd" {
        return rage_formats::dump_ynd_xml(data, names);
    }
    let dir = if no_dds {
        None
    } else {
        let dir = output.and_then(Path::parent).filter(|p| !p.as_os_str().is_empty()).map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        Some(dir)
    };
    dump_ydr_xml(data, names, dir.as_deref())
}

/// Which format an XML document is, by its root element.
pub fn sniff_xml(text: &str) -> Option<&'static str> {
    let doc = roxmltree::Document::parse(text).ok()?;
    match doc.root_element().tag_name().name() {
        "Drawable" => Some("ydr"),
        "BoundsFile" | "Bounds" => Some("ybn"),
        "NodeDictionary" => Some("ynd"),
        _ => None,
    }
}

/// `resource build` of a `.ydr` or `.ybn` from XML: builds, checks that the file reads back
/// identically, prints the warnings (an error with `--strict`), writes it and reports on stderr.
pub fn build(kind: &str, text: &str, args: &BuildArgs) -> Result<()> {
    let built = if kind == "ydr" {
        let dir = args.textures.clone().unwrap_or_else(|| args.file.parent().filter(|p| !p.as_os_str().is_empty()).map_or_else(|| PathBuf::from("."), Path::to_path_buf));
        build_ydr_from_xml_checked(text, Some(&dir))
    } else if kind == "ynd" {
        build_ynd_checked(text)
    } else {
        build_ybn_from_xml_checked(text)
    };
    let built = built.with_context(|| format!("'{}'", args.file.display()))?;
    for warning in &built.warnings {
        eprintln!("warning: {warning}");
    }
    if args.strict && !built.warnings.is_empty() {
        bail!("{} warning(s) (--strict)", built.warnings.len());
    }
    let bytes = built.bytes;
    let summary = if kind == "ydr" {
        drawable_summary(&bytes, &args.file)?
    } else if kind == "ynd" {
        let ynd = rage_formats::parse_ynd(&bytes).context("the written path nodes cannot be read back")?;
        format!("RSC7 path nodes from {}: {} nodes, {} links, {} junctions", args.file.display(), ynd.nodes.len(), ynd.nodes.iter().map(|n| n.links.len()).sum::<usize>(), ynd.junctions.len())
    } else {
        format!("RSC7 bounds from {}: {}", args.file.display(), read_bound_info(&bytes).context("the written bound cannot be read back")?.label())
    };
    if let Some(parent) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&args.output, &bytes).with_context(|| format!("writing {}", args.output.display()))?;
    eprintln!("Wrote {} ({} bytes, {})", args.output.display(), bytes.len(), summary);
    Ok(())
}

/// `XmlYnd.GetYnd` then a write, read back and compared as XML: a file that does not read back
/// identically is an error, never handed over.
fn build_ynd_checked(text: &str) -> Result<rage_formats::Built> {
    let ynd = rage_formats::ynd_from_xml(text)?;
    let bytes = rage_formats::serialize_ynd(&ynd)?;
    let names = NameTable::core();
    let expected = rage_formats::ynd_to_xml(&ynd, &names);
    let back = rage_formats::dump_ynd_xml(&bytes, &names).context("the written file cannot be read back")?;
    if expected != back {
        bail!("the written file does not read back identically");
    }
    Ok(rage_formats::Built { bytes, xml: expected, warnings: Vec::new() })
}

fn drawable_summary(bytes: &[u8], from: &Path) -> Result<String> {
    let (g, root) = read_ydr(bytes).context("the written drawable cannot be read back")?;
    let d = g.get::<Drawable>(root);
    let models = d.models.map(|m| g.get::<DrawableModelsBlock>(m).all_models()).unwrap_or_default();
    let geometries: usize = models.iter().map(|m| g.get::<DrawableModel>(*m).geometries.len()).sum();
    let group = d.shader_group.map(|s| g.get::<ShaderGroup>(s));
    let shaders = group.map_or(0, |s| s.count(&g));
    let textures = group.and_then(|s| s.dictionary).map_or(0, |t| g.get::<TextureDictionary>(t).count(&g));
    Ok(format!("RSC7 drawable from {}: {} models, {geometries} geometries, {shaders} shaders, {textures} textures", from.display(), models.len()))
}
