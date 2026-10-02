# `rage plot --region ... --game`: a region of the vanilla map

Closes VIRUXE/rage-cli#25; takes the heightmap and water parts of
VIRUXE/rage-formats#10. Ported from CodeWalker's `Space.cs` (`InitCacheData`,
`GetVisibleYmaps`, `IsYmapAvailable`, `SpaceMapDataStore`), `YmapFile.cs`
(the entity hierarchy built in `EnsureEntities`, `ConnectToParent`, the
`LodDist`/`ChildLodDist` fallbacks), `Renderer.cs` (`RenderLodManager`,
`RenderWorldYmapIsVisible`, `RenderWorldAddInteriorEntities`,
`RenderWaterQuads`), `World/Water.cs`, `World/Heightmaps.cs`,
`HeightmapFile.cs` and `GameFileCache.cs` (`YmapDict`, `cache_y.dat` scan).

## What the user gets

```sh
rage plot --game --region -1700,-1200,-1100,-600 -o docks.png
rage plot --game --region -1700,-1200,-1100,-600 --detail 4 --layers entities,water,terrain -o docks.svg
rage plot --game --region 3200,-5300,3900,-4600 --hour 22 --max-lod lod -o cayo.png
```

- `--game` assembles, for the `--region` box, every vanilla `.ymap` the game
  would stream there, in load order: the `cache_y.dat` files say which maps
  cover the box (their entities extents, as CodeWalker's box query uses) and
  their parent chains come with them. The maps are read out of the archives
  through the index, entities linked into the LOD tree exactly as CodeWalker
  links them, and the leaves a map view at the region's scale would show are
  drawn as entity marks with their models (through the existing prop
  resolver), interiors included (an MLO instance's own entities, placed in
  the world).
- `--detail D` is CodeWalker's `MapViewDetail`: the view distance that picks
  the LOD level is the region's longer side divided by `D`. `1` (the default)
  shows a 600 m region as LOD/SLOD geometry, as CodeWalker's map view does at
  that zoom; a larger `D` brings HD props in.
- `--max-lod LEVEL` is CodeWalker's `MaxLOD`: `hd lod slod1 slod2 slod3 slod4`
  cap the tree at that level, `orphanhd` (the default) shows everything.
- `--hour H` (0-23) and `--weather NAME` hide the timed and weather-gated maps
  that `_manifest.ymf` map data groups switch off at that hour or weather
  (`Space.IsYmapAvailable`); without them every map is drawn.
- Two new layers, on by default: `water` fills the `water.xml` quads
  (`water_heistisland.xml` too) over the region, dashed when invisible;
  `terrain` draws height contours from `heightmap.dat` and
  `heightmapheistisland.dat` (the max-height surface, at a round interval that
  gives about ten levels over the region, every fifth line heavier).
- `navmesh` and `paths` fetch the cells covering the region from the game;
  `collision` reads the `.ybn` chunks the cache's bounds store places there.
  With `--game`, the default layers are `entities,water,terrain,navmesh,paths`;
  an explicit `--layers` is honoured as given, collision and drawable included.
- Everything else `plot` does still works: `--region` without `--game` frames
  the inputs as before; inputs and `--game` combine, so a resource folder can
  be drawn over its vanilla surroundings.

## Data the game provides

| File | Where | Form | CodeWalker |
|---|---|---|---|
| `*cache_y.dat` | `update.rpf/x64/data/cacheloaderdata/`, `dlc.rpf/x64/data/cacheloaderdata_dlc/` | text + records | `CacheDatFile`; `Space.InitCacheData` keeps the last node per name whose `.ymap` exists |
| `heightmap.dat`, `heightmapheistisland.dat` | `update.rpf/common/data/levels/gta5/` (`common.rpf` without DLC) | `HMAP` big-endian, row-compressed | `HeightmapFile`, `Heightmaps.Init` |
| `water.xml`, `water_heistisland.xml` | `common.rpf/data/levels/gta5/`, `update.rpf/common/data/levels/gta5/` | XML | `Water.LoadWaterXml`; only `WaterQuads` are rendered |
| `_manifest.ymf` | every map pack | PSO/RBF | `Space.InitCacheData`: `MapDataGroups` hours and weather |
| `.ymap` | map packs | RSC7 Meta | `YmapFile`; `YmapDict[shortNameHash]`, last wins |

Retail (build 3889): `heightmap.dat` is 183x249 cells over (-4050,-4050)..(5100,8400), 816 m tall;
the island one is 50x50 cells over (3500,-6500). `water.xml` holds 504 water quads (542 calming, 116 wave), the island file 62 more.

## rage-formats 0.5.0

- `ymap.rs`: `YmapEntity` gains `child_lod_dist` (@80), `lod_level` (@84) and
  `num_children` (@88), with `lod_in_parent_ymap()` (flags bit 3) and the
  `LodLevel` constants (`HD=0 LOD=1 SLOD1=2 SLOD2=3 SLOD3=4 ORPHANHD=5 SLOD4=6`).
  The test-support fixture grows a `sample_lod_ymap` that writes parent, LOD
  level, parent index and the distances.
- `heightmap.rs`: `parse_heightmap(&[u8]) -> Heightmap { width, height, bb_min,
  bb_max, max_heights, min_heights, little_endian }` decoding the compressed
  rows as `HeightmapFile.Read` does (both endiannesses), `serialize_heightmap`
  writing them back as `Write` does, and `Heightmap::max_height_at(ix, iy)` /
  `cell_size()` for callers that sample it. Round-trips the retail file byte
  for byte under `RAGE_TEST_HEIGHTMAP`.
- `water.rs`: `parse_water_xml(&str) -> WaterData { quads, calming_quads,
  wave_quads }` with the fields `Water.cs` reads.

## rage-render 0.5.0

- `Layer::Water` and `Layer::Terrain` after `Paths`; `Scene.water: Vec<WaterQuadShape
  { x0, y0, x1, y1, z, invisible }>` and `Scene.terrain: Option<HeightField {
  x0, y0, step_x, step_y, width, height, z: Vec<f32> }>`.
- `terrain` is drawn as contour lines by marching squares over the field at
  `contour_step(z range)`: the round step (1 2 5 10 20 50 100 m) giving at most
  twelve levels across the region's z range; every fifth level a heavier line.
  Both go under the mesh underlay; water above terrain.
- Legend rows `water N` (quads drawn) and `terrain N` (contour levels); report
  counts likewise; `scene_bounds` counts water quads in the exterior tier.

## rage-cli

- Index part `WORLD` (`world.bin`, format 13): `map_nodes: HashMap<u32, MapNode>`
  (the cache's node, last archive wins, only names that are a known `.ymap`),
  `ymap_by_name`, `ynv_by_name`, `ynd_by_name`, `bounds: Vec<BoundsItem>`
  (the cache's bounds store, by `.ybn` name), `map_hours: HashMap<u32, u32>`
  and `map_weathers: HashMap<u32, Vec<u32>>` from every manifest.
- `src/region.rs`: `assemble(region, opts, index, keys) -> Region { ymaps,
  entities (visible leaves with their archetype, position, placement), interiors,
  notes }`. The LOD tree is CodeWalker's: within a map an entity whose parent
  index names a lower-or-equal LOD level, or is an orphan, is a root; a root
  with a parent index links to its parent map's entity of that index
  (`ConnectToParent`); `LodDist <= 0` takes the archetype's, `ChildLodDist < 0`
  is half of it. From each root within `dist <= LodDist`, descend while the
  children are all present and (`dist <= ChildLodDist` or any child's `LodDist
  >= dist`); otherwise the entity is a leaf. `MaxLOD` prunes as
  `EntityVisibleAtMaxLodLevel`/`EntityChildrenVisibleAtMaxLodLevel` do.
- `plot`: `--game`, `--detail`, `--max-lod`, `--hour`, `--weather`; inputs
  optional with `--game`. Water and heightmap read once through the index's
  world file locations; contour field cut to the region plus one cell.
- README: the command row, a recipe with a picture, the index table row.

## Not done here

`watermap.dat` and `distantlights.dat` readers and the `cache_y.dat` writer
(the rest of rage-formats#10) are left open; CodeWalker's world explorer does
not draw either, and the plot needs neither. A 3D render of the region is the
issue's "later".
