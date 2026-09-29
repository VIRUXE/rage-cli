# Ped composition and vehicle variants in `rage screenshot`

Closes VIRUXE/rage-cli#24 and VIRUXE/rage-formats#11. Ported from CodeWalker's
`GameFileCache.InitPeds/InitVehicles`, `World/Ped.cs`, `World/Vehicle.cs`,
`PedsForm.cs`, `VehicleForm.cs`, `Renderer.RenderPed/RenderPedComponent`,
`MetaTypes.cs` (`MCPedVariationInfo` and friends) and the typed file classes
`VehiclesFile`, `CarColsFile`, `CarVariationsFile`, `CarModColsFile`, `PedsFile`,
`PedFile`. Where CodeWalker has no code path (paint from carcols, liveries) the
game's own data drives the behaviour and the spec says so.

## What the user gets

```sh
rage screenshot --ped a_m_y_acult_01 --views front,iso --grid
rage screenshot --ped a_m_y_acult_01 --component uppr=2:1 --component hair=none
rage screenshot --vehicle police --hi --livery 2 --colour-from carcols
rage screenshot ./nested/levels/gta5/vehicles.rpf police.yft --hi --livery 2 --colour-from carcols:1
```

- `--ped NAME` composes the ped from its `.ymt` variation info: every one of the
  12 component slots (head berd hair uppr lowr hand feet teef accs task decl
  jbib) gets drawable 0, alternative 0, texture 0 (`Ped.LoadDefaultComponents`),
  each drawable found in the ped's `.ydd` or its streamed per-component `.ydd`,
  each texture in the ped's `.ytd` or its streamed `.ytd`, and the texture
  replaces the drawable's `DiffuseSampler` (`Renderer.RenderDrawable`'s
  `diffOverride`). `--component SLOT=D[:T[:A]]` picks drawable index `D`,
  texture index `T` and alternative `A` for a slot; `SLOT=none` leaves it out.
  One composite image per view, named after the ped.
- `--vehicle NAME` resolves the vehicle through the index (its `vehicles.meta`
  entry and its `.yft`) instead of an archive path. `--hi` swaps in `NAME_hi.yft`
  (`Vehicle.Init(hidef)`), falling back to the base model with a warning when
  there is none. Both work with the positional `ARCHIVE FILE` form too.
- `--livery N` shows livery `N` (0-based, as the game counts them): every
  texture the model references whose name ends in `_sign_1` is read from
  `<prefix>_sign_<N+1>` instead. A livery the model's carvariations entry marks
  unavailable, or whose texture is missing, is reported.
- `--colour-from carcols[:C]` paints the vehicle with the primary colour of
  colour combination `C` (default 0) from its carvariations entry, looked up in
  carcols' colour list; prints the colour's name. Conflicts with `--paint`.

## Data the game provides

| File | Where | Form | CodeWalker class |
|---|---|---|---|
| `vehicles.meta` | `update.rpf/common/data/levels/gta5/`, `dlc.rpf/common/data/levels/gta5/`, `dlc_patch/*` | XML | `VehiclesFile` (`VehicleInitData`) |
| `carcols.ymt` / `carcols.meta` | `update.rpf/x64/data/`, `dlc.rpf/common/data/` | PSO / XML | `CarColsFile` (`CVehicleModelInfoVarGlobal`) |
| `carvariations.ymt` / `.meta` | same | PSO / XML | `CarVariationsFile` (`CVehicleModelInfoVariation`) |
| `carmodcols.ymt` | `update.rpf/x64/data/` | PSO | `CarModColsFile` (`CVehicleModColours`) |
| `peds.ymt` / `peds.meta` | `update.rpf/x64/data/`, `dlc.rpf/common/data/` | PSO / XML | `PedsFile` (`CPedModelInfo__InitDataList`) |
| `<ped>.ymt` | `componentpeds_*.rpf`, `streamedpeds_*.rpf` beside `<ped>.ydd/.ytd/.yft` | RSC7 Meta | `PedFile` (`MCPedVariationInfo`) |
| `<ped>/<part>.ydd`, `<ped>/<tex>.ytd` | a folder named after the ped, next to its `.ymt` (streamed peds) | RSC7 | `PedDrawableDicts` / `PedTextureDicts` |

Retail check: `police.yft` references `policenew_sign_1`; `police.ytd` holds
`policenew_sign_1` … `policenew_sign_6`. Liveries are texture swaps by name,
not shader parameters. Every ped in `x64v.rpf/models/cdimages/componentpeds_*`
keeps `<ped>.ydd/.yft/.ymt/.ytd` together; `streamedpeds_*` keeps `<ped>.yft`
and `<ped>.ymt` with the parts in a `<ped>/` folder.

## rage-formats 0.5.0

