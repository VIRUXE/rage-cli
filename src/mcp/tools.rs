//! The tool table: what `tools/list` advertises and what `tools/call`
//! runs. Each tool is a thin wrapper over a command's value-returning core
//! and returns the same object that command's `--json` prints.

use anyhow::{bail, Context as _, Result};
use json::JsonValue;
use std::path::{Path, PathBuf};

use super::base64;
use super::rpc::RpcError;
use crate::catalog::search::parse_size_spec;
use crate::catalog::{self, Catalog, Kind};
use crate::commands::catalog as catalog_cmd;
use crate::commands::screenshot;
use crate::rpf::GtaKeys;

/// What every tool call can reach.
pub struct Context<'a> {
    /// `--db` as given, or `None` to resolve it from the game build.
    pub db: Option<&'a Path>,
    pub output: &'a Path,
    pub keys: Option<&'a GtaKeys>,
    pub exe: Option<&'a Path>,
}

impl Context<'_> {
    fn db(&self) -> Result<PathBuf> {
        catalog::resolve_path(self.db, self.exe)
    }

    /// `output_dir` from the arguments, else `--output` plus `sub`.
    fn out_dir(&self, args: &Args, sub: &str) -> Result<PathBuf, RpcError> {
        Ok(match args.string("output_dir")? {
            Some(d) => PathBuf::from(d),
            None => self.output.join(sub),
        })
    }
}

/// What a tool call produced: the content items and the structured form.
#[derive(Debug)]
pub struct ToolResult {
    pub content: Vec<JsonValue>,
    pub structured: JsonValue,
    pub is_error: bool,
}

impl ToolResult {
    fn ok(value: JsonValue) -> Self {
        let text = value.pretty(2);
        Self { content: vec![json::object! { type: "text", text: text }], structured: value, is_error: false }
    }

    fn error(err: &anyhow::Error) -> Self {
        let text = format!("{err:#}");
        Self { content: vec![json::object! { type: "text", text: text.clone() }], structured: json::object! { error: text }, is_error: true }
    }

    fn with_image(mut self, path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else { return self };
        let mime = match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("webp") => "image/webp",
            _ => "image/png",
        };
        self.content.push(json::object! { type: "image", data: base64::encode(&bytes), mimeType: mime });
        self
    }

    pub fn json(&self) -> JsonValue {
        json::object! { content: self.content.clone(), structuredContent: self.structured.clone(), isError: self.is_error }
    }
}

/// Typed access to a call's `arguments`, with `-32602` on a wrong type.
pub struct Args<'a>(pub &'a JsonValue);

impl Args<'_> {
    fn get(&self, key: &str) -> Option<&JsonValue> {
        let v = &self.0[key];
        (!v.is_null()).then_some(v)
    }

    pub fn string(&self, key: &str) -> Result<Option<String>, RpcError> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v.as_str().map(|s| Some(s.to_string())).ok_or_else(|| RpcError::invalid_params(format!("'{key}' must be a string"))),
        }
    }

    pub fn string_or(&self, key: &str, default: &str) -> Result<String, RpcError> {
        Ok(self.string(key)?.unwrap_or_else(|| default.to_string()))
    }

    pub fn required(&self, key: &str) -> Result<String, RpcError> {
        self.string(key)?.ok_or_else(|| RpcError::invalid_params(format!("'{key}' is required")))
    }

    pub fn boolean(&self, key: &str, default: bool) -> Result<bool, RpcError> {
        match self.get(key) {
            None => Ok(default),
            Some(v) => v.as_bool().ok_or_else(|| RpcError::invalid_params(format!("'{key}' must be a boolean"))),
        }
    }

    pub fn integer(&self, key: &str, default: u64) -> Result<u64, RpcError> {
        match self.get(key) {
            None => Ok(default),
            Some(v) => v.as_u64().ok_or_else(|| RpcError::invalid_params(format!("'{key}' must be a non-negative integer"))),
        }
    }

    pub fn optional_integer(&self, key: &str) -> Result<Option<u64>, RpcError> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v.as_u64().map(Some).ok_or_else(|| RpcError::invalid_params(format!("'{key}' must be a non-negative integer"))),
        }
    }

    /// A list of strings; a single string or a comma-separated string is
    /// accepted too, as the CLI's `value_delimiter = ','` would.
    pub fn strings(&self, key: &str) -> Result<Vec<String>, RpcError> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(v) if v.is_array() => v
                .members()
                .map(|m| m.as_str().map(str::to_string).ok_or_else(|| RpcError::invalid_params(format!("'{key}' must be a list of strings"))))
                .collect(),
            Some(v) => match v.as_str() {
                Some(s) => Ok(s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()),
                None => Err(RpcError::invalid_params(format!("'{key}' must be a list of strings"))),
            },
        }
    }

    pub fn one_of(&self, key: &str, allowed: &[&str], default: &str) -> Result<String, RpcError> {
        let v = self.string_or(key, default)?;
        if allowed.contains(&v.as_str()) {
            Ok(v)
        } else {
            Err(RpcError::invalid_params(format!("'{key}' must be one of {}", allowed.join(", "))))
        }
    }
}

