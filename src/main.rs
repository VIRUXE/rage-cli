use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

mod rpf;
mod commands;
mod navmesh;
mod index;
mod keys;
mod paths;
mod resources;
mod update;
mod utils;

use commands::{info, list, extract, verify, tree, textures, screenshot, create, search, resource, index_cmd, navmesh as navmesh_cmd};
use commands::update as update_cmd;
use rpf::GtaKeys;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "rage")]
#[command(about = "A CLI for RAGE game files: RPF archives, RSC7 resources, textures, renders and navmeshes", long_about = None)]
struct Cli {
    /// Enable verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// GTA5.exe (or the folder holding it) to read the keys from; the keys are
    /// cached per game build under ~/.rage-cli/keys (RAGE_KEYS_CACHE overrides)
    #[arg(long, global = true, value_name = "PATH", env = "GTAV_PATH")]
    exe: Option<PathBuf>,

    /// Directory with keys already written out by `extract-keys`; takes
    /// precedence over --exe
    #[arg(long, global = true, value_name = "DIR")]
    keys: Option<PathBuf>,

    /// Skip the daily background check for a newer release
    /// (also RAGE_NO_UPDATE_CHECK)
    #[arg(long, global = true)]
    no_update_check: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Display information about an RPF archive
    Info {
        /// Path to the RPF archive
        archive: PathBuf,
    },

    /// List files in an RPF archive
    List {
        /// Path to the RPF archive
        archive: PathBuf,

        /// Pattern to filter files (e.g., "*.xml")
        pattern: Option<String>,

        /// Show detailed information
        #[arg(short, long)]
        detailed: bool,
    },

    /// Extract files from an RPF archive
    Extract {
        /// Path to the RPF archive
        archive: PathBuf,

        /// Output directory (defaults to archive name without extension)
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,

        /// Specific file or pattern to extract
        pattern: Option<String>,

        /// Recurse into nested RPF archives, extracting them to loose files
        /// (resource files get a valid RSC7 header, like CodeWalker)
        #[arg(short, long)]
        recursive: bool,
    },

    /// Find files by name, contents or hash, descending into nested archives without extracting
    Search(search::SearchArgs),

    /// Verify integrity of an RPF archive
    Verify {
        /// Path to the RPF archive
        archive: PathBuf,
    },

    /// Display archive contents in tree format
    Tree {
        /// Path to the RPF archive
        archive: PathBuf,

        /// Maximum depth to display
        #[arg(short, long)]
        depth: Option<usize>,
    },

    /// Export textures from a .ytd/.ydr/.ydd/.yft as PNG/JPG/WebP (or DDS with --dds)
    #[command(alias = "ytd")]
    Textures(textures::TexturesArgs),

    /// Render a .ydr/.ydd/.yft to an image
    Screenshot(screenshot::ScreenshotArgs),

    /// Inspect loose resource files (.ydr/.ytd/...) or entries inside an archive
    Resource(resource::ResourceArgs),

    /// Inspect, fetch, export and build navmesh cells (.ynv)
    Navmesh(navmesh_cmd::NavmeshArgs),

    /// Check for a newer release, or update this binary in place
    Update(update_cmd::UpdateArgs),

    /// Create an RPF archive from a directory
    Create {
        /// Directory to pack
        input: PathBuf,

        /// Output RPF file path
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,

        /// RPF version to create (0, 2, 3, 4, 6, 7)
        #[arg(long, default_value = "7")]
        version: u8,

        /// Encryption mode (none, open, ng)
        #[arg(short, long, default_value = "none")]
        encryption: String,
    },

    /// Build, inspect, or clear the cached game-wide texture index that
    /// `screenshot` uses to resolve external texture dictionaries
    Index(index_cmd::IndexArgs),

    /// Write the keys out to disk for reuse with --keys
    ExtractKeys {
        /// Directory to save extracted keys into
        #[arg(short, long, value_name = "DIR")]
        output: PathBuf,
    },
}

fn load_keys(exe: Option<&Path>, keys_dir: Option<&Path>) -> Result<Option<GtaKeys>> {
    // Keys win over the executable, so an explicit --keys still works when
    // GTAV_PATH is set in the environment.
    match (keys_dir, exe) {
        (Some(dir), _)  => Ok(Some(GtaKeys::load_from_path(dir)?)),
        (_, Some(exe))  => Ok(Some(keys::from_exe_cached_default(&keys::resolve_exe(exe)?)?)),
        (None, None)    => Ok(None),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(if cli.verbose { "debug" } else { "info" })
    ).init();

    // Spawned before the command runs but only reported after it succeeds,
    // so a real command never waits on the network and the note prints last.
    let check = update::spawn_background_check(cli.no_update_check);

    let keys = if matches!(cli.command, Commands::Update(_)) {
        None
    } else {
        load_keys(cli.exe.as_deref(), cli.keys.as_deref())?
    };

    let result = dispatch(cli.command, keys.as_ref(), cli.exe.as_deref(), cli.verbose);

    if result.is_ok() {
        update::report_background_check(check);
    }

    result
}

fn dispatch(command: Commands, keys: Option<&GtaKeys>, exe: Option<&Path>, verbose: bool) -> Result<()> {
    match command {
        Commands::Info        { archive }                    => info::run(&archive, keys),
        Commands::List        { archive, pattern, detailed } => list::run(&archive, pattern.as_deref(), detailed, keys),
        Commands::Extract     { archive, output, pattern, recursive } => extract::run(&archive, output.as_deref(), pattern.as_deref(), recursive, keys),
        Commands::Search(args)                               => search::run(&args, keys),
        Commands::Verify      { archive }                    => verify::run(&archive, keys),
        Commands::Tree        { archive, depth }             => tree::run(&archive, depth, keys),
        Commands::Textures(args)                             => textures::run(&args, keys),
        Commands::Screenshot(args)                           => screenshot::run(&args, keys, exe),
        Commands::Resource(args)                             => resource::run(&args, keys, verbose),
        Commands::Navmesh(args)                              => navmesh_cmd::run(&args, keys, exe),
        Commands::Update(args)                               => update_cmd::run(&args),
        Commands::Index(args)                                => index_cmd::run(&args, keys, exe),
        Commands::Create { input, output, version, encryption } => {
            create::run(&input, &output, version, &encryption, keys)
        }
        Commands::ExtractKeys { output }                     => {
            let exe = exe.context("--exe is required to extract keys")?;
            keys::extract(&keys::resolve_exe(exe)?, &output)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Ensures clap's own invariants hold for the CLI definition (e.g. no
    /// duplicate short flags within a subcommand). This catches regressions
    /// like `-v` being claimed by both the global `--verbose` and a
    /// subcommand-local argument before they can panic at runtime.
    #[test]
    fn cli_debug_assert() {
        Cli::command().debug_assert();
    }
}
