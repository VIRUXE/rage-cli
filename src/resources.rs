// Shared helpers for commands that pull renderable resources (textures,
// drawables) out of an RPF archive or a loose file on disk.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use rage_formats::{parse_drawables, parse_yft, parse_ytd, Drawable, DrawableEntry, DrawableKind, Fragment,
                   YtdTexture};
use rpf_archive::RpfEntryKind;

use crate::rpf::{Archive, GtaKeys};

/// Finds `name` inside `archive` (case-insensitive name or path), extracts it
/// and returns its raw bytes. The entry must be a resource file (the kind
/// used by .ytd/.ydr/.ydd/.yft).
pub fn load_resource(archive: &Archive, name: &str, keys: Option<&GtaKeys>) -> Result<Vec<u8>> {
    let name_lower = name.to_lowercase();
    let file_ref = archive.find_file(&name_lower).with_context(|| {
        format!(
            "'{}' not found in archive (retail x64*.rpf keep drawables in nested .rpf files — run `rpf search <archive> <name>` to find which one, then extract it)",
            name
        )
    })?;

    if !matches!(archive.entry_kind(file_ref), RpfEntryKind::ResourceFile { .. }) {
        anyhow::bail!("'{}' is not a resource file", name);
    }

    archive
        .extract(file_ref, keys)
        .with_context(|| format!("failed to extract '{}'", name))
}

/// Raw bytes of a resource: read straight from disk, or, when `archive` is
/// given, looked up inside that archive by name.
pub fn load_resource_bytes(file: &str, archive: Option<&Path>, keys: Option<&GtaKeys>) -> Result<Vec<u8>> {
    match archive {
        None => fs::read(file).with_context(|| format!("failed to read '{}'", file)),
        Some(archive_path) => {
            let archive = Archive::open(archive_path, keys)?;
            archive.require_keys(keys)?;
            load_resource(&archive, file, keys)
        }
    }
}

/// The extension of a (possibly archive-relative) name, without the dot;
/// empty when there is none.
pub fn extension_of(name: &str) -> &str {
    Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("")
}

/// The drawable kind `name`'s extension says it is, or an error naming the
/// extensions that would do.
fn drawable_kind_of(name: &str) -> Result<DrawableKind> {
    DrawableKind::from_extension(extension_of(name))
        .with_context(|| format!("'{}' is not a drawable (.ydr/.ydd/.yft)", name))
}

/// Loads and parses `name` from `archive` as a drawable of `kind` (a .ydr,
/// .ydd or .yft) into its list of drawable entries.
pub fn load_drawables(
    archive: &Archive,
    name: &str,
    kind: DrawableKind,
    keys: Option<&GtaKeys>,
) -> Result<Vec<DrawableEntry>> {
    let data = load_resource(archive, name, keys)?;
    parse_drawables(&data, kind).with_context(|| format!("failed to parse drawable '{}'", name))
}

/// What a drawable resource loaded for rendering turned out to hold.
pub enum Loaded {
    /// A .ydr or .ydd: independent drawables.
    Entries(Vec<DrawableEntry>),
    /// A .yft, kept whole so its physics children can be placed on the body.
    Fragment(Fragment),
}

impl Loaded {
    /// Every drawable in the resource, whatever its role.
    pub fn drawables(&self) -> Vec<&Drawable> {
        match self {
            Loaded::Entries(entries) => entries.iter().map(|entry| &entry.drawable).collect(),
            Loaded::Fragment(fragment) => fragment
                .drawable
                .iter()
                .chain(fragment.children.iter().filter_map(|child| child.drawable.as_ref()))
                .chain(fragment.extra_drawables.iter().map(|entry| &entry.drawable))
                .collect(),
        }
    }
}

