# Drawables and bounds from XML: `resource dump` / `resource build` for `.ydr` and `.ybn`

Date: 2026-09-28

## Goal

`rage resource build model.xml -o model.ydr` compiles CodeWalker-format XML into a
legacy-PC `.ydr` the game and CodeWalker load, and `rage resource dump model.ydr -o
model.xml` produces that XML (plus the embedded textures as `.dds`) from a game file.
The same pair works for `.ybn`. A model exported or edited as XML (Sollumz, CodeWalker,
by hand) can then be compiled, and a game model exported, without CodeWalker.

**Parity bar.** Build accepts everything CodeWalker's `XmlYdr.GetYdr` / `XmlYbn.GetYbn`
accept; dump emits everything `YdrXml.GetXml` / `YbnXml.GetXml` emit. That covers:
bounds, LOD distances and flags; `ShaderGroup` with an embedded `TextureDictionary`
and shaders with Texture / Vector / Array parameters; `Skeleton` and `Joints`; `Lights`;
an embedded `Bounds` tree of every kind (Sphere, Capsule, Box, Geometry, GeometryBVH,
Composite, Disc, Cylinder, Cloth as CodeWalker handles it) with all five polygon kinds;
`DrawableModelsHigh/Medium/Low/VeryLow` and `DrawableModelsX`; vertex buffers with
`Layout`, `Data` and `Data2`; index buffers. Gen9 is not part of CodeWalker's XML path
and is out of scope.

Success: the round-trip `dump → build → dump` on real game props yields identical XML;
the built file reads back through both `rage-formats` readers; CodeWalker.Core loads the
built file and exports the same XML; a built prop streams in FiveM.

## Where the code lives

- `rage-formats` (local checkout, path dependency during development): a new `blocks`
  module holding the block framework, the ported block types, and their XML. Released
  as **0.3.0**.
- `rage-cli`: `resource dump` / `resource build` / `resource info` wiring. Released as
  **0.23.0** on `rage-formats 0.3.0` from crates.io, as previous releases were done.
- `codewalker-cli` (net48, references the local `CodeWalker.Core.dll`): a `verify`
  command used only as a test oracle.

## Section 1 — Block framework (`rage_formats::blocks`)

A Rust port of CodeWalker's `IResourceBlock` / `ResourceBuilder` / `ResourceDataReader`
/ `ResourceDataWriter`. In CodeWalker every block knows its `BlockLength`, the blocks it
points to (`GetReferences`), the sub-blocks embedded inside it at an offset
(`GetParts`), and can `Write` itself once every block has a `FilePosition`. Identity
matters: one `Texture` object is reached from the dictionary and from a shader
parameter; `GetBlocks` dedups by identity and the reader's `blockPool` returns the same
object for the same virtual address.

The port is an arena, so pointers are ids and identity is free:

```rust
pub struct BlockId(u32);
pub struct Graph { blocks: Vec<Box<dyn Block>>, positions: Vec<u64> }

pub trait Block {
    fn length(&self) -> usize;                                // BlockLength
    fn section(&self) -> Section;                             // System | Graphics
    fn references(&self, g: &Graph) -> Vec<BlockId>;          // GetReferences
    fn parts(&self) -> Vec<(usize, BlockId)>;                 // GetParts (embedded)
    fn write(&self, w: &mut Writer, g: &Graph) -> Result<()>; // pointers via g.position(id)
}
```

- **Layout** — `Graph::build(root) -> Vec<u8>`: walk references and parts from the root
  as `GetBlocks` does (visited set of ids; parts' children are walked, parts themselves
  are not top-level blocks); split system/graphics; assign positions with `pack_pages`
  (a port of `AssignPositions2`: root first, largest-first first-fit, 16-byte alignment,
  five page sizes, base shift grows until the counts fit). Two gaps to close in
  `pack_pages`: CodeWalker starts the base shift so the base page is at least the
  *smallest* block, and graphics pages are capped at `128 − system pages`. Then write
  each block at its position, asserting it wrote exactly `length()` bytes, and wrap
  with `build_rsc7_with_flags` (version 165 for `.ydr`, 43 for `.ybn`).