New modules, each a typed reader over the generic tree (`MetaValue`), so one
reader serves the PSO `.ymt` and the XML `.meta` form of a file. A private
`meta_read` helper set (`str_of`, `hash_of`, `u32_of`, `f32_of`, `bool_of`,
`vec3_of`, `items_of`, `byte_list_of`) absorbs the two forms' differences: XML
scalars arrive as `I32`/`F32`/`Str`, PSO scalars as their exact type; XML
`<indices>0 1 2</indices>` is a `Str`, PSO's is a byte array; XML enums are
their names, PSO enums carry the value and name hash.

- `vehicles.rs`: `parse_vehicles_meta(&[u8]) -> VehiclesMeta { resident_txd,
  init_datas: Vec<VehicleInitData>, txd_relationships }`. `VehicleInitData`
  carries every field `VehiclesFile.cs` reads (names, camera names, the IK
  offsets, dirt/envEff/damage scales, `diffuse_tint`, `lod_distances`, flags,
  type/plate/dashboard/class/wheel type, trailers, drivers, extras, rewards,
  cinematic cameras, buoyancy, ragdoll threshold, first-person drive-by data).
  XML only, as CodeWalker (`vehicles.meta` is never PSO in the game).
- `carcols.rs`: `parse_carcols(&[u8]) -> CarCols` = `CVehicleModelInfoVarGlobal`
  with `plates`, `colors: Vec<VehicleModelColor { color: u32 (ARGB), metallic_id,
  audio_color, audio_prefix, audio_color_hash, audio_prefix_hash, color_name }>`,
  `metallic_settings`, `window_colors`, `lights` (`VehicleLightSettings` with
  its lights and coronas), `sirens` (`SirenSettings` with sequencers and
  `SirenLight`s), `kits: Vec<VehicleKit { kit_name, id, kit_type, visible_mods,
  link_mods, stat_mods, slot_names, livery_names, livery2_names }>`, `wheels:
  Vec<Vec<VehicleWheel>>`, `global_variation_data`, `xenon_light_colors`. Enums
  (`ModKitType`, `VehicleModType`, `VehicleModBone`, `VehicleModCameraPos`,
  `MetallicId`, `AudioColor`, `AudioPrefix`) are Rust enums with CodeWalker's
  values and names, parsed from either the name or the value; unknown values
  keep the raw number.
- `carvariations.rs`: `parse_carvariations(&[u8]) -> CarVariations { variation_data:
  Vec<VehicleVariation { model_name, colors: Vec<ColorCombination { indices:
  Vec<u8>, liveries: Vec<bool> }>, kits: Vec<u32>, windows_with_exposed_edges,
  plate_probabilities, light_settings, siren_settings }> }`.
- `carmodcols.rs`: `parse_carmodcols(&[u8]) -> CarModCols { metallic, classic,
  matte, metals, chrome, pearlescent { base_cols, spec_cols } }` of
  `VehicleModColor { name, col, spec }`.
- `peds.rs`: `parse_peds_meta(&[u8]) -> PedsMeta { resident_txd, resident_anims,
  init_datas: Vec<PedInitData>, txd_relationships, multi_txd_relationships }`;
  `PedInitData` carries every field `PedsFile.cs` reads.
- `ymt.rs` gains CodeWalker's naming on `PedVariationInfo`: `COMPONENT_NAMES`
  (the 12 slot names), `component(slot) -> Option<&ComponentData>`
  (`GetComponentData`: `avail_comp[slot]` indexes `component_data`),
  `DrawableData::prop_type` (`(prop_mask >> 4) & 3`), `drawable_name(slot,
  index, alt)` (`uppr_000_u`, suffix `u`/`r`/`m`/`m` by prop type, `_<alt>` when
  alt > 0) and `texture_name(slot, index, tex)` (`uppr_diff_000_a_whi`: letter
  `a`+tex, race code from `tex_id`: 0 uni, 1 whi, 2 bla, 3 chi, 4 lat, 5 ara,
  8 kor, 10 pak, else whi), plus `variants(slot)` listing every (drawable,
  alternative, texture) triple `PedsForm.PopulateCompCombo` would offer.
  `DIFFUSE_SAMPLER` becomes public for the renderer.

Tests: hand-written XML fixtures for each file (a colour, a kit with liveries, a
variation with two combinations, one ped, one vehicle) and PSO fixtures built
with `build_pso` from the same trees, round-tripped through the readers; unit
tests for the ped names against CodeWalker's rules.

## rage-render 0.5.0

- `RenderPart.diffuse_override: Option<&'a str>`: when set, every geometry of
  the part whose shader has a `DiffuseSampler` parameter samples that texture
  instead (CodeWalker replaces only geometries whose texture parameter hash is
  `DiffuseSampler`). The report's missing list names the override when it is
  absent.
- `TextureSet::alias(&mut self, name: &str, target: &str) -> bool`: `name`
  resolves to `target`'s image from then on (a top-priority layer); false when
  `target` is unknown.
- `RenderReport` unchanged otherwise.

## rage-cli

### Index

Two new parts, each in its own cache file; format version 12.