/// A value parser's error becomes a tool error (the value is of the right
/// type but means nothing), never a protocol error.
fn parsed<T>(what: &str, r: std::result::Result<T, String>) -> Result<T> {
    r.map_err(|e| anyhow::anyhow!("{what}: {e}"))
}

/// The search filters shared by `catalog_search` and `catalog_sheet`.
fn filters(args: &Args) -> Result<Result<catalog::search::Filters>, RpcError> {
    let min_size = args.string("min_size")?;
    let max_size = args.string("max_size")?;
    let fa = catalog_cmd::FilterArgs {
        kind: args.strings("kind")?,
        dlc: args.strings("dlc")?,
        min_size: None,
        max_size: None,
        all_copies: args.boolean("all_copies", false)?,
        annotated: args.boolean("annotated", false)?,
    };
    Ok((|| {
        let mut fa = fa;
        if let Some(s) = min_size {
            fa.min_size = Some(parsed("min_size", parse_size_spec(&s))?);
        }
        if let Some(s) = max_size {
            fa.max_size = Some(parsed("max_size", parse_size_spec(&s))?);
        }
        fa.to_filters()
    })())
}

const FILTER_PROPERTIES: &str = r##"{
  "kind": { "type": "array", "items": { "type": "string" }, "description": "Only these kinds: drawable, fragment, dictionary, dd_entry, txd, texture, or the aliases ydr, ydd, yft, ytd, model, tex" },
  "dlc": { "type": "array", "items": { "type": "string" }, "description": "Only these DLC packs (e.g. mpheist3), or the tiers base, update, dlc" },
  "min_size": { "type": "string", "description": "At least this big: N (largest extent) or X,Y,Z; metres for models, pixels for textures" },
  "max_size": { "type": "string", "description": "At most this big, same form as min_size" },
  "annotated": { "type": "boolean", "description": "Only items with a visual description (default false)" },
  "all_copies": { "type": "boolean", "description": "Include overridden copies, not just the one the game loads (default false)" }
}"##;

fn schema(properties: &str, required: &[&str]) -> JsonValue {
    let mut props = json::parse(properties).expect("tool schema is valid JSON");
    let filter_props = json::parse(FILTER_PROPERTIES).expect("filter schema is valid JSON");
    if props.has_key("$filters") {
        props.remove("$filters");
        for (k, v) in filter_props.entries() {
            props[k] = v.clone();
        }
    }
    let required: Vec<JsonValue> = required.iter().map(|r| JsonValue::from(*r)).collect();
    json::object! { type: "object", properties: props, required: required, additionalProperties: false }
}

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    properties: &'static str,
    required: &'static [&'static str],
}