- **Saving-only blocks** CodeWalker creates inside `GetReferences` (`string_r`,
  `ResourceSystemStructBlock<T>`, `ResourceSimpleArray<T>`) are ordinary arena blocks
  created when the graph is assembled, so `references()` is pure.
- **Reader** — `Graph::read(bytes, kind)`: a port of `ResourceDataReader` with the
  `blockPool` as `HashMap<u64, BlockId>`, so re-reading a virtual address yields the
  same id. This is a second drawable reader beside `ydd.rs`; the existing parser and
  everything on it (render, screenshot, textures) is untouched. Re-basing `ydd.rs` on
  the graph is a possible follow-up, not part of this project.
- **Base types** (`blocks/base.rs`): `FileBase` (16 B: VFT, 1, pages-info pointer),
  `PagesInfo` (16 + 8·pages), `StringBlock` (len+1), `SimpleList64<T>` (pointer, count
  u16, capacity u16, pad), `SimpleList64b<T>` (u32 count and capacity; BVH nodes),
  `PointerArray64<T>` (8·n), `PointerList64<T>` (header + pointer array), `StructArray<T>`,
  `RawBytes`.
- Not ported: Gen9 paths, `AssignPositionsForMeta`, the `Meta` special case.

## Section 2 — Block catalogue

One Rust block type per CodeWalker class, same byte layout, same `Write`, same
`ReadXml`/`WriteXml`, same derived-field rules; every `Unknown_xx` keeps its default
constant. Legacy PC sizes.

`blocks/drawable.rs`

| Block | Size | Notes |
|---|---|---|
| `Drawable` (`DrawableBase` + `Drawable`) | 208 | VFT 1079456120; bounds, 4 LOD distances, 4 render-mask words, pointers to shader group / skeleton / joints / models / name / bound; `SimpleList64<Light>` embedded at 0xB0 as a part; `DrawableModelsBlocksSize` = ⌈models block / 16⌉ |
| `DrawableModelsBlock` | computed | High/Med/Low/VLow/Extra lists back-to-back: per list a 16 B pointer-list header, n pointers, then each model 16-aligned; the LOD pointers point into this block |
| `DrawableModel` | 48 + mapping + pointers + AABBs + geometries | VFT 1080101528; shader mapping u16×n (n == 1 → +6 pad, else align 16), geometry pointers, `AABB_s` × (n > 1 ? n+1 : n) outer box first, geometries embedded 16-aligned as parts; `SkeletonBinding` packs HasSkin/BoneIndex/Unknown1, `RenderMaskFlags` packs RenderMask/Flags |
| `DrawableGeometry` | 152 (+8 if > 4 bone ids) + 2·ids | VFT 1080133528; counts from the buffers; bone ids at the tail |
| `Light` (struct) | 168 | every field from XML |

`blocks/shader.rs`

| Block | Size | Notes |
|---|---|---|
| `ShaderGroup` | 64 | VFT 1080113136; texture dictionary pointer, `PointerArray64<ShaderFx>`, counts, `ShaderGroupBlocksSize` = 4 |
| `ShaderFx` | 48 | name and file-name hashes, render bucket, `RenderBucketMask = (1 << bucket) \| 0xFF00`, parameter and texture-parameter counts, `ParameterSize` / `ParameterDataSize` from the parameters block |
| `ShaderParametersBlock` | 32 + Σ(16 + 16·type) + 4·n, then padding | n 16 B records (type 0 texture pointer, 1 one Vec4, k > 1 k Vec4s), the Vec4 data as parts, the n name hashes, then `32 + 4·ParametersDataSize` zero bytes; `Unknown_1h` = i+2 for textures and the descending 160+ offsets for vectors, as `ReadXml` sets them |

