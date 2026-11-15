# TXD, DFF, IPL and IMG support

Date: 2026-11-15
Status: proposed for approval

## Intent and requirements

Introduce full file workflows for TXD texture dictionaries, DFF models, IPL
placements and IMG archives: inspection, export, editing, rebuilding, rendering
and conversion to GTA V assets. This is the complete requested scope, rather
than an inspection-only first release. Implement and validate it in milestones,
but do not describe the overall work as complete until the acceptance criteria
below are met.

The user requires our own implementation, rather than adopting the licenses of
the Rust implementations found online. Implement binary readers and writers
independently from format descriptions and observed files. Existing projects
are behavioral references only: do not copy, translate, adapt or vendor their
code. Reuse our existing rpf-archive, rage-formats and rage-render libraries.
Keep the repository's existing dependency policy and attribution intact.

The installed PC San Andreas at `D:/Games/gtasa` supplies private validation
fixtures. It contains gta_sa.exe, models/gta3.img, gta_int.img, player.img,
cutscene.img, loose TXDs, and data/gta.dat with IDE/IPL references. Read it only;
write generated files under the CLI's ignored target directory. Do not commit
game assets, extracts, converted files or executable data.

A read-only TOC inventory confirms gta3.img is IMG2 (`VER2`) with 16,316
entries: 12,964 DFF, 2,759 TXD, 164 IPL, 216 COL, 149 IFP and 64 DAT. These
are inventory counts, not successful-parse counts. COL/IFP/DAT payloads must
survive archive rebuilding even where no semantic editor is provided.

## Approaches

1. Adopt standalone Rust parsers, adapting their data structures to our tools.
   Fastest initial parsing, but rejected by the user's requirement to implement
   our own code and avoid adopting those projects' licenses. Their advertised
   support also does not supply the whole conversion/rebuild pipeline.
2. Put all parsers and conversions directly in rage-cli. Keeps work in one repo,
   but contradicts the existing architecture in which reusable libraries own
   formats, archives and drawing.
3. Extend our existing libraries with independently implemented codecs and
   adapters, and wire the existing CLI/MCP workflows to them. Recommended design
   choice: consistent with the toolchain and the user's own-code requirement.

## Compatibility boundary

Target the classic PC RenderWare formats used by GTA III, Vice City and San
Andreas, with real-game validation first on the installed San Andreas. Support
SA text and binary IPL and the III/VC text instance layouts. Console-native and
mobile geometry/raster encodings are separate layouts: identify them explicitly
and retain their raw data for round trips, rather than pretending to decode them.
The absence of local III/VC fixtures must be stated in the verification report.

GTA V conversion targets legacy PC resources, matching the existing writers and
FiveM. A converted model/map is an asset conversion, not a port of San Andreas
gameplay, scripts, missions, animation systems or vehicle handling. DFF vehicle
and character hierarchies must remain inspectable/editable/rebuildable; producing
a functional GTA V vehicle or ped also needs target metadata and physics that a
DFF/TXD/IPL set alone does not contain. Never label a static exported vehicle
mesh as a functional converted vehicle.

## Ownership and integration

- rpf-archive: IMG readers/writers, checked offsets and sector alignment, archive
  update/rebuild primitives and metadata needed to report the format.
- rage-formats: RenderWare chunk representation, TXD/DFF/IDE/IPL data models,
  readers/writers, editable dumps, and conversion into existing RAGE types.
- rage-render: existing drawable rasterizer, texture sheets, GLB export and plan
  renderer, fed by adapters rather than an unrelated rendering implementation.
- rage-cli: command arguments, filesystem operations, game dependency resolution,
  conversion reports, batch execution and MCP integration.

Sibling repositories already exist beside this checkout. Make changes there
where ownership requires it. Use local dependency overrides during development;
keep dependency publication/pinning reviewable and do not publish as an implicit
part of this request. No permanent machine-specific paths in Cargo manifests.
Provide a documented local-toolchain build/test wrapper accepting a toolchain
root. It supplies temporary Cargo configuration patches for both crates.io and
the renderer's git source, resolving one rage-formats instance across all three
libraries. Record the source revisions and exact invocation in the test report;
ordinary users must not need undocumented global Cargo configuration.

## Archive workflows

The library already reads and writes IMG1/2/3. CLI gaps include IMG1 paired DIR
opening, IMG creation selection, archive format reporting, and an RSC7-only
entry loader that rejects ordinary IMG binary entries.

Extend `info`, `list`, `tree`, `search`, `extract` and `verify` to handle IMG
consistently. IMG1 opens its same-stem DIR automatically, with an explicit DIR
override for unusual locations. Missing DIR errors explain the required pair.
IMG2 and IMG3 use signature detection. Key loading must remain unnecessary for
unencrypted RenderWare archives.