impl Tool {
    pub fn json(&self) -> JsonValue {
        json::object! { name: self.name, description: self.description, inputSchema: schema(self.properties, self.required) }
    }
}

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "catalog_search",
        description: "Search the GTA V asset catalogue (every drawable, fragment, dictionary and texture in the install) by words over names, archive paths and visual review notes. Words are ANDed prefixes. Returns rage-catalog-search/1: results with provenance (archive, tier, DLC pack, nesting), bounds or texture size, annotations, and ready-to-run follow-up commands; and coverage (how much of the catalogue has a visual description, so absence from a result is not proof of absence). Try several phrasings before concluding an asset does not exist.",
        properties: r##"{
  "query": { "type": "string", "description": "Words to find; empty lists items by name, subject to the filters" },
  "$filters": {},
  "limit": { "type": "integer", "description": "Maximum results (default 20)" },
  "mode": { "type": "string", "enum": ["lexical", "semantic", "hybrid"], "description": "lexical (default) matches words; semantic ranks by meaning and hybrid fuses both, when the catalogue has embeddings" },
  "raw": { "type": "boolean", "description": "Pass the query to SQLite FTS5 as written (OR, NEAR, column:term)" }
}"##,
        required: &[],
    },
    Tool {
        name: "catalog_get",
        description: "Write one catalogued entry to disk as a loose file so it can be opened, inspected or rendered: the entry itself, or with rpf the nested .rpf archive that holds it (needed before screenshot or resource_info can reach an entry inside a nested archive), or with with_textures also the .ytd dictionaries of its texture chain. Returns the files written.",
        properties: r##"{
  "key": { "type": "string", "description": "Catalogue key (from catalog_search) or numeric id" },
  "output_dir": { "type": "string", "description": "Directory to write into (default: the server's output directory)" },
  "rpf": { "type": "boolean", "description": "Write the nested .rpf holding the entry instead of the entry" },
  "with_textures": { "type": "boolean", "description": "Also write the texture dictionaries the model draws from" }
}"##,
        required: &["key"],
    },
    Tool {
        name: "catalog_info",
        description: "Where the catalogue lives, which game build it describes, row and winner counts per kind, parse failures, and review coverage (annotations, packets, embeddings). Call this first: no catalogue means `rage catalog build` has not been run.",
        properties: "{}",
        required: &[],
    },
    Tool {
        name: "catalog_sheet",
        description: "Render blind, numbered contact sheets of a selection (a search, or explicit keys) for visual review: tiles show only a number and view names, never the asset's name, so the review is judged on appearance. Writes sheet-NNN.png, packet.json (tile hashes, sizes, missing-texture counts) and responses.template.json. Look at each sheet, describe every tile, then import with catalog_annotate. Set inline_images to receive the sheets as image content.",
        properties: r##"{
  "query": { "type": "string", "description": "Words to select items with, as in catalog_search" },
  "keys": { "type": "array", "items": { "type": "string" }, "description": "Select these catalogue keys or ids instead of a query" },
  "$filters": {},
  "limit": { "type": "integer", "description": "Most items to select (default 64)" },
  "output_dir": { "type": "string", "description": "Directory for the sheets and packet (default: a new folder under the server's output directory)" },
  "views": { "type": "array", "items": { "type": "string" }, "description": "Views per model tile: front, back, left, right, top, iso, or AZIMUTH:ELEVATION in degrees (default front, iso, top)" },
  "facing": { "type": "string", "enum": ["auto", "vehicle", "prop"], "description": "Which way models face (default auto)" },
  "per_sheet": { "type": "integer", "description": "Tiles per sheet (default 16)" },
  "cell": { "type": "integer", "description": "Pixel size of each view, 16 to 2048 (default 256)" },
  "inline_images": { "type": "boolean", "description": "Also return each sheet as an image content item (default false)" }
}"##,
        required: &[],
    },
    Tool {
        name: "catalog_reveal",
        description: "Which asset each tile of a packet shows, for a person to check a review afterwards. Do not call this before the tiles have been reviewed: it defeats the point of a blind sheet.",
        properties: r##"{
  "packet_id": { "type": "string", "description": "The packet_id from catalog_sheet or packet.json" }
}"##,
        required: &["packet_id"],
    },
    Tool {
        name: "catalog_annotate",
        description: "Import a reviewer's descriptions of a packet into the catalogue, where they become searchable at once. Each tile's entry must quote its sha256 from packet.json; descriptions of any other image are refused, and sheet files that changed on disk are refused when verify_files is set. Give the responses as a path or as the object itself (schema rage-catalog-responses/1). Never mark an AI review as reviewer human.",
        properties: r##"{
  "packet": { "type": "string", "description": "Path to the packet.json catalog_sheet wrote" },
  "responses": { "type": "string", "description": "Path to the filled-in responses file" },
  "responses_json": { "type": "object", "description": "The responses object itself, instead of a path; it is written beside packet.json and imported from there" },
  "reviewer": { "type": "string", "enum": ["agent", "human"], "description": "Who wrote the descriptions (default: the responses' own reviewer field)" },
  "reviewer_name": { "type": "string", "description": "Which agent or person, e.g. a model name" },
  "verify_files": { "type": "boolean", "description": "Also check the sheet images beside packet.json still hash as issued (default true)" }
}"##,
        required: &["packet"],
    },
    Tool {
        name: "screenshot",
        description: "Render a model to images from one or more views and return the paths (and the images, with inline_images). Pick the model by catalogue key (nested archives are unpacked first), by archive path and file name, by ped name (composed from its variation info, with components), or by vehicle name (with the _hi model, a livery and carcols paint). Returns rage-screenshot/1: per entry the images, triangle and geometry counts, missing textures (surfaces drawn grey that are not grey in the game), the texture dictionaries used, and warnings.",
        properties: r##"{
  "key": { "type": "string", "description": "A catalogue key from catalog_search (a model: drawable, fragment, dictionary or dd_entry)" },
  "archive": { "type": "string", "description": "Path to an .rpf archive (with file)" },
  "file": { "type": "string", "description": "Name of a .ydr, .ydd or .yft inside the archive" },
  "ped": { "type": "string", "description": "A ped name, e.g. a_m_y_acult_01; needs the game (GTAV_PATH)" },
  "components": { "type": "array", "items": { "type": "string" }, "description": "With ped: SLOT=D[:T[:A]] or SLOT=none per slot (head berd hair uppr lowr hand feet teef accs task decl jbib)" },
  "vehicle": { "type": "string", "description": "A vehicle model name, e.g. police; needs the game" },
  "hi": { "type": "boolean", "description": "Use the high-detail _hi model when the game has one" },
  "livery": { "type": "integer", "description": "Show livery N (0-based)" },
  "colour_from": { "type": "string", "description": "carcols or carcols:C: paint from the model's carvariations colour combination C" },
  "paint": { "type": "string", "description": "#rrggbb body colour for vehicle paint shaders" },
  "entry": { "type": "string", "description": "For .ydd/.yft: render only the entry with this name or 0x hash" },
  "views": { "type": "array", "items": { "type": "string" }, "description": "front, back, left, right, top, iso, or AZIMUTH:ELEVATION in degrees (default iso)" },
  "facing": { "type": "string", "enum": ["auto", "vehicle", "prop"], "description": "Which way the model faces (default auto)" },
  "size": { "type": "string", "description": "Image size as WxH (default 1024x1024)" },
  "grid": { "type": "boolean", "description": "Also combine all views into one labelled grid image" },
  "lod": { "type": "string", "enum": ["high", "medium", "low", "verylow"], "description": "LOD to render (default high)" },
  "format": { "type": "string", "enum": ["png", "jpg", "webp"], "description": "Image format (default png)" },
  "background": { "type": "string", "description": "grey (default), transparent, or #rrggbb" },
  "output_dir": { "type": "string", "description": "Directory for the images (default: under the server's output directory)" },
  "inline_images": { "type": "boolean", "description": "Also return each image as an image content item (default false)" }
}"##,
        required: &[],
    },
    Tool {
        name: "resource_info",
        description: "The header and a summary of a resource file (.ydr .ydd .yft .ytd .ybn .ynd .ymap .ytyp .ymf): a loose file on disk, or a name inside an .rpf archive. Same object as `rage resource info --json`.",
        properties: r##"{
  "file": { "type": "string", "description": "A loose resource file on disk, or (with archive) a name inside the archive" },
  "archive": { "type": "string", "description": "Look file up inside this .rpf archive instead of on disk" },
  "limit": { "type": "integer", "description": "How many entities of a map to list, 0 for all (default 50)" },
  "names": { "type": "array", "items": { "type": "string" }, "description": "Extra name lists (one name per line) for resolving hashes" }
}"##,
        required: &["file"],
    },
];