`blocks/vertex.rs` — `VertexBuffer` 128 (VFT 1080153080; `Data2` written when present,
else `Data1` for both pointers; flags 0/1024), `VertexDeclaration` 16 (stride and count
derived from flags and types), `VertexData` (raw bytes, system section), `IndexBuffer`
96 (VFT 1080152408) + `StructArray<u16>`. Encode/decode for the nine component types
(Float, Float2, Float3, Float4, Half2, Half4, Colour, UByte4, RGBA8SNorm) ported from
`SetString` / `GetString`, half floats through an f16 helper.

`blocks/skeleton.rs` — `Skeleton` 112 (VFT 1080114336), `SkeletonBonesBlock` 16 + 80·n
with `Bone` 80 embedded as parts (name → `StringBlock`), `SkeletonBoneTag` 16 in a
`PointerArray64` hash table with `Next` chains, `StructArray<Mat4>` × 2 (transforms,
inverses), `StructArray<i16>` × 2 (parent, child indices). `Joints` 64 (VFT 1080130656)
+ `StructArray<JointRotationLimit>` (128 B) / `JointTranslationLimit` (64 B).

`blocks/texture.rs` — `TextureDictionary` 64 (`FileBase`, `SimpleList64<u32>` hashes
sorted ascending, `PointerList64<Texture>` in the same order), `Texture` 144
(`TextureBase` 80 + dimensions, format, levels, stride, data pointer), `TextureData`
(graphics section, `.ytd` mip layout from `ytd.rs`). Reuses `TextureFormat`,
`parse_dds`, `to_dds`.

`blocks/bounds.rs` + `blocks/bvh.rs` — `Bound` base 112 (type byte at 0x10; box, sphere,
margin, material, flags, volume, inertia) with `Sphere` 112, `Box` 112, `Capsule` /
`Disc` / `Cylinder` 128, `Geometry` 304 (quantised `BoundVertex_s` i16×3, 16 B polygons
with the kind in the low 3 bits of byte 0, 16 B materials, material colours, vertex
colours, shrunk vertices, `Octants` 128 + items, polygon-material bytes),
`GeometryBVH` 336 + `Bvh` 128 (`SimpleList64b<BvhNode>` 8 B nodes with padded
capacity, `SimpleList64<BvhTree>` 16 B), `Composite` 176 (`PointerArray64<Bound>`
children, `Matrix4F_s` × n, `AABB_s` × n, flags1/flags2 × n, optional BVH), `Cloth`
exactly as CodeWalker reads and writes it. Polygons: Triangle (area, three vertex
indices with the flag in bit 15, three edge indices), Sphere, Capsule, Box, Cylinder.

## Section 3 — XML

Format is CodeWalker's verbatim: element and attribute names, `<Item>` arrays, `type`
attributes, vertex-row text and raw index/vertex arrays (`YdrXml`, `YbnXml`, `YtdXml`).
Numbers print in the shortest round-tripping form (`xml::float`); integers as decimals.
Both directions live in `blocks::xml`, one `WriteXml`/`ReadXml` pair per block, next
to the block's byte layout and derived-field rules — one file per CodeWalker class.

- **Dump**: root `<Drawable>` or `<Bounds type="…">`, two-space indent. Hashes print as
  names when known, else `hash_XXXXXXXX`. Shader parameter names come from a built-in
  shader-parameter name list added to `names/`; bone, shader and texture names are
  strings. Textures write `<name>.dds` beside the XML (`to_dds`) and `<FileName>`;
  `--no-dds` skips the files.
- **Build**: `roxmltree`; optional sections are optional exactly where CodeWalker's
  `ReadXml` tolerates their absence (no `Skeleton` → none; no `Lights` → empty list;
  `<Bounds type="None"/>` → no bound). Texture items read their `.dds` from the XML's
  folder (or `--textures DIR`) and take width, height, format, levels and stride from
  the file; a missing file is an error naming the path. Vertex rows are parsed against
  `<Layout>`; a row with the wrong column count is an error naming the vertex index.
  Hash strings accept a name or `hash_…`.

## Section 4 — Derived on import (never trusted from the XML)

What CodeWalker recomputes in `ReadXml` / `GetReferences`; ours matches it.

