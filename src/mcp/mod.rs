//! `rage mcp`: the catalogue, renders and resource info as MCP tools over
//! stdio, for Claude Code, Cursor or any other MCP client.
//!
//! The transport owns stdout, and the commands wrapped here (and the
//! libraries under them) print to it. So before the first message the
//! process-level stdout is swapped for stderr and the original kept for the
//! protocol: every `println!` from then on lands on stderr, where the MCP
//! stdio transport allows free-form logging.

pub mod base64;
pub mod rpc;
pub mod tools;

use anyhow::{Context as _, Result};
use json::JsonValue;
use std::fs::File;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::rpf::GtaKeys;
use rpc::{Handler, RpcError};

const LATEST_PROTOCOL: &str = "2025-06-18";
const KNOWN_PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// What the client's model is told at `initialize`: the loop and the rules
/// from `AGENTS.md`, condensed.
pub const INSTRUCTIONS: &str = "\
rage: GTA V asset tools over the user's own game install. The catalogue indexes every \
drawable and texture by name, archive, size and any visual review notes; names are opaque \
(prop_..., hashes), so review notes are what make assets findable by what they look like.

Loop: catalog_info (is there a catalogue?) -> catalog_search (several phrasings; check \
coverage before trusting a negative) -> screenshot or catalog_sheet to see candidates -> \
describe what you see -> catalog_annotate -> search again (annotated: true narrows to \
reviewed items). catalog_get writes an entry out for other tools; resource_info reads a \
file's header and summary.

Rules: describe only what is visible in a render, never infer shape or material from a \
name, hash or path. Never mark an AI review as reviewer human. Never pad reviews to raise \
coverage; an empty description is correct when a tile cannot be judged. missing_textures > 0 \
means grey surfaces that are not grey in the game: say so in limitations. Check bounds.size \
before using an asset in a scene. Do not call catalog_reveal before the review is done.";

pub struct Server<'k> {
    pub db: Option<PathBuf>,
    pub output: PathBuf,
    pub keys: Option<&'k GtaKeys>,
    pub exe: Option<PathBuf>,
    initialized: bool,
}

impl<'k> Server<'k> {
    pub fn new(db: Option<PathBuf>, output: PathBuf, keys: Option<&'k GtaKeys>, exe: Option<PathBuf>) -> Self {
        Self { db, output, keys, exe, initialized: false }
    }

    fn context(&self) -> tools::Context<'_> {
        tools::Context { db: self.db.as_deref(), output: &self.output, keys: self.keys, exe: self.exe.as_deref() }
    }

    fn initialize(&mut self, params: &JsonValue) -> JsonValue {
        let requested = params["protocolVersion"].as_str().unwrap_or(LATEST_PROTOCOL);
        let version = if KNOWN_PROTOCOLS.contains(&requested) { requested } else { LATEST_PROTOCOL };
        self.initialized = true;
        json::object! {
            protocolVersion: version,
            capabilities: json::object! { tools: json::object! { listChanged: false } },
            serverInfo: json::object! { name: "rage", version: env!("CARGO_PKG_VERSION") },
            instructions: INSTRUCTIONS,
        }
    }
}

impl Handler for Server<'_> {
    fn request(&mut self, method: &str, params: &JsonValue) -> Result<JsonValue, RpcError> {
        match method {
            "initialize" => Ok(self.initialize(params)),
            "ping" => Ok(JsonValue::new_object()),
            "tools/list" => Ok(json::object! { tools: tools::TOOLS.iter().map(tools::Tool::json).collect::<Vec<_>>() }),
            "tools/call" => {
                let name = params["name"].as_str().ok_or_else(|| RpcError::invalid_params("'name' is required"))?;
                let args = &params["arguments"];
                let ctx = self.context();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tools::call(&ctx, name, args)));
                match outcome {
                    Ok(result) => result.map(|r| r.json()),
                    Err(panic) => {
                        let what = panic
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_else(|| "panic".to_string());
                        Err(RpcError::new(rpc::INTERNAL_ERROR, format!("{name} panicked: {what}")))
                    }
                }
            }
            "resources/list" => Ok(json::object! { resources: json::array![] }),
            "prompts/list" => Ok(json::object! { prompts: json::array![] }),
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    fn notification(&mut self, method: &str, _params: &JsonValue) {
        log::debug!("notification {method}");
    }
}