pub fn find(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.name == name)
}

/// Runs a tool. `Err` is a protocol error (unknown tool, wrong argument
/// types); a failure inside the tool is an `is_error` result.
pub fn call(ctx: &Context, name: &str, args: &JsonValue) -> Result<ToolResult, RpcError> {
    if find(name).is_none() {
        return Err(RpcError::invalid_params(format!("unknown tool: {name}")));
    }
    if !args.is_null() && !args.is_object() {
        return Err(RpcError::invalid_params("arguments must be an object"));
    }
    let args = Args(args);
    let outcome = match name {
        "catalog_search" => catalog_search(ctx, &args)?,
        "catalog_get" => catalog_get(ctx, &args)?,
        "catalog_info" => ctx.db().and_then(|db| catalog_cmd::info_json(&db)).map(ToolResult::ok),
        "catalog_sheet" => catalog_sheet(ctx, &args)?,
        "catalog_reveal" => {
            let pid = args.required("packet_id")?;
            ctx.db().and_then(|db| catalog_cmd::reveal_json(&db, &pid)).map(ToolResult::ok)
        }
        "catalog_annotate" => catalog_annotate(ctx, &args)?,
        "screenshot" => screenshot_tool(ctx, &args)?,
        "resource_info" => resource_info(ctx, &args)?,
        _ => unreachable!("every advertised tool is handled"),
    };
    Ok(outcome.unwrap_or_else(|e| ToolResult::error(&e)))
}