Extend `create` with an explicit format selector (`rpf`, `img1`, `img2`, `img3`),
retaining existing RPF version flags and defaults. Add archive rebuild/update
operations for add, replace and remove, writing a new output archive. Retain
unchanged entries and format-specific metadata where possible. Validate IMG's
flat namespace, byte-length limits, duplicate case-insensitive names, sector
count limits and output collisions; never truncate names silently. IMG1 writes
and validates its output pair before reporting success.

Treat IMG entry sector padding as container data: preserve opaque entries and
use format-aware payload extents for TXD/DFF/IPL; do not trim arbitrary zeroes.
Reject malformed offsets/lengths and paths that escape extraction destinations.
`verify` must return a failing exit status for archive-open failures or detected
entry errors, validate sector/range consistency and provide an optional deep
extract/parse check. Its current success exit after detected errors must not
carry over into IMG support.

## Shared RenderWare model and editable documents

Represent chunks with checked type/length/version headers, bounded child ranges,
stable identities and original ordering. Preserve unknown plugin chunks,
version stamps and trailing container padding. Enforce allocation, nesting,
vertex/index, mip and chunk-count limits before allocating from file values.

`resource info` recognizes these formats before attempting RSC7 detection.
`resource dump` produces a versioned JSON document containing editable semantic
fields and the opaque chunks necessary for a lossless rebuild. `resource build`
recognizes those documents and validates references and ranges. Binary payloads
can use companion files under the dump directory, with checked relative paths.
Document schemas and editable fields. No-op dump/build must preserve unknown
data; edited builds must recompute lengths, offsets, counts and bounds.

## TXD

Read/write native PC D3D8 and D3D9 texture dictionaries, names/masks, filtering,
addressing, alpha flags, mip chains, raster formats and platform/version fields.
Decode uncompressed packed formats, PAL4/PAL8 and DXT1/3/5. Reuse existing pixel
and BC codecs where applicable. Preserve original encoded bytes for unedited
textures. Replacing/adding/removing textures must preserve unrelated chunks and
entries; explicit target game/platform chooses the layout of newly built TXDs.

Wire TXD into `textures` image/DDS export and sheets, and into texture build/from
workflows. Support loose files and archive entries. Preserve mip/alpha semantics
in conversion to YTD; use explicit re-encoding when no direct format mapping
exists, and report it. Names that collide after normalization are errors.

## DFF

Read/write clumps, frames and transforms, atomics, geometry lists, vertices,
normals, UV sets, colors, triangles and material sharing. Model common plugins
needed by PC assets: bin meshes, frame names, skin weights/bone indices, HAnim,
material effects, night colors, reflection/specular data and embedded collision.
Unmodeled plugins remain available as preserved opaque chunks in the dump.

Add DFF rendering to `screenshot`, including TXD resolution and missing-texture
reports. Export through existing GLB facilities with frame transforms, material
assignments and skinning retained where supported by the exporter. Extend the
exporter if necessary rather than silently flattening a rig. Test winding,
normals, parent transforms and mirrored/transparent materials visually.

DFF-to-YDR conversion builds existing typed RAGE blocks: geometry, materials,
texture references, bounds, skeletons and weights. Compute valid declarations,
index ranges and shader mappings. Preserve meaningful hierarchy in the editable
source; record target representations and any unavoidable loss in the conversion
report. Unsupported target semantics must fail by default; an explicit lossy
mode can allow named losses. Do not silently drop a plugin merely because an
independent reader can skip it.
Track plugin dependencies on geometry, frame, material and bone indices. An edit
that changes these invalidates affected plugin payloads: regenerate known
plugins, or reject the edit if an unknown dependent plugin cannot be updated.
Only explicit lossy removal may discard one. Do not preserve stale dependent
plugin bytes and call the result a lossless rebuild.

Rigged GLB export requires a new structured scene/rig interface with skeleton
nodes, inverse bind matrices, JOINTS/WEIGHTS attributes and frame transforms;
the existing flattened RenderPart geometry is insufficient. Include bind-pose
and deformation checks in the DFF milestone.

## IPL and dependency resolution

Read/write text IPL sections and SA binary IPL instance/car-generator tables.
Model instance identity, model ID/name, interior, position, rotation, scale where
the source provides it, and LOD references. Preserve comments, section ordering,
unknown text records and reserved binary fields in editable round trips. Reject
malformed known records with line/record context. Do not silently skip errors.

Read IDE model definitions and texture-parent relationships, plus gta.dat load
directives, to resolve numeric IDs in streamed IPLs and texture dictionaries.
Provide `--game-root`, explicit IDE/TXD/archive overrides and deterministic
precedence. The SA root is separate from GTA5's existing `--exe`/GTAV_PATH.
Resolve companion streamed IPLs as a single placement set with explicit LOD
index scope, rather than assuming all indices refer to the current file.

