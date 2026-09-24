// The `rage index` subcommand, hidden from the help: every command that
// needs the game index builds and caches the part it uses by itself (see
// `crate::index`). This is for rebuilding, inspecting or clearing it by hand.

use anyhow::{Context, Result};

use crate::index::{archives_fingerprint, GameIndex, Parts, Stale, LEGACY_FILE};
use crate::rpf::GtaKeys;

#[derive(clap::Args)]
pub struct IndexArgs {
    #[command(subcommand)]
    pub command: IndexCommand,
}

#[derive(clap::Subcommand)]
pub enum IndexCommand {
    /// Build every part of the index and write it to the cache (rebuilds
    /// even if the cache is current)
    Build,
    /// Show where the cache for this game build lives and whether each part
    /// is current
    Info,
    /// Delete the cached index for this game build
    Clear,
}

pub fn run(args: &IndexArgs, keys: Option<&GtaKeys>, exe: Option<&std::path::Path>) -> Result<()> {
    let exe = exe.context("--exe or GTAV_PATH is required for `rage index`")?;
    let exe_path = crate::keys::resolve_exe(exe)?;
    let game_root = exe_path.parent().context("--exe has no parent directory")?.to_path_buf();
    let dir = GameIndex::cache_dir(&exe_path).context("no cache directory available (no HOME/USERPROFILE?)")?;

    match &args.command {
        IndexCommand::Build => {
            println!("Indexing {}...", game_root.display());
            let fingerprint = archives_fingerprint(&game_root)?;
            let index = GameIndex::build(&game_root, keys, Parts::ALL)?;
            println!("{}", index.summary());
            index.save(&dir, fingerprint)?;
            println!("Wrote {}", dir.display());
            Ok(())
        }
        IndexCommand::Info => {
            println!("Cache: {}", dir.display());
            let fingerprint = archives_fingerprint(&game_root)?;
            for part in Parts::ALL.each() {
                let path = dir.join(part.file_name());
                let status = if !path.is_file() {
                    "not built (built on first use)".to_string()
                } else {
                    let size = std::fs::metadata(&path)?.len();
                    match GameIndex::load_part(&path, part, fingerprint) {
                        Ok(Ok(index)) => format!("{size} bytes, current: {}", index.summary()),
                        Ok(Err(Stale)) => format!("{size} bytes, stale (the game's archives changed; rebuilt on next use)"),
                        Err(_) => format!("{size} bytes, from another version of rage (rebuilt on next use)"),
                    }
                };
                println!("{:<10} {status}", part.name());
            }
            Ok(())
        }
        IndexCommand::Clear => {
            let mut removed = 0;
            let files = Parts::ALL.each().map(|p| p.file_name()).chain([LEGACY_FILE.to_string()]);
            for name in files {
                let path = dir.join(name);
                if path.is_file() {
                    std::fs::remove_file(&path)?;
                    println!("Removed {}", path.display());
                    removed += 1;
                }
            }
            if removed == 0 {
                println!("Nothing to remove in {}", dir.display());
            }
            Ok(())
        }
    }
}