fn catalog_search(ctx: &Context, args: &Args) -> Result<Result<ToolResult>, RpcError> {
    let text = args.string_or("query", "")?;
    let filters = filters(args)?;
    let limit = args.integer("limit", 20)? as usize;
    let mode = match args.one_of("mode", &["lexical", "semantic", "hybrid"], "lexical")?.as_str() {
        "semantic" => catalog::search::Mode::Semantic,
        "hybrid" => catalog::search::Mode::Hybrid,
        _ => catalog::search::Mode::Lexical,
    };
    let raw = args.boolean("raw", false)?;
    Ok((|| {
        let db = ctx.db()?;
        let req = catalog_cmd::SearchRequest { text: &text, filters: filters?, limit, raw, mode };
        catalog_cmd::search_json(&db, &req).map(ToolResult::ok)
    })())
}

fn catalog_get(ctx: &Context, args: &Args) -> Result<Result<ToolResult>, RpcError> {
    let key = args.required("key")?;
    let out = ctx.out_dir(args, "get")?;
    let rpf = args.boolean("rpf", false)?;
    let with_textures = args.boolean("with_textures", false)?;
    Ok((|| {
        let db = ctx.db()?;
        let outcome = catalog_cmd::get_files(&db, ctx.keys, &key, &out, rpf, with_textures)?;
        let files: Vec<String> = outcome.files.iter().map(|p| absolute(p)).collect();
        let item = outcome.item.as_ref();
        Ok(ToolResult::ok(json::object! {
            key: key.clone(),
            name: item.map(|i| i.label()),
            kind: item.map(|i| i.kind.as_str()),
            files: files,
            warnings: outcome.warnings.clone(),
        }))
    })())
}

