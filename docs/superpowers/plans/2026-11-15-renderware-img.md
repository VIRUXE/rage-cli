# RenderWare and IMG Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Inspect, export, edit, rebuild, render and convert PC TXD/DFF/IPL assets and handle IMG archives using our own implementations.

**Architecture:** Extend rpf-archive for containers and rage-formats for independently written codecs and conversion. Adapt their data to rage-render, and expose the workflows through existing CLI commands and MCP.

**Tech Stack:** Rust, existing anyhow/json/clap/image/BC codecs, existing typed RAGE writers, PowerShell development wrapper.

**Spec:** `docs/superpowers/specs/2026-11-15-renderware-img-design.md`

## Global Constraints

- Implement binary readers and writers independently from format descriptions and observed files.
- Do not copy, translate, adapt or vendor external parser code.
- Read `D:/Games/gtasa` only; write private generated fixtures under ignored `target/`.
- Target classic PC III/VC/SA and legacy GTA V; identify unsupported encodings explicitly.
- Preserve opaque data during unchanged rebuilds; reject edits that invalidate unmodeled dependent data.
- Preserve existing RPF/RSC7 commands and defaults; no publication or public release is implicit.
- Use temporary local Cargo patches resolving one rage-formats instance across the toolchain.

## Review Focus

- IMG names with garbage after the first NUL, duplicate names and 24-byte limits: Task 1.
- Valid opaque plugins whose indices become stale after a semantic edit: Task 3/5.
- Palette nibble order, alpha, compressed mip tails and D3D8/9 differences: Task 4.
- Streamed IPL LOD indices, inverse quaternions and interior partitions: Task 7/9.
- A batch with one failed dependency must not report complete conversion success: Task 10.

## File map and execution

Paths prefixed `../` refer to the existing sibling repositories. Implement inline
in this session; the design is already approved. Record task completion and any
interface rulings in this plan's ignored progress ledger. Each task below uses
the same sequence: write its named tests, run and observe the expected failure,
implement its listed interface, run the whole relevant suite, then commit the
task and record the result. Do not count an inventory scan as a parser test.

### Task 1: IMG commands and checked archive rebuilding

**Files:** modify `src/rpf.rs`, `src/resources.rs`, `src/main.rs`,
`src/commands/{info,create,verify,extract}.rs`; create
`src/commands/archive_rebuild.rs`, `tests/img.rs`; modify
`../rpf-archive-rs/src/{archive,writer}.rs` if validation primitives need fixes.

**Interfaces:** `Archive::open_with_dir(path: &Path, dir: Option<&Path>, keys:
Option<&GtaKeys>) -> Result<Self>`, `Archive::version() -> RpfVersion`;
`load_entry_bytes(file: &str, archive: Option<&Path>, keys: Option<&GtaKeys>)
-> Result<Vec<u8>>`. Keep the existing resource-only loader for RSC7 callers.
Add `create --format rpf|img1|img2|img3`, shared global `--img-dir`, and
`archive rebuild INPUT -o OUTPUT [--add FILE] [--replace FILE] [--remove NAME]`.

- [ ] Write tests `img1_pair_opens`, `img2_create_extract`, `img3_create_extract`,
  `rebuild_preserves_unmodified`, `names_stop_at_first_nul`,
  `rejects_truncating_duplicate_names`, `verify_invalid_archive_fails` and
  `extract_cannot_escape_output`. Assert payload equality (including sector
  padding), selected version and nonzero failure exit status.
- [ ] Run `cargo test --test img`; expected new command/IMG1 behavior fails.
- [ ] Implement signatures above using existing IMG library APIs. Preserve
  unchanged payloads; validate all destinations before writes. Propagate errors.
- [ ] Run `cargo test`; expected no failures. Read-only real check: list/info and
  extraction of a selected DFF/TXD/IPL from GTASA gta3.img under target/.
- [ ] Commit `feat: expose IMG archive workflows` and record test evidence.

### Task 2: Reproducible local toolchain

**Files:** create `scripts/with-local-toolchain.ps1`,
`docs/local-toolchain.md`; modify development setup only, no committed absolute
paths. **Interface:** `with-local-toolchain.ps1 [-ToolchainRoot PATH] [cargo args]`
uses temporary Cargo configuration for local rpf-archive/rage-formats/rage-render
and the renderer git source, forwarding the cargo exit code.

