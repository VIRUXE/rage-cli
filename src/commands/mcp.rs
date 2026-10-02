//! `rage mcp`: serve the catalogue, renders and resource info to an MCP
//! client over stdio. The server lives in `crate::mcp`.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::mcp::{self, Server};
use crate::rpf::GtaKeys;

#[derive(clap::Args)]
pub struct McpArgs {
    /// The catalogue database (default ~/.rage-cli/catalog/<game build>/catalog.sqlite)
    #[arg(long, value_name = "FILE", env = "RAGE_CATALOG")]
    pub db: Option<PathBuf>,

    /// Where tools write files when a call names no output_dir
    /// (default ~/.rage-cli/mcp)
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,
}

pub fn run(args: &McpArgs, keys: Option<&GtaKeys>, exe: Option<&Path>) -> Result<()> {
    let output = args.output.clone().unwrap_or_else(mcp::default_output);
    let mut server = Server::new(args.db.clone(), output, keys, exe.map(Path::to_path_buf));
    mcp::serve(&mut server)
}