fn catalog_sheet(ctx: &Context, args: &Args) -> Result<Result<ToolResult>, RpcError> {
    let query = args.string_or("query", "")?;
    let keys = args.strings("keys")?;
    let filters = filters(args)?;
    let limit = args.integer("limit", 64)? as usize;
    let out_dir = ctx.out_dir(args, &format!("sheet-{}", catalog::now_secs()))?;
    let views = args.strings("views")?;
    let views = if views.is_empty() { vec!["front".into(), "iso".into(), "top".into()] } else { views };
    let facing = args.one_of("facing", &["auto", "vehicle", "prop"], "auto")?;
    let per_sheet = args.integer("per_sheet", 16)? as usize;
    let cell = args.integer("cell", 256)? as u32;
    let inline = args.boolean("inline_images", false)?;
    Ok((|| {
        let db = ctx.db()?;
        let req = catalog_cmd::SheetRequest {
            query,
            keys,
            filters: filters?,
            limit,
            views: catalog_cmd::parse_views(&views)?,
            facing: parsed("facing", screenshot::parse_facing(&facing))?,
            per_sheet: per_sheet.max(1),
            cell,
            out_dir,
            write_tiles: false,
            seed: None,
            texture_budget_mib: 512,
            quiet: true,
        };
        let summary = catalog_cmd::make_sheet(&db, ctx.keys, &req)?;
        let mut value = catalog_cmd::sheet_json(&summary);
        value["instructions"] = "Look at each sheet. Describe only what is visible in each numbered tile; fill a copy of responses_template keeping every tile's sha256, leave description empty for tiles you cannot judge, set reviewer to agent, then call catalog_annotate.".into();
        let mut result = ToolResult::ok(value);
        if inline {
            for sheet in &summary.sheets {
                result = result.with_image(sheet);
            }
        }
        Ok(result)
    })())
}

fn catalog_annotate(ctx: &Context, args: &Args) -> Result<Result<ToolResult>, RpcError> {
    let packet = PathBuf::from(args.required("packet")?);
    let responses = args.string("responses")?;
    let responses_json = args.get("responses_json").cloned();
    if let Some(v) = &responses_json
        && !v.is_object()
    {
        return Err(RpcError::invalid_params("'responses_json' must be an object"));
    }
    let reviewer = match args.string("reviewer")? {
        Some(r) if r != "agent" && r != "human" => return Err(RpcError::invalid_params("'reviewer' must be agent or human")),
        r => r,
    };
    let reviewer_name = args.string("reviewer_name")?;
    let verify_files = args.boolean("verify_files", true)?;
    Ok((|| {
        let responses_path = match (responses, responses_json) {
            (Some(p), _) => PathBuf::from(p),
            (None, Some(v)) => {
                let dir = packet.parent().map(Path::to_path_buf).unwrap_or_default();
                let path = dir.join(format!("responses-{}.json", catalog::now_secs()));
                std::fs::write(&path, v.pretty(2)).with_context(|| format!("failed to write {}", path.display()))?;
                path
            }
            (None, None) => bail!("give 'responses' (a path) or 'responses_json' (the object)"),
        };
        let db = ctx.db()?;
        let mut cat = Catalog::open_existing(&db)?;
        let opts = catalog::annotate::AnnotateOptions { reviewer, reviewer_name, verify_files };
        let summary = catalog::annotate::import(&mut cat, &packet, &responses_path, &opts)?;
        let mut value = summary.json();
        value["responses"] = absolute(&responses_path).into();
        Ok(ToolResult::ok(value))
    })())
}

