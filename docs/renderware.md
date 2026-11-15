# Classic GTA assets

Development builds use the adjacent crates through the
[local-toolchain wrapper](local-toolchain.md). Parsing and writing are our own
implementations; no external RenderWare parser is vendored.

## IMG archives

```powershell
rage info D:/Games/gtasa/models/gta3.img
rage list D:/Games/gtasa/models/gta3.img '*.txd'
rage search D:/Games/gtasa/models/gta3.img '*.dff' --limit 20 --json
rage extract D:/Games/gtasa/models/gta3.img bistro.txd -o ./extracted
rage create ./assets -o ./custom.img --format img2
rage archive rebuild ./custom.img -o ./edited.img --replace ./bistro.txd
rage archive rebuild ./edited.img -o ./smaller.img --remove bistro.txd
```

IMG1 needs its paired same-stem `.dir`; `--img-dir FILE` overrides its location.
Creation accepts `--format img1`, `img2` and `img3`. IMG1 produces two files.
New IMG outputs must not already exist. Names that collide ignoring case,
non-ASCII names, and names exceeding IMG1/2's 24-byte field are rejected.
IMG containers are flat; creating one from a directory uses each file's basename
and checks collisions across subdirectories. Rebuilding copies unchanged entry
payloads and retains opaque IMG1/2 directory bytes. IMG3 native resources retain
their original headers/compression when copied; arbitrary replacement of these
resources requires a resource-aware encoder and is currently refused.

## TXD dictionaries

```powershell
rage resource info ./extracted/bistro.txd --json
rage resource info bistro.txd --archive D:/Games/gtasa/models/gta3.img --json
rage textures ./extracted/bistro.txd --sheet -o ./images
rage textures D:/Games/gtasa/models/gta3.img bistro.txd --dds -o ./dds
rage textures build ./images/*.png --game sa --format rgba8 -o ./new.txd
rage textures build ./replacement.png --from ./original.txd -o ./edited.txd
rage textures build --from ./edited.txd --remove unused -o ./smaller.txd
rage resource convert ./edited.txd --to gta5 -o ./converted
```

New TXDs require `--game iii|vc|sa`; starting from a TXD retains its original
layout and unknown chunks. III/VC presets write PC D3D8 natives, SA writes PC
D3D9 natives. Source III/VC compatibility is covered by synthetic fixtures;
the real-game suite currently uses the installed San Andreas.

PNG/JPG/WebP exports show the top mip. DDS export retains every available
source mip. Some retail dictionaries declare zero-length records at the end of
their compressed mip chains: these remain untouched in TXD rebuilds, are
reported separately by inspection, and do not become invented pixels on export.
YTD conversion may normalize such empty records away. A source chain with
nonempty tiny levels that the current YTD adapter cannot preserve fails
explicitly; DDS export still retains those levels.

DXT1 alpha-mode textures use alpha-aware decoding; conversions can emit RGBA
pixels to preserve transparency. New encodings from images use the existing
image/BC encoders; unedited source textures retain their original bytes.
PC palettes, packed 16-bit pixels, RGB/RGBA and luminance are also supported.
Console/mobile-native and cubemap raster layouts are identified as unsupported.

## Editable documents

```powershell
rage resource dump ./original.txd -o ./texture.json
rage resource build ./texture.json -o ./rebuilt.txd
```

The JSON schema is `rage-renderware/1`. `format` selects the source format;
`chunks` retain original kind/version words and opaque hexadecimal payloads;
`padding` retains zero sector padding. Known TXD dictionaries also expose a
`textures` array with names, masks, sampler flags, raster fields, a hexadecimal
palette and a hexadecimal byte string per mip level. Edit those semantic fields
to rename textures, change pixels or change supported raster settings. Dimensions,
depth, formats and mip byte counts must agree; the builder checks them.

`source_index` identifies the original texture even after renaming. Removing an
entry from `textures` removes it from the dictionary. Add image/DDS textures with
`textures build --from`. Semantic texture fields take precedence over the raw
native payload for that texture, so avoid competing edits to both representations.
Unknown fields are rejected rather than silently ignored. Raw chunk editing is
an expert interface; structural validation does not prove an arbitrary plugin
payload is semantically valid.

## Private game validation

```powershell
$env:GTASA_PATH = 'D:/Games/gtasa'
./scripts/with-local-toolchain.ps1 test --test gtasa '--' --nocapture
```

The opt-in suite reads the install without modifying it and writes its report
under `target/gtasa`. Generated reports distinguish source parse/decode/rebuild
failures, absent mip records and target conversion layout failures. Extracted
and generated game data must stay outside commits.
