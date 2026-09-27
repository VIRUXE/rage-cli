# Shared review packs

Text-only descriptions of GTA V assets, for `rage catalog pack import`. Every
legacy PC install of the same game build has byte-identical assets, so a
description written once applies to everyone's copy.

A pack holds, per asset: its identity (catalogue asset key, name, hash, the
SHA-256 of its source bytes, the game build it was reviewed on) and the
review (description, shape, material, condition, likely use, tags,
confidence, missing views, limitations, who reviewed it). It holds no
images, no archive paths and no game data.

```sh
rage catalog build
rage catalog pack import data/packs/gta5-3889-2026-09-27.json
rage catalog search blackjack table
```

On the same build, reviews match your assets by source bytes. On another
build they match by asset identity only and say so in `limitations`.
Imported reviews are stored as `shared-visual`, never replace a review made
on your own machine, and are not re-exported unless `--include-shared`.

## Packs

| File | Game build | Reviews | Reviewer | Made |
|---|---|---|---|---|
| `gta5-3889-2026-09-27.json` | 3889 (legacy PC) | 4,153 | agent: gemini-3.8-flash-low via agy (3,806), claude-sonnet-5 via Claude Code (189), gemini-3.8-flash-medium pilot (158) | 2026-09-27 |

### How the 2026-09-27 pack was made

- Props were drawables and fragments the game loads, 0.2 to 5 m across,
  with at least 50 triangles, excluding LOD, `slod` and proxy models: 47,099
  on build 3889. The pack covers a random sample of them.
- Each was rendered blind (numbered tiles, no names) from front, iso and top
  by `rage catalog sheet`, eight to a sheet, and described by a vision model
  through agy in sandbox mode, or by Claude Code subagents reading the same
  sheets. Each review names its model in `reviewer_name`. `rage catalog annotate` accepted a
  description only when it quoted the exact image hash it was given.
- The 158 pilot reviews were rendered before rage-render 0.3 fixed prop
  orientation, so some say the "front" view showed the back.
- In a graded sample, about 95% of descriptions identified the object.
  Gemini's `confidence` runs high (mean 0.89); claude-sonnet-5's is lower
  and better calibrated (mean 0.51), especially on vague car parts.
  Known weak spot: loose vehicle body parts, such as a roof panel, can be
  described as a building canopy or awning. Treat descriptions as search
  leads and check the asset before relying on it.

These descriptions are machine-written and released into the public domain
under the Unlicense, like the rest of this repository.