fn screenshot_tool(ctx: &Context, args: &Args) -> Result<Result<ToolResult>, RpcError> {
    let key = args.string("key")?;
    let archive = args.string("archive")?;
    let file = args.string("file")?;
    let ped = args.string("ped")?;
    let components = args.strings("components")?;
    let vehicle = args.string("vehicle")?;
    let hi = args.boolean("hi", false)?;
    let livery = args.optional_integer("livery")?.map(|n| n as usize);
    let colour_from = args.string("colour_from")?;
    let paint = args.string("paint")?;
    let entry = args.string("entry")?;
    let views = args.strings("views")?;
    let views = if views.is_empty() { vec!["iso".to_string()] } else { views };
    let facing = args.one_of("facing", &["auto", "vehicle", "prop"], "auto")?;
    let size = args.string_or("size", "1024x1024")?;
    let grid = args.boolean("grid", false)?;
    let lod = args.one_of("lod", &["high", "medium", "low", "verylow"], "high")?;
    let format = args.one_of("format", &["png", "jpg", "webp"], "png")?;
    let background = args.string_or("background", "grey")?;
    let inline = args.boolean("inline_images", false)?;
    let selectors = [key.is_some(), archive.is_some() || file.is_some(), ped.is_some(), vehicle.is_some()].iter().filter(|b| **b).count();
    if selectors != 1 {
        return Err(RpcError::invalid_params("give exactly one of key, archive+file, ped or vehicle"));
    }
    if archive.is_some() != file.is_some() {
        return Err(RpcError::invalid_params("archive and file go together"));
    }
    let out_dir = ctx.out_dir(args, "screenshot")?;
    Ok((|| {
        let (archive, file, entry) = match &key {
            Some(key) => {
                let (a, f, e) = resolve_key(ctx, key, &out_dir)?;
                (Some(a), Some(f), e.or(entry))
            }
            None => (archive.map(PathBuf::from), file, entry),
        };
        let sargs = screenshot::ScreenshotArgs {
            archive,
            file,
            ped,
            component: components,
            vehicle,
            hi,
            livery,
            colour_from: match colour_from {
                Some(c) => Some(parsed("colour_from", screenshot::parse_colour_from(&c))?),
                None => None,
            },
            output: Some(out_dir),
            ytd: Vec::new(),
            views: catalog_cmd::parse_views(&views)?,
            facing: parsed("facing", screenshot::parse_facing(&facing))?,
            size: parsed("size", screenshot::parse_size(&size))?,
            grid,
            lod: lod.parse().map_err(|e| anyhow::anyhow!("lod: {e}"))?,
            format: format.parse().map_err(|e| anyhow::anyhow!("format: {e}"))?,
            background: parsed("background", screenshot::parse_background(&background))?,
            cull: false,
            vertex_colors: false,
            entry,
            paint: match paint {
                Some(p) => Some(parsed("paint", screenshot::parse_paint(&p))?),
                None => None,
            },
            no_index: false,
            no_cluster_framing: false,
            json: true,
        };
        let report = screenshot::render(&sargs, ctx.keys, ctx.exe)?;
        let mut result = ToolResult::ok(report.json());
        if inline {
            for e in &report.entries {
                for (_, path) in &e.images {
                    result = result.with_image(path);
                }
                if let Some(g) = &e.grid {
                    result = result.with_image(g);
                }
            }
        }
        Ok(result)
    })())
}

/// A catalogue key as `screenshot` wants it: the archive to open, the file
/// inside, and for a dictionary member the `--entry` filter. An entry inside
/// a nested archive has that archive written under `out_dir/rpf/` first.
fn resolve_key(ctx: &Context, key: &str, out_dir: &Path) -> Result<(PathBuf, String, Option<String>)> {
    let db = ctx.db()?;
    let cat = Catalog::open_existing(&db)?;
    let item = catalog::search::find_item(&cat, key)?;
    if !item.kind.is_model() {
        bail!("'{}' is a {}, not a model; screenshot renders drawables, fragments and dictionaries", key, item.kind.as_str());
    }
    let entry = match (item.kind, &item.member) {
        (Kind::DdEntry, Some(m)) => Some(m.split('~').next().unwrap_or(m).to_string()),
        _ => None,
    };
    if item.nested.is_empty() {
        return Ok((PathBuf::from(&item.archive_path), item.inner_path.clone(), entry));
    }
    let rpf_dir = out_dir.join("rpf").join(safe_dir_name(key));
    let outcome = catalog_cmd::get_files(&db, ctx.keys, key, &rpf_dir, true, false)?;
    let archive = outcome.files.into_iter().next().context("the nested archive was not written")?;
    Ok((archive, item.inner_path.clone(), entry))
}