- [ ] Add a wrapper smoke check verifying `cargo tree -i rage-formats` identifies
  exactly the local package and a failing cargo command returns failure.
- [ ] Run it before adding overrides; expected dependency-origin check fails.
- [ ] Implement wrapper and exact documented invocations, with try/finally
  cleanup and safe argument forwarding.
- [ ] Run wrapper smoke check plus `cargo test` through the wrapper; expected
  local origins and green suite. Use wrapper for subsequent CLI tests.
- [ ] Commit `build: support reproducible local toolchain testing`.

### Task 3: Checked RenderWare chunks and editable document contract

**Files:** create `../rage-formats/src/renderware/{mod,chunk,document}.rs`,
`../rage-formats/tests/renderware.rs`; export module from `src/lib.rs` there.
Create `src/commands/resource_renderware.rs`; integrate in `resource.rs`.

**Interfaces:** `Chunk { kind: u32, version: u32, payload: Vec<u8> }`,
`parse_chunks(data: &[u8]) -> Result<Vec<Chunk>>`,
`write_chunks(chunks: &[Chunk]) -> Result<Vec<u8>>`;
`Document::parse(data: &[u8], format: Format) -> Result<Document>`,
`Document::to_json() -> Result<String>`, `Document::from_json(text: &str)
-> Result<Document>`, `Document::build() -> Result<Vec<u8>>`.
`Format` is `Txd|Dff|Ipl`; documents carry schema `rage-renderware/1`, source
layout, semantic edits and opaque chunks. Companion payloads use checked paths.

- [ ] Add `unknown_chunk_roundtrip`, `rejects_truncated_and_overflowing_chunk`,
  `rejects_excessive_nesting`, `document_schema_is_checked`,
  `opaque_plugin_edit_rejected`; assert exact original bytes/no panic and errors
  naming offset/plugin, not silently skipped input.
- [ ] Run formats tests; expected missing module/API failure.
- [ ] Implement bounded parsing, header/version retention, explicit containers,
  strict document validation and chunk serialization. Wire non-RSC7 dispatch.
- [ ] Run formats suite and CLI resource suite; expected all green.
- [ ] Commit `feat: add checked RenderWare documents` in affected repositories.

### Task 4: TXD codecs, texture editing and YTD conversion

**Files:** create `../rage-formats/src/renderware/{txd,raster}.rs`,
`../rage-formats/tests/txd.rs`; modify texture/document adapters and
`src/commands/{textures,resource_renderware,resource_convert}.rs`;
create `tests/txd.rs`.

**Interfaces:** `Txd::parse(data: &[u8]) -> Result<Txd>`, `Txd::build()
-> Result<Vec<u8>>`, `Txd::textures() -> Result<Vec<YtdTexture>>`,
`Txd::replace(texture: YtdTexture) -> Result<()>`, `Txd::to_ytd()
-> Result<Vec<u8>>`, `Txd::from_textures(textures: Vec<YtdTexture>,
game: Game) -> Result<Txd>`. `Game` identifies III/VC/SA PC explicitly.

- [ ] Fixtures assert known RGBA values for 565/1555/4444/888/8888, luminance,
  PAL4/PAL8 and BC1/2/3, D3D8/9 headers, alpha and every mip. Check replacing
  one texture retains unknown chunks and other encoded bytes, and building a
  dictionary with case-insensitive name collisions fails.
- [ ] Run formats TXD tests and `cargo test --test txd`; expected missing support.
- [ ] Implement codec and document edits; extend texture export/build/from,
  including explicit source game for new dictionaries, and YTD conversion.
- [ ] Run both full suites. Scan all real GTA3 TXDs, decode all mip levels and
  compare no-op rebuilds. View sheets of representative textures and record
  failures with format/platform details. Reparse produced YTDs.
- [ ] Commit `feat: read write edit and convert PC TXD textures`.

### Task 5: DFF codecs and editable geometry/rigs

**Files:** create `../rage-formats/src/renderware/{dff,geometry,plugins}.rs`,
`../rage-formats/tests/dff.rs`; extend document/resource adapters and
`tests/dff.rs`.

**Interfaces:** `Dff::parse(data: &[u8]) -> Result<Dff>`, `Dff::build()
-> Result<Vec<u8>>`, `Dff::drawables() -> Result<Vec<DrawableEntry>>`.
Typed scene holds local frames/parents, atomics, shared materials, geometries,
skin/HAnim, effects, night colors and opaque plugins; edits retain stable IDs.