- `Parts::PEDS` (`peds.bin`): `ped_init: HashMap<u32, PedIndexEntry { name,
  props_name, clip_dictionary_name, is_streamed_gfx }>` from every `peds.ymt`
  and `peds.meta` (last wins, CodeWalker's `allPeds[hash] = initData`);
  `ped_files: HashMap<u32, PedFiles { ymt, ydd, ytd, yft: Option<EntryLoc>,
  streamed: Vec<(u32, EntryLoc)> }>` keyed by `joaat(lowercase stem)` of every
  `.ymt` that has a `.ydd`, `.ytd` or `.yft` of the same stem beside it, or a
  sibling folder of its name (`GameFileCache.addPedDicts`), the folder's
  `.ydd/.ytd/.yld` files keyed by their own stem hash. Last archive wins.
- `Parts::VEHICLES` (`vehicles.bin`): `vehicle_init: HashMap<u32, VehicleIndexEntry
  { model_name, txd_name, game_name, vehicle_make_name, vehicle_type,
  vehicle_class }>` from every `vehicles.meta` (last wins); `car_colors:
  Vec<CarColorEntry { color, name, metallic_id }>` from the last `carcols.ymt`
  or `.meta` in load order whose colour list is not empty; `car_variations:
  HashMap<u32, VariationEntry { colors: Vec<(Vec<u8>, Vec<bool>)>, kits }>` (last
  wins, `allCarVariationsDict[hash] = variation`); `car_kits: HashMap<u32,
  KitEntry { id, livery_names, livery2_names }>` by kit name hash, last wins.
- `rage index` summary and README table gain the two parts.

### `screenshot`

- `ARCHIVE` and `FILE` become optional; `--ped NAME` and `--vehicle NAME` are
  the alternatives, each requiring `--exe`/`GTAV_PATH`. Exactly one of the
  three forms.
- `--component SLOT=SPEC` repeatable (`SPEC` = `D[:T[:A]]` or `none`), only with
  `--ped`. `--entry`, `--hi`, `--livery`, `--colour-from` are rejected with
  `--ped`; `--component` is rejected without it.
- `--hi`, `--livery N`, `--colour-from carcols[:C]` with `--vehicle` or with a
  positional `.yft`. In the positional form the model's stem is the vehicle
  name; carcols/carvariations still come from the index (so `--exe` is needed
  for `--livery`'s availability check and for `--colour-from`; `--hi` first
  looks for `<stem>_hi.yft` in the same archive, then in the index).
- Ped composition (`src/peds.rs`): resolve the ped (index `PEDS`), load its
  `.ymt`, `.ydd`, `.ytd`; for each slot compute the names, find the drawable
  (own `.ydd` by name hash, else the streamed `.ydd` of that hash: its first
  drawable), find the texture (own `.ytd`, else the streamed `.ytd` of that
  hash: its first texture), push the streamed textures as layers, and emit one
  `RenderPart` per slot with `diffuse_override` set. Prints one line per slot
  (`uppr: uppr_000_u / uppr_diff_000_a_whi`, or `uppr: none`), warns for a
  drawable or texture it could not find, and fails only when no slot yielded a
  drawable. The texture chain for the composite is the ped's own `.ytd`,
  the streamed dictionaries, then the usual index resolution for the ped's
  name (so `vehshare`/`mapdetail` fallbacks still apply).
- Vehicle variants (`src/vehicles.rs` in rage-cli): `resolve_vehicle(name,
  hi)` picks the `.yft` location; `apply_livery(parts, textures, n)` aliases
  every `*_sign_1` reference; `paint_from_carcols(name, combination)` reads the
  index. Output files are named after the model rendered (`police_hi`).

### README

`screenshot` row in the command table, a "Peds" subsection and a "Vehicle
variants" subsection under "Render a model to an image", the index table
rows, and the release notes list.

### Tests

- Unit: `--component` parsing, slot names, livery aliasing on a stub texture
  set, colour selection from a stub index, `_hi` fallback.
- Retail (`tests/smoke_peds_vehicles.rs`, skipped without `GTAV_PATH`): the
  index builds the two parts; `screenshot --ped a_m_y_acult_01` prints the 12
  slot lines with names CodeWalker's rules give and writes an image whose
  pixels are not all background; `screenshot --vehicle police --hi --livery 2
  --colour-from carcols` prints `police_hi`, `policenew_sign_3` and a colour
  name, and writes an image. No CodeWalker binary is run: the oracle was
  deleted on 2026-09-29.

## Non-goals

Props (hats, glasses: `CPedPropInfo`), cloth (`.yld`), expressions and
animation (`.yed`, `.ycd`), mod kits on the model (`--mod`), secondary/pearl
colours, plates, window tint. `vehiclelayouts*.meta` stays unparsed (CodeWalker
keeps only its XML text).

## Release

rage-formats 0.5.0 and rage-render 0.5.0 are published first (asked before
publishing), then rage-cli 0.25.0 pins them. Until then rage-cli builds with a
`[patch.crates-io]` section pointing at the sibling checkouts.