- **Drawable**: per-LOD render mask = OR of the models' masks; vertex declarations
  deduplicated by declaration id; models block size; geometry counts (indices,
  triangles = indices / 3, vertices, stride) from the buffers; model AABB list (outer
  box first when more than one geometry) and shader mapping from the geometries.
- **Shaders**: render-bucket mask; parameter counts and sizes; the `Unknown_1h`
  numbering; a texture parameter re-pointed at the embedded texture whose name hash
  matches.
- **Skeleton**: parent-index array; the breadth-first-by-4 child-index array with its
  8-entry padding; the bone-tag hash table (`GetNumHashBuckets`, bucket chains
  reversed); `Index2 = Index`; local matrices (rotation, translation, scale, `Column4`
  from `TransformUnk`) and global-inverse matrices with `Column4` zeroed.
- **Bounds**: materials table and per-polygon material indices (dedup by material
  struct); triangle edge indices from shared edges; triangle areas; quantum =
  (max − min) / 2 / 32767 with vertices quantised about `CenterGeom`; for `Geometry`
  only: shrunk vertices (margin-halving loop with the by-normals fallback) and octants;
  for `GeometryBVH`: the BVH (leaf threshold 4, polygons reordered to node order, edge
  indices remapped) and the bound's box and sphere from it; for `Composite`: children
  flags, AABBs (`Min.w = ε`, `Max.w = margin`) and transforms (flags 0,1,1,0; the
  fragment-owned variant 0x7f800001 × 4 kept for later `.yft` use), and a BVH only with
  six or more children (threshold 1, capacity 2n + 1 padded with `ItemId = 1` nodes).
  The BVH split heuristic (mean-centre axis choice, sort fallback, children sorted by
  item count, trees cut at 127 nodes) is ported line for line so node order matches.

## Section 5 — CLI

- `rage resource dump FILE.ydr|FILE.ybn -o FILE.xml [--no-dds] [--names FILE]
  [--archive RPF]` — `.dds` files beside the XML.
- `rage resource build FILE.xml -o FILE.ydr|FILE.ybn [--textures DIR] [--strict]
  [--format ydr|ybn]` — container from the output extension, or from the root element
  when ambiguous. After writing, the file is read back through the graph reader and
  compared with what was built, as `run_build` already does for meta; a file the tool
  cannot reload is never handed over.
- `rage resource info` reports the new sections for a `.ydr` (skeleton bone count,
  lights, embedded bound summary); nothing else in `info` changes.
- Warnings for what CodeWalker accepts silently (a shader parameter naming a texture
  that is not embedded, a bone without a name); errors with path and line for what it
  throws on (missing DDS, unknown bound type, malformed vertex row).

## Section 6 — Testing

1. **Unit, per block**: `length()` equals the CodeWalker constant; write-then-read of a
   hand-built block round-trips; derived-field rules checked against values captured
   from real files (bone-tag buckets, `Unknown_1h` numbering, BVH node order on a small
   fixed polygon set).
2. **Round-trip on real props** (`GTAV_PATH`-gated like `tests/smoke.rs`), one per
   feature: plain prop, embedded textures, lights, skeleton, `Composite` bound of mixed
   kinds, `GeometryBVH`: `dump → build → dump` gives identical XML, and the built file
   parses through both readers (`blocks` and `parse_ydr`) with equal geometry, shaders
   and bounds.
3. **Oracle**: `codewalker-cli verify FILE.ydr -o out.xml` loads the file with
   `YdrFile.Load` and writes `YdrXml.GetXml`; the test diffs it against our dump of the
   same built file. Gated on the exe existing.
4. **In-game**: a built prop streamed into FiveM through the MCP server, screenshot as
   evidence; done manually at the end, not in CI.

## Section 7 — Delivery

`rage-formats` first (path dependency in `rage-cli/Cargo.toml` while developing),
released as 0.3.0 from crates.io; then `rage-cli` 0.23.0 on the crates.io version.
README gains the two commands and the parity statement.