/// Serves until stdin closes. Replies go to the handle stdout had when
/// the server started; the process's stdout is stderr from here on.
pub fn serve(server: &mut Server) -> Result<()> {
    std::fs::create_dir_all(&server.output).with_context(|| format!("failed to create {}", server.output.display()))?;
    let mut protocol = take_stdout().context("failed to take over stdout for the protocol")?;
    eprintln!(
        "rage mcp {}: {} tool(s); catalogue {}; output {}",
        env!("CARGO_PKG_VERSION"),
        tools::TOOLS.len(),
        server.db.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "resolved from the game build".to_string()),
        server.output.display()
    );
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        let n = stdin.lock().read_line(&mut line).context("failed to read stdin")?;
        if n == 0 {
            return Ok(());
        }
        if let Some(reply) = rpc::handle_line(&line, server) {
            let mut text = reply.dump();
            text.push('\n');
            protocol.write_all(text.as_bytes()).context("failed to write to stdout")?;
            protocol.flush()?;
        }
    }
}

/// The original stdout as a file, with the process's stdout pointed at
/// stderr from now on.
#[cfg(unix)]
fn take_stdout() -> std::io::Result<File> {
    use std::os::unix::io::FromRawFd;
    // SAFETY: dup/dup2 on the standard descriptors; the new descriptor is
    // owned by the File returned and nothing else refers to it.
    unsafe {
        let fd = libc::dup(1);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::dup2(2, 1) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(File::from_raw_fd(fd))
    }
}

#[cfg(windows)]
fn take_stdout() -> std::io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    // SAFETY: the handles come from the console API; the stdout handle is
    // handed to exactly one File, and Rust's stdout re-reads the standard
    // handle on each write, so it follows the SetStdHandle.
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        if out == INVALID_HANDLE_VALUE || out == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let err = GetStdHandle(STD_ERROR_HANDLE);
        if err != INVALID_HANDLE_VALUE && err != 0 && SetStdHandle(STD_OUTPUT_HANDLE, err) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(File::from_raw_handle(out as _))
    }
}

/// The default `--output`: `~/.rage-cli/mcp`, else the system temp dir.
pub fn default_output() -> PathBuf {
    crate::paths::config_root().map(|r| r.join("mcp")).unwrap_or_else(|| std::env::temp_dir().join("rage-mcp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server<'static> {
        Server::new(Some(PathBuf::from("/nonexistent/catalog.sqlite")), PathBuf::from("."), None, None)
    }

    #[test]
    fn initialize_negotiates_a_known_version() {
        let mut s = server();
        let r = s.request("initialize", &json::object! { protocolVersion: "2025-03-26" }).unwrap();
        assert_eq!(r["protocolVersion"], "2025-03-26");
        assert_eq!(r["serverInfo"]["name"], "rage");
        assert!(r["instructions"].as_str().unwrap().contains("Never mark an AI review"));
        let r = s.request("initialize", &json::object! { protocolVersion: "1999-01-01" }).unwrap();
        assert_eq!(r["protocolVersion"], LATEST_PROTOCOL);
    }

    #[test]
    fn tools_list_names_every_tool() {
        let mut s = server();
        let r = s.request("tools/list", &JsonValue::Null).unwrap();
        let names: Vec<&str> = r["tools"].members().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["catalog_search", "catalog_get", "catalog_info", "catalog_sheet", "catalog_reveal", "catalog_annotate", "screenshot", "resource_info"]);
    }

    #[test]
    fn a_missing_catalogue_is_a_tool_error_not_a_protocol_error() {
        let mut s = server();
        let r = s.request("tools/call", &json::object! { name: "catalog_info", arguments: {} }).unwrap();
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("catalog"));
        let e = s.request("tools/call", &json::object! { name: "nope", arguments: {} }).unwrap_err();
        assert_eq!(e.code, rpc::INVALID_PARAMS);
        let e = s.request("tools/call", &json::object! { arguments: {} }).unwrap_err();
        assert_eq!(e.code, rpc::INVALID_PARAMS);
        assert_eq!(s.request("resources/list", &JsonValue::Null).unwrap()["resources"].len(), 0);
        assert_eq!(s.request("nope", &JsonValue::Null).unwrap_err().code, rpc::METHOD_NOT_FOUND);
        assert!(s.request("ping", &JsonValue::Null).unwrap().is_object());
    }
}
