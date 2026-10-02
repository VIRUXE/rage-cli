# `rage mcp`: the catalogue, renders and resource info as MCP tools

Closes VIRUXE/rage-cli#31, the follow-up #26 left open. An MCP client
(Claude Code, Cursor, a custom agent) gets the operations `AGENTS.md`
describes as typed tools with JSON results, in the way fivem-mcp exposes a
running FiveM server, instead of spelling out and parsing shell commands.

## What the user gets

```sh
rage mcp                                    # stdio server, keys from GTAV_PATH
rage mcp --db ~/.rage-cli/catalog/3889/catalog.sqlite --output ~/rage-out
```

Claude Code registration, for example:

```sh
claude mcp add rage -- rage mcp
```

Eight tools, each a thin wrapper over an existing command, with the same
JSON the command's `--json` prints as `structuredContent` and, pretty
printed, as the text content:

| Tool | Wraps | Arguments |
|---|---|---|
| `catalog_search` | `catalog search --json` | `query`, `kind[]`, `dlc[]`, `min_size`, `max_size`, `annotated`, `all_copies`, `limit`, `mode` (lexical, semantic, hybrid), `raw` |
| `catalog_get` | `catalog get` | `key`, `output_dir`, `rpf`, `with_textures` → the files written |
| `catalog_info` | `catalog info --json` | none |
| `catalog_sheet` | `catalog sheet --json` | `query` or `keys[]`, the search filters, `output_dir`, `views[]`, `facing`, `per_sheet`, `cell`, `limit`, `inline_images` |
| `catalog_reveal` | `catalog sheet --reveal --json` | `packet_id` |
| `catalog_annotate` | `catalog annotate --json` | `packet` (path), `responses` (path) or `responses_json` (the object itself), `reviewer`, `reviewer_name`, `verify_files` |
| `screenshot` | `screenshot --json` | `key` (a catalogue key) or `archive` + `file`, or `ped` + `components[]`, or `vehicle` + `hi`, `livery`, `colour_from`; `views[]`, `facing`, `size`, `lod`, `format`, `background`, `grid`, `entry`, `output_dir`, `inline_images` |
| `resource_info` | `resource info --json` | `file`, `archive`, `limit`, `names[]` |

- `screenshot` with `key` resolves the asset through the catalogue: a direct
  entry opens its archive; one inside a nested archive has that archive
  written to `output_dir/rpf/` first, as the `get --rpf` follow-up in
  `catalog search --json` would, then renders from it. The catalogue's
  texture chain is already what `screenshot` uses through the index.
- `inline_images` (default false) adds each written PNG/JPEG/WebP as an
  MCP `image` content item (base64), so a vision-capable client can review a
  contact sheet or a render without a separate file read. Paths are always
  returned.
- `catalog_annotate` with `responses_json` writes the object to
  `responses-<unix time>.json` beside `packet.json` and imports that file,
  so the review on disk matches what the catalogue recorded.
- `output_dir` defaults to `--output`, which defaults to
  `~/.rage-cli/mcp/` (the same per-user root as keys and the catalogue).
  Every path in a result is absolute.
- The server's `initialize` result carries `instructions`: the rules from
  `AGENTS.md` (describe only what is visible, never mark an AI review as
  human, coverage before trusting a negative, `missing_textures` means grey
  is not a colour) and the loop build → search → sheet → review → annotate →
  search again. The client's model sees them without reading the repo.

## Protocol

MCP over stdio: newline-delimited JSON-RPC 2.0 on stdin/stdout, as the
MCP specification's stdio transport defines. Written by hand on the `json`
crate the rest of the CLI uses; no async runtime, no serde, no SDK. The
methods a tools-only server needs:

- `initialize` → `protocolVersion` (the client's, when it is one of
  `2024-11-05`, `2025-03-26`, `2025-06-18`; else `2025-06-18`),
  `capabilities: { tools: {} }`, `serverInfo { name: "rage", version }`,
  `instructions`.
- `notifications/initialized`, `notifications/cancelled` → ignored
  (notifications get no reply).
- `ping` → `{}`.
- `tools/list` → the eight tools with `name`, `description`, `inputSchema`
  (JSON Schema objects written by hand, with `required` and `enum` where the
  CLI has them).
- `tools/call` → `{ content: [{type: "text", text}], structuredContent,
  isError }`. A tool that fails (no catalogue, unknown key, render error)
  returns `isError: true` with the error chain as text, never a JSON-RPC
  error; the JSON-RPC errors are reserved for a malformed request (`-32700`),
  an unknown method (`-32601`), an unknown tool or bad argument types
  (`-32602`) and a panic caught around a call (`-32603`).