/// Loads `name` (a .ydr, .ydd or .yft) from `archive` for rendering: a
/// fragment comes back whole, anything else as its drawable entries.
pub fn load_renderables(archive: &Archive, name: &str, keys: Option<&GtaKeys>) -> Result<Loaded> {
    let kind = drawable_kind_of(name)?;
    if kind == DrawableKind::Yft {
        let data = load_resource(archive, name, keys)?;
        let fragment = parse_yft(&data).with_context(|| format!("failed to parse fragment '{}'", name))?;
        return Ok(Loaded::Fragment(fragment));
    }
    Ok(Loaded::Entries(load_drawables(archive, name, kind, keys)?))
}

/// Normalises a `--file`-style spec into the lookup name used against an
/// archive: appends ".ytd" when the spec carries no extension of its own.
pub fn ytd_lookup_name(spec: &str) -> String {
    if Path::new(spec).extension().is_some() {
        spec.to_string()
    } else {
        format!("{spec}.ytd")
    }
}

/// Loads a texture dictionary either from a loose RSC7 file on disk (when
/// `spec` names an existing file) or from `archive` (appending ".ytd" when
/// `spec` has no extension).
pub fn load_texture_dictionary(archive: &Archive, spec: &str, keys: Option<&GtaKeys>) -> Result<Vec<YtdTexture>> {
    if Path::new(spec).is_file() {
        let data = fs::read(spec).with_context(|| format!("failed to read '{}'", spec))?;
        return parse_ytd(&data).with_context(|| format!("failed to parse YTD '{}'", spec));
    }

    let lookup_name = ytd_lookup_name(spec);
    let data = load_resource(archive, &lookup_name, keys)?;
    parse_ytd(&data).with_context(|| format!("failed to parse YTD '{}'", lookup_name))
}

/// Collects every texture referenced by the drawables' shader groups,
/// deduplicated by lowercase name (first occurrence wins), in encounter order.
pub fn embedded_textures(entries: &[DrawableEntry]) -> Vec<&YtdTexture> {
    embedded_textures_of(entries.iter().map(|entry| &entry.drawable))
}

/// [`embedded_textures`] over any set of drawables.
pub fn embedded_textures_of<'a>(drawables: impl IntoIterator<Item = &'a Drawable>) -> Vec<&'a YtdTexture> {
    let mut seen = std::collections::HashSet::new();
    let mut textures = Vec::new();

    for drawable in drawables {
        let Some(shader_group) = &drawable.shader_group else { continue };
        for tex in &shader_group.textures {
            if seen.insert(tex.name.to_lowercase()) {
                textures.push(tex);
            }
        }
    }

    textures
}

/// Reduces a name to characters that are safe in a file name: a crafted name
/// containing `..`/`/` must not be able to escape the destination directory.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();

    if cleaned.is_empty() { "entry".to_string() } else { cleaned }
}

/// The file stem of a (possibly archive-relative) path: `"a/b/prop_x.ydr"` ->
/// `"prop_x"`.
pub fn file_stem(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stem_strips_dirs_and_extension() {
        assert_eq!(file_stem("a/b/prop_x.ydr"), "prop_x");
        assert_eq!(file_stem("prop_x.ydr"), "prop_x");
        assert_eq!(file_stem("prop_x"), "prop_x");
        assert_eq!(file_stem("a\\b\\vehicles.ytd"), "vehicles");
    }

    #[test]
    fn sanitize_strips_path_traversal_characters() {
        assert_eq!(sanitize("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize("prop_x"), "prop_x");
        assert_eq!(sanitize(""), "entry");
    }

    #[test]
    fn ytd_lookup_name_appends_extension_when_missing() {
        assert_eq!(ytd_lookup_name("vehicles"), "vehicles.ytd");
        assert_eq!(ytd_lookup_name("vehicles.ytd"), "vehicles.ytd");
        assert_eq!(ytd_lookup_name("path/to/vehicles"), "path/to/vehicles.ytd");
        assert_eq!(ytd_lookup_name("weird.name.ytd"), "weird.name.ytd");
    }
}