- [ ] Test translated/rotated parent frames, shared geometry/materials, winding,
  multiple UVs, colors, skin weights and HAnim. Assert no-op opaque retention,
  valid regenerated bin meshes after topology edits and refusal when unknown
  dependent plugins cannot be updated. Native geometry errors identify layout.
- [ ] Run formats DFF tests and CLI dump/build tests; expected missing support.
- [ ] Implement independent readers/writers, plugin modeling and dependency
  invalidation, editable documents and render-data adapter.
- [ ] Run full suites; scan real DFFs and round-trip representative props,
  vehicles and characters without losing hierarchy/encoded plugin state.
- [ ] Commit `feat: read write and edit PC DFF models`.

### Task 6: DFF rendering, structured GLB export and YDR conversion

**Files:** create `../rage-formats/src/renderware/to_rage.rs`;
modify `../rage-render/src/render/{mod,gltf}.rs` and public exports;
create `../rage-render/tests/rig_export.rs`; modify
`src/commands/{screenshot,resource_convert}.rs`, `src/resources.rs`.

**Interfaces:** `Dff::to_ydr(options: &ConversionOptions) -> Result<Converted>`;
`ConversionOptions` carries explicit allow-loss policy; `Converted` contains
bytes plus structured losses. `export_scene_glb(scene: &RigScene) -> Result<Vec<u8>>`
preserves nodes, skeleton, inverse bind matrices, JOINTS/WEIGHTS and materials.
`RigScene` is exported by the formats adapter; rasterizer inputs stay separate.

- [ ] Assert known source/target vertex positions and material/texture hashes,
  valid weight sums, bind-pose transforms and GLB accessor/node relationships.
  Assert unsupported semantic loss fails without an explicit allowed loss.
- [ ] Run formats conversion/render tests; expected missing conversion/export.
- [ ] Implement typed RAGE blocks and structured GLB export, TXD resolution and
  DFF screenshot paths. Preserve all existing screenshot options where meaningful.
- [ ] Run full suites; visually compare source/converted static, transparent and
  skinned samples. Reopen YDR with both existing readers and CodeWalker tooling.
- [ ] Commit `feat: render export and convert DFF assets`.

### Task 7: IPL/IDE codecs and source scene resolution

**Files:** create `../rage-formats/src/renderware/{ipl,ide,game_data}.rs`,
`../rage-formats/tests/ipl.rs`; create `src/legacy_game.rs` and CLI IPL tests.

**Interfaces:** `Ipl::parse(data: &[u8], game: Game) -> Result<Ipl>`,
`Ipl::build() -> Result<Vec<u8>>`; `Ide::parse(text: &str, game: Game)
-> Result<Ide>`; `LegacyGame::open(root: &Path, game: Game) -> Result<Self>`;
`LegacyGame::resolve_scene(input: &Path) -> Result<ResolvedScene>`.
The resolver loads gta.dat/IDEs/archives/TXD parent chains deterministically.

- [ ] Test III/VC/SA text fields, SA binary instances/car generators, comments,
  unknown sections/reserved bytes, finite rotations/scales, missing model IDs,
  duplicate definitions, cross-stream LOD scope/cycles and text line errors.
  Quaternion test maps a known local point to the expected world point.
- [ ] Run codecs/resolver tests; expected missing API.
- [ ] Implement parsing/writing and documents; resolve streamed companions with
  explicit index scope and preserve source metadata. Never skip malformed records.
- [ ] Run full suites; read gta.dat and every local IDE/IPL/streamed IPL; edit one
  placement and car generator, rebuild/reparse, and check the intended values.
- [ ] Commit `feat: edit IPL placements and resolve classic GTA scenes`.

### Task 8: COL collision and YBN conversion

**Files:** create `../rage-formats/src/renderware/{col,collision}.rs`,
`../rage-formats/tests/col.rs`; extend `src/legacy_game.rs`.

**Interfaces:** `ColFile::parse(data: &[u8]) -> Result<ColFile>`;
`ColFile::build() -> Result<Vec<u8>>`, plus editable document info/dump/build
integration, preserving unknown records during no-op round trips;
`ColModel::to_ybn(options: &ConversionOptions) -> Result<Converted>`.
Parse required PC COL/COLL/COL2/COL3 layouts independently, retain source
metadata and associate models through IDE identity/name.

