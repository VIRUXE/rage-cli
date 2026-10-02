# AGENTS.md

Guide for an AI agent using `rage catalog` to find GTA V assets by what they
look like, not what they're named. Inspired by
[GTA Scout's AGENTS.md](https://github.com/Dryxio/gta-scout).

The catalogue indexes every drawable and texture in the user's own install.
Names are opaque (`Prop_ChairPlastic01a`, hashes, DLC-pack shorthand), so a
name search only gets you so far. Visual review turns those opaque entries
into searchable descriptions — shape, material, condition, likely use — so
later searches (by yourself or anyone else) can find "a cracked porcelain
sink" instead of guessing at `v_ilev_sink_02`. You are the reviewer.

## Start

Set `GTAV_PATH` (or pass `--exe`) so keys load — `catalog build` needs the
game's decryption keys to read most archives:

```sh
export GTAV_PATH="<path to GTA V>"
rage catalog build
```

`build` is incremental; a cold run over a full retail install takes a few
minutes. Check it worked before relying on it:

```sh
rage catalog info          # row/winner counts per kind, parse failures
rage catalog info --json   # "report": catalog-report.json beside the database lists parse failures
```

A metadata build is not a visual review. `catalog build` tells you sizes,
kinds, DLC packs and texture resolutions — it does not tell you what
anything looks like. That only happens in the review loop below.

## Search

```sh
rage catalog search WORDS [--kind model|tex|ydr|ydd|yft|ytd] \
    [--min-size N|X,Y,Z] [--max-size ...] [--dlc PACK|base|update|dlc] \
    [--annotated] [--all-copies] --json
```

Words are ANDed prefixes matched over names (split on `_`, camelCase and
digits), archive paths, and any review notes attached by `annotate`. Sizes
are metres for models, pixels for textures. Only the winner — the copy the
game actually loads — is returned unless `--all-copies` is given.

`--json` output follows schema `rage-catalog-search/1`:

- `results[]`: `key`, `kind`, `name`, `archive{path, rel, tier, dlc_pack,
  nested[]}`, `bounds{size}`, `texture{width, height, format}`,
  `annotations[]`, `commands{screenshot, textures, info, get}`
- `coverage{results, results_annotated, catalogue_items, catalogue_annotated}`

Try several phrasings and related categories before concluding an asset
doesn't exist — "sink", "basin", "washbasin" can each turn up different
hits. Absence from a search is not proof the asset is missing; check
`coverage` to see how much of the catalogue has been reviewed at all before
trusting a negative result. Use the `commands` lines exactly as given rather
than reconstructing paths yourself. A hit inside a nested archive needs
`rage catalog get KEY -o DIR --rpf` first — the `get` command line is
provided in `commands` — before `screenshot` or `textures` can open it.

## Review loop

1. Generate contact sheets over a search, a saved result set, or one key:

   ```sh
   rage catalog sheet WORDS [filters] -o DIR --views front,iso,top \
       --per-sheet 16 --limit 64
   # or: --ids results.json    (a saved search)
   # or: --key KEY             (one asset)
   ```

   Writes `sheet-001.png`, `sheet-002.png`, ..., `packet.json`, and
   `responses.template.json`. Tiles show only `#N` and view names — names
   are withheld so review is judged on appearance, not a filename hint.

2. Look at each sheet image with your image-viewing capability. Fill in a
   copy of `responses.template.json`, one entry per tile:

   - `description` (required)
   - `shape`, `material`, `condition`, `likely_use`
   - `tags` (array)
   - `confidence` (0..1)
   - `orientation_doubt` (bool)
   - `missing_views` (array)
   - `limitations`

   Keep each tile's `sha256` exactly as given. Leave `description` empty for
   any tile you can't judge — empty tiles are skipped on import, not
   errors. Set `"reviewer": "agent"` and `"reviewer_name"` to your model
   name.

3. Import the review:

   ```sh
   rage catalog annotate --packet DIR/packet.json --responses DIR/responses.json \
       --verify-files [--strict] [--json]
   ```

   Refused on: a wrong `sha256`, an unknown tile, a packet not in this
   catalogue, or (with `--verify-files`) a sheet file that changed on disk
   since it was written. A newer review of the same asset by the same kind
   of reviewer (agent vs. human) supersedes the old one; re-importing
   identical responses counts as a duplicate, not a second review.

4. Search again — descriptions are searchable immediately. Add
   `--annotated` to narrow to items that have been reviewed.

5. `rage catalog sheet --reveal PACKET_ID` maps tile numbers back to real
   asset names, for a human to check your work afterwards. Don't use
   `--reveal` before you've reviewed the tiles — it defeats the point of a
   blind sheet.

### `packet.json` (schema `rage-catalog-packet/1`, trimmed)

```json
{
  "schema": "rage-catalog-packet/1",
  "packet_id": "pk_0123456789abcdef",
  "views": ["front", "iso", "top"],
  "cell": { "width": 256, "height": 256 },
  "sheets": [
    {
      "file": "sheet-001.png",
      "sha256": "...",
      "columns": 4,
      "rows": 4,
      "tiles": [
        {
          "tile": 1,
          "col": 0,
          "row": 0,
          "kind": "model",
          "views": ["front", "iso", "top"],
          "sha256": "...",
          "size_m": [0.8, 0.6, 1.1],
          "render": { "missing_textures": 0, "lod": "high" }
        }
      ]
    }
  ],
  "instructions": "...",
  "response_schema": "rage-catalog-responses/1"
}
```

### `responses.json` (schema `rage-catalog-responses/1`, trimmed)

```json
{
  "schema": "rage-catalog-responses/1",
  "packet_id": "pk_0123456789abcdef",
  "reviewer": "agent",
  "reviewer_name": "claude-sonnet-5",
  "tiles": [
    {
      "tile": 1,
      "sha256": "...",
      "description": "wooden bar stool, round seat, four splayed legs",
      "shape": "stool",
      "material": "wood",
      "condition": "worn",
      "likely_use": "bar/restaurant interior prop",
      "tags": ["furniture", "seating", "wood"],
      "confidence": 0.85,
      "orientation_doubt": false,
      "missing_views": [],
      "limitations": ""
    }
  ]
}
```

## Rules

- Describe only what is visible in the render. Never infer shape or
  material from the filename, hash, or archive path, and never guess at a
  side you can't see.
- Never mark an AI review as `"reviewer": "human"`, and never let an AI
  review pass as human-verified.
- Never fabricate or pad reviews to raise coverage numbers. An empty
  `description` is correct when you genuinely can't judge a tile.
- Report `missing_views` and `orientation_doubt` honestly rather than
  filling in a confident-sounding guess.
- A render with `missing_textures > 0` may show plain grey surfaces that
  aren't actually grey in-game. Say so in `limitations`, don't describe the
  object itself as grey.
- Check `bounds.size` before using an asset in a scene — a "chair" that's
  1.8 m tall is probably not a chair prop.
- When reporting results to whoever asked, record which candidates you used
  and which you rejected, and why.

## Over MCP

`rage mcp` serves the same loop as tools to an MCP client, so none of the
commands above has to be spelled out or parsed: `catalog_info`,
`catalog_search`, `catalog_get`, `catalog_sheet`, `catalog_reveal`,
`catalog_annotate`, `screenshot` and `resource_info`, each returning the
object the command's `--json` prints. Register it once:

```sh
claude mcp add rage -s user -- rage mcp
```

`catalog_sheet` and `screenshot` take `inline_images: true` to hand you the
pictures directly; `catalog_annotate` takes `responses_json` (the filled-in
responses object) instead of a file path. The rules below apply unchanged,
and the server repeats them to you at `initialize`.

## Sharing

- `rage catalog pack export -o FILE` writes text-only annotations (no
  images, no game data); `rage catalog pack import FILE` matches them to
  another install by asset identity and source hash. Reviews imported this
  way land as `shared-visual` — treat them as leads, not as verified for
  the local install. See `--help` for flags.
- `rage catalog embed` plus `search --semantic|--hybrid` rank results by
  meaning rather than exact word match, when `rage` was built with
  `--features semantic`. See `--help` for flags.