Scene conversion includes associated external COL files as well as embedded
collision: independently implement the necessary PC COL readers, resolve model
associations from IDE/archive data and generate YBN bounds. Retain material and
collision flags with an explicit mapping report. Missing collision is a distinct
conversion failure unless the user explicitly requests visual-only output;
visual success must never imply a collidable scene.

IPL-to-YMAP conversion also produces the necessary YTYP definitions, dependency
manifest, model/texture assets when requested, and a mapping report. Check
quaternion convention with known placements and preserve interiors, scale and
LOD relationships. Compute world and streaming bounds from transformed models.
Source interior IDs partition output into separate named map sets with source
metadata and activation instructions, keeping interiors from overlapping in one
always-active map. An SA interior ID is not a GTA V MLO definition. Automatic
MLO room/portal authoring requires information outside these formats and is not
implied; report this representation prominently in conversion results.
Non-placement sections with no GTA V analogue follow the explicit-loss policy.
Add IPL scene plotting/rendering through the existing scene representation.

## Command surface and conversion reports

Preserve existing commands and extend their accepted formats. Examples of the
intended interface (new flags are subject to consistency with Clap's current
arguments):

```sh
rage info gta3.img
rage resource info model.dff --json
rage resource info model.dff --archive gta3.img --json
rage resource dump model.dff -o model.json
rage resource build model.json -o edited.dff
rage textures gta3.img buildings.txd --sheet -o textures
rage textures build textures/*.dds --from original.txd -o edited.txd
rage screenshot gta3.img building.dff --txd buildings.txd --views front,iso
rage create assets -o rebuilt.img --format img2
rage resource convert source --to gta5 --game-root D:/Games/gtasa -o converted
```

Retain existing `resource convert --to legacy` behavior. Conversion supports
individual files, archive entries and batch directories/IMG inputs. Emit a
machine-readable per-source report: output paths, resolved/missing dependencies,
name/ID mapping, source and target counts, unsupported features, warnings,
explicit losses and success/failure. Exit unsuccessfully when requested inputs
fail. Detect output collisions before writing. Reopen generated files before
declaring conversion success; a parseable output alone is not proof it works
in the target game.

Expose corresponding operations through MCP using the same implementation and
error reporting. Existing GTA V catalog keys must retain their meaning; adding
a multi-game visual catalogue is a separate task, not a requirement for direct
IMG/resource workflows.

## Validation and acceptance

1. Small synthetic fixtures cover binary boundaries, malformed/truncated input,
   integer overflow, unknown-chunk retention, name collisions, IMG1 pairs and
   IMG2/3 read/write round trips. Unit tests verify coordinate and LOD mappings.
2. CLI integration tests exercise inspect/dump/edit/build/export/render/convert
   on fixtures without needing any installed game. Existing RPF/RSC7 tests pass.
3. An opt-in real-game suite reads `GTASA_PATH` (locally D:/Games/gtasa). Inventory
   all IMG entries and loose/streamed IPLs; parse all requested supported types,
   report every failure and select representative textures, props, vehicles,
   skinned characters, interiors, transparency, mip formats and LOD placements.
4. Rebuild unchanged representative files and archives, re-extract entries and
   compare preserved raw data and semantic state. Edit a texture, a mesh/material
   and a placement, then rebuild/reparse and demonstrate the intended changes.
5. Inspect generated images and scene plans, comparing source and converted
   renders for geometry/material/placement agreement. Record missing textures
   honestly. Test regenerated TXD/DFF/IPL/IMG files with an independent reader
   or the source game where practical; record the oracle used.
6. Reopen YTD/YDR/YBN/YTYP/YMAP outputs through our readers and an independent GTA V
   reader such as the local CodeWalker tooling. Validate a representative
   converted static scene in FiveM if a running target is available. Report any
   absent runtime validation explicitly rather than claiming game compatibility.
7. Final report names tested assets, unsupported layouts/semantics, successful
   conversions and rejected candidates with reasons. No game data is committed.

## Delivery order

1. IMG CLI integration and safe generic entry access.
2. Checked chunk foundation and editable lossless round-trip documents.
3. TXD codecs, editing/building, export, sheets and YTD conversion.
4. DFF codecs, editing/building, rendering, GLB and YDR conversion.
5. IDE/IPL/COL codecs, IPL editing/building, scene resolution and YBN/YMAP/YTYP conversion.
6. Batch archive conversion, MCP, real-game matrix and documentation.

This order is an implementation dependency sequence, not a reduction of scope.
Track unresolved functionality explicitly until the full requested workflow is
delivered. Dependency publishing, public releases, and modifying installed game
archives are not implicit completion steps.