- [ ] Test sphere/box/triangle bounds, compressed coordinates, face materials,
  concatenated models and bad offsets; compare resulting YBN geometry/materials
  and require a separate error for missing collision in scene conversion.
- [ ] Run collision tests; expected unsupported/missing codec.
- [ ] Implement bounded readers/writers, editable COL dumps and typed YBN conversion; explicit mapping losses
  use the shared policy, never silently invent physics metadata.
- [ ] Run full suite and parse real COLs. Reopen converted YBNs independently.
- [ ] Commit `feat: resolve and convert classic GTA collision`.

### Task 9: Scene rendering and YMAP/YTYP conversion

**Files:** create `../rage-formats/src/renderware/scene_convert.rs`;
create `src/commands/legacy_scene.rs`; extend plot/resource conversion.

**Interfaces:** `ResolvedScene::convert(options: &ConversionOptions)
-> Result<ConvertedScene>` containing named output bytes, ID mapping,
interior partitions and dependency reports; `ResolvedScene::render_scene()
-> Result<rage_render::Scene>` is a CLI adapter (formats does not depend on renderer).

- [ ] Test entity counts, transformed bounds, material/TXD lookup, LOD parents
  and child counts, interior map separation, missing-model/collision failures,
  and generated YTYP/YMAP/YBN/manifest references resolving to named outputs.
- [ ] Run scene conversion tests; expected missing adapter/converter.
- [ ] Implement dependency conversion and legacy resource generation using
  existing writers. Nonzero source interiors produce separate map sets with
  activation instructions and source IDs, never falsely claim generated MLOs.
- [ ] Run full suites; render a real selected area before/after conversion,
  compare placements and validate artifacts with independent readers. Record
  availability/result of FiveM runtime validation.
- [ ] Commit `feat: convert classic GTA scenes into GTA V assets`.

### Task 10: Batch/archive conversion, reports and MCP

**Files:** modify `src/commands/resource_convert.rs`, `src/mcp/tools.rs`;
create `src/conversion.rs`, `tests/renderware_convert.rs`, extend `tests/mcp.rs`.

**Interfaces:** `ConversionReport { inputs, outputs, mappings, losses, failures }`;
`convert_inputs(inputs: &[Source], options: &Options) -> Result<ConversionReport>`.
Support loose files, IMG selections, directories and deterministic output paths.
Add `--to gta5`, `--game-root`/GTASA_PATH, `--game`, `--report`, explicit allowed
losses and visual-only mode while preserving existing `--to legacy` behavior.

- [ ] Assert collision preflight prevents any writes, a mixed success/failure
  batch exits nonzero and reports both, dry runs write nothing, and the MCP
  operations return the same structured results as CLI without protocol noise.
- [ ] Run conversion/MCP tests; expected missing flags/tools/behavior.
- [ ] Implement preflight, write/reopen checks, deterministic reports and MCP
  operations for archive/resource/texture/render/conversion workflows.
- [ ] Run full suite and a representative IMG-selected batch; verify every
  report output exists and is parseable and every failure has a cause.
- [ ] Commit `feat: batch RenderWare conversion and MCP workflows`.

### Task 11: Real-game matrix and user documentation

**Files:** create `tests/gtasa.rs`, `docs/renderware.md`,
`docs/renderware-validation.md`; update README and command help.

**Interfaces:** opt-in tests read `GTASA_PATH`, output only under target/gtasa,
and skip with a clear reason when the install is absent. Reports identify exact
input hashes/names and oracle/tool revisions without distributing game content.

- [ ] Add opt-in inventory/parse/decode/roundtrip/edited rebuild/conversion tests
  asserting processed totals match inventory and all requested failures are
  represented. Test shell examples on small generated fixtures.
- [ ] Run new suite before final fixes to expose remaining unsupported cases.
- [ ] Resolve supported-layout failures; record genuinely unsupported encodings
  explicitly. Document all commands, schemas, loss policy and interior behavior.
- [ ] Run full suites in every affected crate and opt-in GTASA matrix. View
  representative images; run independent reader and available runtime checks.
- [ ] Commit `test: validate RenderWare workflows against San Andreas`.

## Completion review

Request a fresh whole-change reviewer against this plan, approved spec and the
progress ledger. Fix important findings with failing regression tests first.
Report completed workflows, tested assets, unsupported cases and unavailable
runtime validation. Do not report the overall request complete while a task or
required supported-layout workflow remains unfinished.
