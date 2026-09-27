//! `rage catalog`: a searchable SQLite catalogue of every drawable and
//! texture in the game, built for agents (JSON with provenance) as much as
//! for people. The heavy lifting lives in `crate::catalog`.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::catalog::search::{self, parse_size_spec, Filters, Mode, SearchQuery, SizeSpec};
use crate::catalog::{self, human_duration, scan, thousands, Catalog, Kind};
use crate::index::{load_entry, EntryLoc};
use crate::rpf::GtaKeys;

#[derive(clap::Args)]
pub struct CatalogArgs {
    /// The catalogue database (default ~/.rage-cli/catalog/<game build>/catalog.sqlite)
    #[arg(long, global = true, value_name = "FILE", env = "RAGE_CATALOG")]
    pub db: Option<PathBuf>,

    #[command(subcommand)]
    pub command: CatalogCommand,
}

#[derive(clap::Subcommand)]
pub enum CatalogCommand {
    /// Walk the game in load order and record every drawable, fragment,
    /// drawable-dictionary entry and texture with its size, LODs, texture
    /// dictionaries and archetype. Later runs rescan only archives that changed
    Build(BuildArgs),

    /// Search names, archive paths and review notes. Only the copy the game
    /// loads (the winner) is shown unless --all-copies; --json carries enough
    /// provenance to render or extract a hit directly
    Search(SearchArgs),

    /// Write one catalogued entry, or with --rpf the nested archive holding
    /// it, to disk so `screenshot`, `textures` and `resource info` can open it
    Get(GetArgs),

    /// Where the catalogue lives, which game build it describes, row counts
    /// and review coverage
    Info(InfoArgs),

    /// Render blind, numbered contact sheets of a selection for a reviewer
    /// (a vision model or a person) and write packet.json and a responses
    /// template. Tiles show a number and view names only: the mapping to
    /// assets stays in the catalogue until `--reveal`
    Sheet(SheetArgs),

    /// Import a reviewer's descriptions of a packet. Each must quote its
    /// tile's image hash from packet.json; descriptions of any other image
    /// are refused. Imported text becomes searchable at once
    Annotate(AnnotateArgs),

    /// Share reviews as text-only packs: descriptions and asset identities,
    /// never images or game data. Imported reviews are marked as shared, not
    /// local, and never replace a local review
    Pack(PackArgs),

    /// Compute local text embeddings of reviewed items (and with --models,
    /// of every model name) for `search --semantic` and `--hybrid`. Needs a
    /// rage built with `--features semantic`; the model downloads on first use
    Embed(EmbedArgs),
}

#[derive(clap::Args)]
pub struct EmbedArgs {
    /// Also embed every model the game loads, not just reviewed items (slow on small CPUs)
    #[arg(long)]
    pub models: bool,

    /// Re-embed everything, not just new or changed text
    #[arg(long)]
    pub all: bool,

    /// Embed at most this many items
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Only say whether this rage can embed (exit 0) or not (exit 1)
    #[arg(long)]
    pub check: bool,