fn safe_dir_name(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect()
}

fn resource_info(ctx: &Context, args: &Args) -> Result<Result<ToolResult>, RpcError> {
    let file = args.required("file")?;
    let archive = args.string("archive")?.map(PathBuf::from);
    let limit = args.integer("limit", 50)? as usize;
    let names = args.strings("names")?.into_iter().map(PathBuf::from).collect();
    Ok((|| {
        let iargs = crate::commands::resource::InfoArgs { file, archive, json: true, limit, names };
        let text = crate::commands::resource::info_text(&iargs, ctx.keys, false)?;
        let value = json::parse(&text).context("resource info produced invalid JSON")?;
        Ok(ToolResult::ok(value))
    })())
}

fn absolute(p: &Path) -> String {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_valid_schema_with_its_required_keys() {
        for tool in TOOLS {
            let j = tool.json();
            let schema = &j["inputSchema"];
            assert_eq!(schema["type"], "object", "{}", tool.name);
            for r in schema["required"].members() {
                assert!(schema["properties"].has_key(r.as_str().unwrap()), "{}: required {} is not a property", tool.name, r);
            }
            assert!(!schema["properties"].has_key("$filters"), "{}: filters not expanded", tool.name);
        }
        assert!(find("catalog_search").unwrap().json()["inputSchema"]["properties"].has_key("min_size"));
        assert!(find("nope").is_none());
    }

    #[test]
    fn arguments_are_typed() {
        let v = json::object! { s: "x", b: true, n: 3, list: ["a", "b"], csv: "a, b", bad: 1.5 };
        let a = Args(&v);
        assert_eq!(a.string("s").unwrap().as_deref(), Some("x"));
        assert_eq!(a.string("missing").unwrap(), None);
        assert!(a.string("n").is_err());
        assert!(a.boolean("b", false).unwrap());
        assert!(a.boolean("s", false).is_err());
        assert_eq!(a.integer("n", 0).unwrap(), 3);
        assert!(a.integer("bad", 0).is_err());
        assert_eq!(a.strings("list").unwrap(), vec!["a", "b"]);
        assert_eq!(a.strings("csv").unwrap(), vec!["a", "b"]);
        assert!(a.strings("n").is_err());
        assert_eq!(a.one_of("s", &["x", "y"], "y").unwrap(), "x");
        assert!(a.one_of("s", &["y"], "y").is_err());
        assert_eq!(a.required("missing").unwrap_err().code, super::super::rpc::INVALID_PARAMS);
    }

    #[test]
    fn filters_reject_junk_as_a_tool_error_not_a_protocol_error() {
        let v = json::object! { kind: ["ydr"], min_size: "1,2,3", max_size: "big" };
        let r = filters(&Args(&v)).expect("types are right");
        assert!(r.is_err(), "'big' is not a size");
        let v = json::object! { kind: ["ydr"], min_size: "1,2,3" };
        let f = filters(&Args(&v)).unwrap().unwrap();
        assert_eq!(f.kinds, vec![Kind::Drawable]);
        assert!(f.min_size.is_some());
    }

    #[test]
    fn screenshot_needs_exactly_one_selector() {
        let ctx = Context { db: None, output: Path::new("."), keys: None, exe: None };
        let err = call(&ctx, "screenshot", &json::object! {}).unwrap_err();
        assert!(err.message.contains("exactly one"));
        let err = call(&ctx, "screenshot", &json::object! { key: "k", ped: "p" }).unwrap_err();
        assert!(err.message.contains("exactly one"));
        let err = call(&ctx, "screenshot", &json::object! { archive: "a.rpf" }).unwrap_err();
        assert!(err.message.contains("go together"));
        assert_eq!(call(&ctx, "no_such_tool", &json::object! {}).unwrap_err().code, super::super::rpc::INVALID_PARAMS);
    }
}