- A JSON array on a line is treated as a batch (older clients send them):
  each element is answered and the replies are sent as one array.
- Any other request method → `-32601`.

Requests are served one at a time, in order, on the main thread: the
operations hold the catalogue's SQLite lock and the render pipeline is not
shared across threads here either.

## Keeping stdout clean

The transport owns stdout. The commands being wrapped print to stdout
(`screenshot` says which dictionary it used, `catalog get` prints the paths
it wrote) and so do the libraries underneath on occasion; one stray line
corrupts the stream. Two measures:

1. Before the first message, the process-level stdout is swapped for
   stderr: the original handle is duplicated and kept for the protocol
   writer, then `dup2(2, 1)` on Unix and `SetStdHandle(STD_OUTPUT_HANDLE,
   stderr)` on Windows. Every `println!` anywhere in the process from then on
   lands on stderr, where the MCP transport allows free-form logging. Rust's
   standard streams read the handle on each write, so the swap takes effect
   for code already compiled against `std::io::stdout`. This needs `libc`
   on Unix and `windows-sys` on Windows, both already in the dependency
   tree.
2. The wrapped commands get value-returning cores so the server does not
   depend on parsing its own captured text: `commands/catalog.rs` splits
   each `run_*` into a function that returns the JSON object (`search_json`,
   `info_json`, `sheet_json`, `reveal_json`, `get_files`) and a printing
   shell; `screenshot` gains `--json` (one object: `images[]` with view and
   path, per entry the `missing_textures`, `warnings[]`), and its chatter is
   suppressed in JSON mode; `resource info` already writes its JSON to a
   `String`, which `info_json` returns.

## Files

```
src/mcp/
  mod.rs        the server loop: stdout swap, line reader, dispatch, batch handling
  rpc.rs        JSON-RPC 2.0 framing: parse a message, build result/error replies
  tools.rs      the tool table: schemas, argument decoding, the call into each command
  base64.rs     the 20-line encoder for inline images
src/commands/mcp.rs   clap args (--db, --output, --no-stdout-swap for tests) and `run`
src/commands/catalog.rs      JSON cores split from the printing
src/commands/screenshot.rs   --json and the report struct
src/commands/resource.rs     info_json
tests/mcp.rs  spawns `rage mcp` against the catalogue fixture from tests/catalog.rs
README.md     a `### MCP server` section under Commands and a recipe under Agents
AGENTS.md     a paragraph: the same loop through the MCP tools
```

## Testing

- Unit tests in `rpc.rs` for framing: a request, a notification, a batch, a
  parse error, an unknown method, an id echoed back as number or string.
- Unit tests in `tools.rs` for argument decoding: defaults, `enum` rejection,
  a size spec, a views list with an `AZIMUTH:ELEVATION` entry.
- `tests/mcp.rs`: build the hand-made install from `tests/catalog.rs` into
  a temporary catalogue, start `rage mcp --db`, and over its stdin/stdout:
  `initialize` → version and instructions; `tools/list` → eight names;
  `catalog_search {query: "prop test"}` → the fixture's drawable with
  `commands`; `catalog_info`; `catalog_get` of the nested entry with
  `rpf: true` → a file exists; `catalog_sheet` on the drawable with
  `inline_images: true` → one `image` content item whose base64 decodes to
  a PNG header; `catalog_reveal` of that packet; `catalog_annotate` with a
  `responses_json` built from the packet's hashes → `imported: 1`, then
  `catalog_search` finds the description; `screenshot {key}` → an image
  path; `resource_info` of the extracted file; an unknown tool → `-32602`;
  a wrong `kind` → `isError`. One stray `println!` from a tool (a test-only
  tool behind `cfg(test)` is not possible across the binary boundary, so
  the test asserts `catalog_get`, which prints its paths in the CLI, leaves
  stdout parseable).
- `cargo test` stays game-free; nothing here needs `GTAV_PATH`.

## Not in this change

- Resources and prompts (`resources/list`, `prompts/list`): `instructions`
  carries what an agent needs; a `rage://` resource for `AGENTS.md` can come
  when a client asks for it.
- HTTP or SSE transport. Stdio is what every MCP client speaks and what the
  issue asks for.
- `catalog build`, `pack`, `embed` as tools: they run for minutes or write
  shared files, and an agent can call the CLI for them. `catalog_info`
  tells the client whether a build exists.
- Progress notifications during `catalog_sheet` and `screenshot`.