    /// Print one JSON object instead of a summary line
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct PackArgs {
    #[command(subcommand)]
    pub command: PackCommand,
}

#[derive(clap::Subcommand)]
pub enum PackCommand {
    /// Write this catalogue's own visual reviews to a pack
    Export {
        /// The pack file to write
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        /// Only reviews by agent or human
        #[arg(long, value_name = "WHO")]
        reviewer: Option<String>,
        /// Only reviews made on or after this Unix time
        #[arg(long, value_name = "SECS")]
        since: Option<i64>,
        /// Also pass on reviews that were themselves imported from packs
        #[arg(long)]
        include_shared: bool,
        /// Print one JSON object instead of a summary line
        #[arg(long)]
        json: bool,
    },
    /// Match a pack's reviews to this catalogue by asset identity and source bytes
    Import {
        /// The pack file to read
        pack: PathBuf,
        /// Exit non-zero when any entry is invalid
        #[arg(long)]
        strict: bool,
        /// Print one JSON object instead of a summary line
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
pub struct SheetArgs {
    /// Words to select items with, as in `catalog search`
    pub query: Vec<String>,

    #[command(flatten)]
    pub filters: FilterArgs,

    /// Select from a file instead: ids or keys one per line, or the output of `catalog search --json`
    #[arg(long, value_name = "FILE", conflicts_with = "query")]
    pub ids: Option<PathBuf>,

    /// Select these catalogue keys or ids (repeatable)
    #[arg(long, value_name = "KEY", conflicts_with = "query")]
    pub key: Vec<String>,

    /// Directory for sheet-NNN.png, packet.json and responses.template.json
    #[arg(short, long, value_name = "DIR", required_unless_present = "reveal")]
    pub output: Option<PathBuf>,

    /// Views per model tile: front, back, left, right, top, iso, or
    /// AZIMUTH:ELEVATION in degrees from the model's front (comma-separated)
    #[arg(long, value_delimiter = ',', default_value = "front,iso,top", value_name = "VIEWS")]
    pub views: Vec<String>,

    /// Which way models face: vehicle (+Y), prop (-Y), or auto (vehicle shaders mean +Y)
    #[arg(long, default_value = "auto", value_parser = crate::commands::screenshot::parse_facing, value_name = "FACING")]
    pub facing: rage_render::Facing,

    /// Tiles per sheet
    #[arg(long, default_value = "16", value_name = "N")]
    pub per_sheet: usize,

    /// Pixel size of each view
    #[arg(long, default_value = "256", value_name = "PX")]
    pub cell: u32,

    /// Most items to select
    #[arg(long, default_value = "64", value_name = "N")]
    pub limit: usize,

    /// Also write each tile on its own under tiles/
    #[arg(long)]
    pub tiles: bool,

    /// Layout seed (default: derived from the packet, so reruns match)
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,

    /// Texture cache budget in MiB while rendering
    #[arg(long, default_value = "512", value_name = "MIB")]
    pub texture_budget: usize,

    /// Print which asset each tile of this packet shows, for people once reviewing is done
    #[arg(long, value_name = "PACKET_ID")]
    pub reveal: Option<String>,

    /// Print one JSON object instead of text
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct AnnotateArgs {
    /// The packet.json `catalog sheet` wrote
    #[arg(long, value_name = "FILE")]
    pub packet: PathBuf,

    /// The reviewer's answers (see responses.template.json)
    #[arg(long, value_name = "FILE")]
    pub responses: PathBuf,

    /// Who wrote the descriptions: agent or human (default: the responses file's "reviewer")
    #[arg(long, value_name = "WHO")]
    pub reviewer: Option<String>,

    /// Which agent or person, e.g. a model name
    #[arg(long, value_name = "NAME")]
    pub reviewer_name: Option<String>,

    /// Also check the sheet images beside packet.json still hash as issued
    #[arg(long)]
    pub verify_files: bool,

    /// Exit non-zero when any description was refused
    #[arg(long)]
    pub strict: bool,

    /// Print one JSON object instead of a summary line
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    /// Game folder to scan instead of the one holding --exe (archives that
    /// need keys are then skipped unless --keys is given)
    #[arg(long, value_name = "DIR")]
    pub game: Option<PathBuf>,

    /// Only record these kinds: drawable, fragment, dictionary, dd_entry,
    /// txd, texture, or ydr/ydd/yft/ytd/model/tex (comma-separated)
    #[arg(long, value_delimiter = ',', value_name = "KIND")]
    pub kind: Vec<String>,

    /// Only scan these load tiers: base, update, dlc (comma-separated)
    #[arg(long, value_delimiter = ',', value_name = "TIER")]
    pub scope: Vec<String>,

    /// Rescan every archive, even unchanged ones
    #[arg(long)]
    pub full: bool,

    /// Worker threads (default: one per core)
    #[arg(long, value_name = "N")]
    pub jobs: Option<usize>,

    /// Extra hash-name lists to resolve names with (repeatable)
    #[arg(long, value_name = "FILE")]
    pub names: Vec<PathBuf>,

    /// Where to write the build report (default: catalog-report.json beside the database)
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,

    /// Exit non-zero when any archive or entry failed
    #[arg(long)]
    pub strict: bool,

    /// Print the build report as JSON instead of a summary line
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Clone)]
pub struct FilterArgs {
    /// Only these kinds: drawable, fragment, dictionary, dd_entry, txd,
    /// texture, or ydr/ydd/yft/ytd/model/tex (comma-separated)
    #[arg(long, value_delimiter = ',', value_name = "KIND")]
    pub kind: Vec<String>,

    /// Only these DLC packs, or the tiers base/update/dlc (comma-separated)
    #[arg(long, value_delimiter = ',', value_name = "PACK")]
    pub dlc: Vec<String>,

    /// At least this big: N (largest extent) or X,Y,Z; metres for models, pixels for textures
    #[arg(long, value_parser = parse_size_spec, value_name = "SIZE")]
    pub min_size: Option<SizeSpec>,

    /// At most this big: N (largest extent) or X,Y,Z; metres for models, pixels for textures
    #[arg(long, value_parser = parse_size_spec, value_name = "SIZE")]
    pub max_size: Option<SizeSpec>,

    /// Include overridden copies, not just the one the game loads
    #[arg(long)]
    pub all_copies: bool,

    /// Only items with a visual description
    #[arg(long)]
    pub annotated: bool,
}

impl FilterArgs {
    pub fn to_filters(&self) -> Result<Filters> {
        let mut kinds = Vec::new();
        for k in &self.kind {
            for kind in Kind::expand(k)? {
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
        }
        Ok(Filters {
            kinds,
            dlc: self.dlc.clone(),
            min_size: self.min_size,
            max_size: self.max_size,
            all_copies: self.all_copies,
            annotated_only: self.annotated,
        })
    }
}

#[derive(clap::Args)]
pub struct SearchArgs {
    /// Words to find in names, paths and descriptions (all must match; each
    /// is a prefix). None lists items by name, subject to the filters
    pub query: Vec<String>,

    #[command(flatten)]
    pub filters: FilterArgs,

    /// Maximum results
    #[arg(long, default_value = "20", value_name = "N")]
    pub limit: usize,

    /// Pass the query to SQLite FTS5 as written (OR, NEAR, column:term)
    #[arg(long)]
    pub raw: bool,

    /// Rank by meaning with local embeddings instead of words (needs `catalog embed`)
    #[arg(long, conflicts_with = "hybrid")]
    pub semantic: bool,

    /// Fuse word and meaning rankings
    #[arg(long)]
    pub hybrid: bool,

    /// Print one JSON object instead of a table
    #[arg(short, long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct GetArgs {
    /// Catalogue key (from `search --json`) or numeric id
    pub key: String,

    /// Directory to write into
    #[arg(short, long, value_name = "DIR")]
    pub output: PathBuf,

    /// Write the nested .rpf that holds the entry instead of the entry itself
    #[arg(long)]
    pub rpf: bool,

    /// Also write the texture dictionaries the model draws from (its txd chain)
    #[arg(long)]
    pub with_textures: bool,
}

#[derive(clap::Args)]
pub struct InfoArgs {
    /// Print one JSON object instead of text
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: &CatalogArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    // `embed --check` answers about this binary, not about any catalogue.
    if let CatalogCommand::Embed(e) = &args.command
        && e.check
    {
        return run_embed(e, Path::new(""));
    }
    let db = catalog::resolve_path(args.db.as_deref(), exe)?;
    match &args.command {
        CatalogCommand::Build(b) => run_build(b, &db, keys, exe),
        CatalogCommand::Search(s) => run_search(s, &db),
        CatalogCommand::Get(g) => run_get(g, &db, keys),
        CatalogCommand::Info(i) => run_info(i, &db, exe),
        CatalogCommand::Sheet(a) => run_sheet(a, &db, keys),
        CatalogCommand::Annotate(a) => run_annotate(a, &db),
        CatalogCommand::Pack(p) => run_pack(p, &db, keys),
        CatalogCommand::Embed(e) => run_embed(e, &db),
    }
}

fn run_build(args: &BuildArgs, db: &Path, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    let exe = match exe {
        Some(e) => Some(crate::keys::resolve_exe(e)?),
        None => None,
    };
    let game_root = match (&args.game, &exe) {
        (Some(g), _) => g.clone(),
        (None, Some(e)) => e.parent().map(Path::to_path_buf).context("GTA5.exe has no parent folder")?,
        (None, None) => bail!("--exe (or GTAV_PATH) or --game is required for `catalog build`"),
    };
    for tier in &args.scope {
        if !matches!(tier.as_str(), "base" | "update" | "dlc") {
            bail!("unknown --scope '{tier}' (expected base, update or dlc)");
        }
    }
    let mut kinds = Vec::new();
    for k in &args.kind {
        kinds.extend(Kind::expand(k)?);
    }
    let mut cat = Catalog::open(db)?;
    let opts = scan::BuildOptions {
        game_root,
        exe,
        kinds,
        scope: args.scope.clone(),
        full: args.full,
        jobs: args.jobs,
        names: args.names.clone(),
        report: args.report.clone(),
        quiet: false,
    };
    let summary = scan::build(&mut cat, keys, &opts)?;
    if args.json {
        print!("{}", std::fs::read_to_string(&summary.report_path)?);
        println!();
    } else {
        println!("{}", summary.line());
        println!("Catalogue: {}", db.display());
    }
    if args.strict && (!summary.failures.is_empty() || summary.failed > 0 || summary.needs_keys > 0) {
        bail!(
            "{} entr(ies) failed, {} archive(s) failed, {} need keys (see {})",
            summary.failures.len(),
            summary.failed,
            summary.needs_keys,
            summary.report_path.display()
        );
    }
    Ok(())
}

fn mode_of(args: &SearchArgs) -> Mode {
    if args.hybrid {
        Mode::Hybrid
    } else if args.semantic {
        Mode::Semantic
    } else {
        Mode::Lexical
    }
}

fn run_search(args: &SearchArgs, db: &Path) -> Result<()> {
    let cat = Catalog::open_existing(db)?;
    let filters = args.filters.to_filters()?;
    let joined = args.query.join(" ");
    let text = joined.as_str();
    let mode = mode_of(args);
    if mode != Mode::Lexical && text.trim().is_empty() {
        bail!("--semantic and --hybrid need a query");
    }
    let q = SearchQuery { text, raw: args.raw, filters: &filters, limit: args.limit, mode };
    let (hits, coverage) = search::search(&cat, &q)?;
    let db_str = db.to_string_lossy();

    if args.json {
        let game_build: json::JsonValue =
            cat.meta("game_build")?.and_then(|b| b.parse::<u64>().ok()).map(Into::into).unwrap_or(json::JsonValue::Null);
        let results: Vec<json::JsonValue> = hits.iter().map(|h| search::hit_json(h, &db_str)).collect();
        let out = json::object! {
            schema: "rage-catalog-search/1",
            query: text,
            mode: mode.as_str(),
            game_build: game_build,
            db: db_str.to_string(),
            coverage: search::coverage_json(&coverage),
            results: results,
        };
        println!("{}", out.pretty(2));
        return Ok(());
    }

    if hits.is_empty() {
        println!("No matches");
    } else {
        let stdout = std::io::stdout();
        let mut w = std::io::BufWriter::new(stdout.lock());
        writeln!(w, "{:>7}  {:<10} {:<34} {:<16} {:<14} {:>3}  location", "id", "kind", "name", "size", "tier/pack", "ann")?;
        writeln!(w, "{}", "-".repeat(120))?;
        for h in &hits {
            let i = &h.item;
            let size = match (i.kind, i.size, i.width, i.height) {
                (Kind::Texture, _, Some(w), Some(hh)) => format!("{w}x{hh} px"),
                (_, Some(s), _, _) => format!("{:.1}x{:.1}x{:.1} m", s[0], s[1], s[2]),
                _ => String::new(),
            };
            let tier = match &i.dlc_pack {
                Some(p) => p.clone(),
                None => i.tier.clone(),
            };
            let mut loc = crate::catalog::scan::location(&i.archive_rel, &i.nested, &i.inner_path);
            if let Some(m) = &i.member {
                loc.push('#');
                loc.push_str(m);
            }
            let mut name = i.label();
            if !i.winner {
                name.push_str(" (overridden)");
            }
            writeln!(
                w,
                "{:>7}  {:<10} {:<34} {:<16} {:<14} {:>3}  {}",
                i.id,
                i.kind.as_str(),
                truncate(&name, 34),
                size,
                truncate(&tier, 14),
                h.annotations.len(),
                loc
            )?;
            if let Some(a) = h.annotations.first() {
                writeln!(w, "{:>9}{}", "", truncate(&format!("\"{}\" ({} {})", a.description, a.reviewer, a.method), 110))?;
            }
        }
        w.flush()?;
    }
    eprintln!("{}", search::coverage_line(&coverage));
    Ok(())
}

fn run_sheet(args: &SheetArgs, db: &Path, keys: Option<&GtaKeys>) -> Result<()> {
    use crate::catalog::sheet;
    let mut cat = Catalog::open_existing(db)?;
    if let Some(pid) = &args.reveal {
        let rows = sheet::reveal(&cat, pid)?;
        if args.json {
            let tiles: Vec<json::JsonValue> = rows
                .iter()
                .map(|(t, k, n, f)| json::object! { tile: *t, key: k.clone(), name: n.clone(), sheet: f.clone() })
                .collect();
            println!("{}", json::object! { packet_id: pid.clone(), tiles: tiles }.pretty(2));
        } else {
            for (t, k, n, f) in rows {
                println!("#{t:<4} {:<12} {:<32} {k}", f, n.unwrap_or_default());
            }
        }
        return Ok(());
    }
    let out_dir = args.output.clone().context("-o DIR is required")?;
    let mut views = Vec::new();
    for v in &args.views {
        let view: rage_render::View = v.parse().map_err(|e| anyhow::anyhow!("--views: {e}"))?;
        if !views.contains(&view) {
            views.push(view);
        }
    }
    if !(16..=2048).contains(&args.cell) {
        bail!("--cell must be between 16 and 2048");
    }

    let filters = args.filters.to_filters()?;
    let mut items = Vec::new();
    let selection: Vec<String> = match &args.ids {
        Some(file) => crate::catalog::sheet::read_selection(file)?,
        None => args.key.clone(),
    };
    let query = args.query.join(" ");
    if !selection.is_empty() {
        for k in selection.iter().take(args.limit) {
            items.push(search::find_item(&cat, k)?);
        }
    } else {
        let q = SearchQuery { text: &query, raw: false, filters: &filters, limit: args.limit, mode: Mode::Lexical };
        let (hits, _) = search::search(&cat, &q)?;
        items = hits.into_iter().map(|h| h.item).collect();
    }
    if items.is_empty() {
        bail!("nothing selected");
    }
    let opts = sheet::SheetOptions {
        views,
        facing: args.facing,
        per_sheet: args.per_sheet,
        cell: args.cell,
        out_dir,
        write_tiles: args.tiles,
        seed: args.seed,
        query: (!query.is_empty()).then(|| query.clone()),
        filters: format!("{:?}", filters),
        texture_budget: args.texture_budget << 20,
        quiet: args.json,
    };
    let summary = sheet::make_packet(&mut cat, keys, items, &opts)?;
    for (key, why) in &summary.skipped {
        eprintln!("skipped {key}: {why}");
    }
    if args.json {
        let sheets: Vec<String> = summary.sheets.iter().map(|p| p.to_string_lossy().to_string()).collect();
        let skipped: Vec<json::JsonValue> = summary.skipped.iter().map(|(k, r)| json::object! { key: k.clone(), reason: r.clone() }).collect();
        println!("{}", json::object! {
            packet_id: summary.packet_id.clone(),
            packet: summary.packet_path.to_string_lossy().to_string(),
            responses_template: summary.template_path.to_string_lossy().to_string(),
            sheets: sheets, tiles: summary.tiles, skipped: skipped, missing_textures: summary.missing_textures,
        }.pretty(2));
    } else {
        println!(
            "Packet {}: {} tile(s) on {} sheet(s) in {}{}",
            summary.packet_id,
            summary.tiles,
            summary.sheets.len(),
            opts.out_dir.display(),
            if summary.skipped.is_empty() { String::new() } else { format!(", {} skipped", summary.skipped.len()) }
        );
        println!("Review with {}; answer in a copy of {}", summary.packet_path.display(), summary.template_path.display());
    }
    Ok(())
}

fn run_annotate(args: &AnnotateArgs, db: &Path) -> Result<()> {
    use crate::catalog::annotate;
    let mut cat = Catalog::open_existing(db)?;
    let opts = annotate::AnnotateOptions {
        reviewer: args.reviewer.clone(),
        reviewer_name: args.reviewer_name.clone(),
        verify_files: args.verify_files,
    };
    let summary = annotate::import(&mut cat, &args.packet, &args.responses, &opts)?;
    if args.json {
        println!("{}", summary.json().pretty(2));
    } else {
        println!("{}", summary.line());
    }
    if args.strict && !summary.rejected.is_empty() {
        bail!("{} description(s) refused", summary.rejected.len());
    }
    Ok(())
}

fn run_pack(args: &PackArgs, db: &Path, keys: Option<&GtaKeys>) -> Result<()> {
    use crate::catalog::pack;
    match &args.command {
        PackCommand::Export { output, reviewer, since, include_shared, json } => {
            let cat = Catalog::open_existing(db)?;
            let opts = pack::ExportOptions { reviewer: reviewer.clone(), since: *since, include_shared: *include_shared };
            let summary = pack::export(&cat, output, &opts)?;
            if *json {
                println!("{}", json::object! { pack_id: summary.pack_id.clone(), count: summary.count, path: output.to_string_lossy().to_string() }.pretty(2));
            } else {
                println!("Wrote {} review(s) to {} ({})", summary.count, output.display(), summary.pack_id);
            }
        }
        PackCommand::Import { pack: file, strict, json } => {
            let mut cat = Catalog::open_existing(db)?;
            let summary = pack::import(&mut cat, keys, file)?;
            if *json {
                println!("{}", summary.json().pretty(2));
            } else {
                println!("{}", summary.line());
            }
            if *strict && !summary.invalid.is_empty() {
                bail!("{} invalid entr(ies) in {}", summary.invalid.len(), file.display());
            }
        }
    }
    Ok(())
}

#[cfg(feature = "semantic")]
fn run_embed(args: &EmbedArgs, db: &Path) -> Result<()> {
    use crate::catalog::embed;
    if args.check {
        println!("semantic search available ({})", embed::ENCODER_ID);
        return Ok(());
    }
    let mut cat = Catalog::open_existing(db)?;
    let summary = embed::embed_all(&mut cat, &embed::EmbedOptions { models: args.models, all: args.all, limit: args.limit, quiet: args.json })?;
    if args.json {
        println!("{}", json::object! {
            encoder: embed::ENCODER_ID, candidates: summary.candidates, embedded: summary.embedded,
            unchanged: summary.unchanged, elapsed_ms: summary.elapsed.as_millis() as u64,
        }.pretty(2));
    } else {
        println!(
            "Embedded {} item(s) ({} unchanged) of {} in {} with {}",
            summary.embedded, summary.unchanged, summary.candidates, human_duration(summary.elapsed), embed::ENCODER_ID
        );
    }
    Ok(())
}

#[cfg(not(feature = "semantic"))]
fn run_embed(args: &EmbedArgs, _db: &Path) -> Result<()> {
    let _ = args;
    bail!("this binary was built without the semantic feature (cargo install rage-cli --features semantic)")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n - 1).collect();
        t.push('~');
        t
    }
}

fn run_get(args: &GetArgs, db: &Path, keys: Option<&GtaKeys>) -> Result<()> {
    let cat = Catalog::open_existing(db)?;
    let item = search::find_item(&cat, &args.key)?;
    std::fs::create_dir_all(&args.output).with_context(|| format!("failed to create {}", args.output.display()))?;

    let (loc, file_name) = if args.rpf {
        let Some((last, chain)) = item.nested.split_last() else {
            bail!("'{}' is not inside a nested archive; open {} directly", item.key, item.archive_path);
        };
        let loc = EntryLoc { top_archive: PathBuf::from(&item.archive_path), nested_rpfs: chain.to_vec(), inner_path: last.clone() };
        (loc, base_name(last))
    } else {
        let loc = EntryLoc {
            top_archive: PathBuf::from(&item.archive_path),
            nested_rpfs: item.nested.clone(),
            inner_path: item.inner_path.clone(),
        };
        (loc, item.entry_name.clone())
    };
    let data = load_entry(&loc, keys).with_context(|| format!("failed to read '{}'", loc.inner_path))?;
    let out = args.output.join(safe_file_name(&file_name));
    std::fs::write(&out, &data).with_context(|| format!("failed to write {}", out.display()))?;
    println!("{}", out.display());

    if args.with_textures {
        let hit = search::hydrate(&cat, item.clone(), 0.0, "get")?;
        let mut stmt = cat.conn.prepare(
            "SELECT a.path, i.nested, i.inner_path, i.entry_name FROM items i JOIN archives a ON a.id = i.archive_id
             WHERE i.kind = 'txd' AND i.hash = ?1 AND i.winner = 1 LIMIT 1",
        )?;
        for (hash, name) in &hit.txd_chain {
            let row: Option<(String, String, String, String)> = rusqlite::OptionalExtension::optional(
                stmt.query_row([*hash as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))),
            )?;
            let Some((top, nested, inner, entry_name)) = row else {
                eprintln!("warning: texture dictionary {} (0x{hash:08X}) is not in the catalogue", name.as_deref().unwrap_or("?"));
                continue;
            };
            let loc = EntryLoc {
                top_archive: PathBuf::from(top),
                nested_rpfs: if nested.is_empty() { Vec::new() } else { nested.split("//").map(str::to_string).collect() },
                inner_path: inner,
            };
            let data = load_entry(&loc, keys)?;
            let out = args.output.join(safe_file_name(&entry_name));
            std::fs::write(&out, &data).with_context(|| format!("failed to write {}", out.display()))?;
            println!("{}", out.display());
        }
    }
    Ok(())
}

/// A file name safe to join onto an output directory: path separators and
/// anything outside `[A-Za-z0-9._-]` become `_`, and a leading dot is dropped.
fn safe_file_name(name: &str) -> String {
    let cleaned: String = base_name(name)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').to_string();
    if cleaned.is_empty() { "entry".to_string() } else { cleaned }
}

fn base_name(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

fn run_info(args: &InfoArgs, db: &Path, exe: Option<&Path>) -> Result<()> {
    let cat = Catalog::open_existing(db)?;
    let count = |sql: &str| -> Result<i64> { Ok(cat.conn.query_row(sql, [], |r| r.get(0))?) };
    let size = std::fs::metadata(db).map(|m| m.len()).unwrap_or(0)
        + std::fs::metadata(db.with_extension("sqlite-wal")).map(|m| m.len()).unwrap_or(0);
    let mut kinds = Vec::new();
    {
        let mut stmt = cat.conn.prepare("SELECT kind, count(*), sum(winner) FROM items GROUP BY kind ORDER BY kind")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
        for row in rows {
            kinds.push(row?);
        }
    }
    let mut statuses = Vec::new();
    {
        let mut stmt = cat.conn.prepare("SELECT status, count(*) FROM archives GROUP BY status ORDER BY status")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            statuses.push(row?);
        }
    }
    let annotations = count("SELECT count(*) FROM annotations WHERE superseded_by IS NULL")?;
    let annotated = count("SELECT count(DISTINCT item_id) FROM annotations WHERE superseded_by IS NULL AND method IN ('visual','shared-visual')")?;
    let packets = count("SELECT count(*) FROM packets")?;
    let embeddings = count("SELECT count(*) FROM embeddings")?;
    let archetypes = count("SELECT count(*) FROM archetypes")?;
    let failures = count("SELECT count(*) FROM items WHERE parse_error IS NOT NULL")?;
    let not_rsc7 = count("SELECT count(*) FROM items WHERE parse_error LIKE '%Not an RSC7%'")?;
    let meta = |k: &str| cat.meta(k).ok().flatten();
    let index_path = exe.and_then(|e| crate::keys::resolve_exe(e).ok()).and_then(|e| crate::index::GameIndex::cache_dir(&e)).map(|d| d.join(crate::index::Parts::TEXTURES.file_name()));

    if args.json {
        let mut kinds_json = json::JsonValue::new_object();
        for (k, n, w) in &kinds {
            kinds_json[k.as_str()] = json::object! { rows: *n, winners: *w };
        }
        let mut status_json = json::JsonValue::new_object();
        for (s, n) in &statuses {
            status_json[s.as_str()] = (*n).into();
        }
        let out = json::object! {
            db: db.to_string_lossy().to_string(),
            bytes: size,
            schema_version: meta("schema_version"),
            game_build: meta("game_build"),
            exe_version: meta("exe_version"),
            built_at: meta("built_at"),
            last_build_elapsed_ms: meta("last_build_elapsed_ms"),
            archives: status_json,
            items: kinds_json,
            archetypes: archetypes,
            parse_failures: failures,
            annotations: annotations,
            annotated_items: annotated,
            packets: packets,
            embeddings: embeddings,
            embed_encoder: meta("embed_encoder"),
            report: db.with_file_name("catalog-report.json").to_string_lossy().to_string(),
        };
        println!("{}", out.pretty(2));
        return Ok(());
    }

    println!("Catalogue: {} ({} MiB)", db.display(), size / (1 << 20));
    if let Some(v) = meta("exe_version") {
        println!("Game build: {v}");
    }
    if let (Some(at), Some(ms)) = (meta("built_at"), meta("last_build_elapsed_ms")) {
        let ms: u64 = ms.parse().unwrap_or(0);
        println!("Last build: {} (took {})", at, human_duration(std::time::Duration::from_millis(ms)));
    }
    println!("Archives: {}", statuses.iter().map(|(s, n)| format!("{n} {s}")).collect::<Vec<_>>().join(", "));
    for (k, n, w) in &kinds {
        println!("  {:<11} {:>10} rows, {:>10} loaded by the game", k, thousands(*n as u64), thousands(*w as u64));
    }
    println!("Archetypes: {}", thousands(archetypes as u64));
    println!("Annotations: {} ({} items described), packets: {}, embeddings: {}", annotations, annotated, packets, embeddings);
    if failures > 0 {
        println!("Parse failures: {failures} (see {})", db.with_file_name("catalog-report.json").display());
        if not_rsc7 * 2 > failures {
            println!("Most failures are not RSC7 resources: this looks like an Enhanced (Gen9) install, which rage cannot parse yet");
        }
    }
    if let Some(p) = index_path {
        println!("Texture index (separate, used by screenshot): {}{}", p.display(), if p.is_file() { "" } else { " (not built)" });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_cannot_escape_the_output_dir() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("levels/gta5/props.rpf"), "props.rpf");
        assert_eq!(safe_file_name(".."), "entry");
        assert_eq!(safe_file_name("a b.ydr"), "a_b.ydr");
    }
}
