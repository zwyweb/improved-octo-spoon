# BSX — Game Asset Bundle v2

Fast, obfuscated game asset bundle format for ML:EKU Studio projects.

## Features

- **ZSQOI** textures — all raster images (PNG, JPG, WebP, etc.) stored as QOI without magic-bytes, zstd compressed; always returned as RGBA8 via ZSQOI2RGBA pipeline
- **RawOpus** audio — zero-copy Ogg strip/rewrap, all formats → Ogg Opus output
- **Rayon parallel pack**, zstd-22 + LZ4 smart selection
- TOC and blob XOR obfuscation (keys assembled at runtime, no plaintext in binary)
- Optional **flat binmap** mode (single encrypted blob, ideal for Web)
- **`--fs` flag** — asset names normalized to JS Object-Literal safe keys
- Encrypted string pool (`key()` API)
- `.bmap` alias files, `.fx` sidecar effect system

## CLI

```
bsx pack  <dir> [--output <file>] [--level 1-22] [--opt] [--binmap] [--fs] [--legal "…"]
bsx depack <bundle.bsx> [--output <dir>]
bsx info   <bundle.bsx>
bsx strings <bundle.bsx> --key <key>
```

## Library (Rust)

```rust
let mut bundle = BsxBundle::open("assets.bsx")?;

// get asset → TextureRaw → RGBA8 bytes, Audio → Ogg Opus bytes, Blob → raw
let rgba = bundle.get("ui/button.png")?;
let ogg  = bundle.get("music/intro")?;

// --fs lookup
if let Some(orig) = bundle.fs_resolve("ui/button_png") {
    let data = bundle.get(orig)?;
}

// string pool
bundle.key("my_secret_key")?;
let s = bundle.string(0)?;
```

## WASM / JavaScript

```html
<script src="bsx.js"></script>
<script>
const bsx = await BSX.open(arrayBuffer);

// standard get
const rgba = await bsx.get("ui/button");   // RGBA8
const ogg  = await bsx.get("sfx/boom");    // Ogg Opus

// --fs mode
const rgba2 = await bsx.fs.ui['button.png'];
</script>
```

## demo.html

Interactive browser preview. Drop a `.bsx` bundle to inspect assets, preview
textures and audio, copy paths, download files.  
Requires `fzstd` (loaded from jsDelivr CDN) for TOC decompression.

## Texture encoding

| Source format | Stored as | Output (`get()`) |
|---|---|---|
| PNG, BMP, TGA, lossless WebP | ZSQOI (QOI, no magic-bytes, zstd) | RGBA8 |
| JPEG, lossy WebP | ZSQOI (QOI, no magic-bytes, zstd) | RGBA8 |
| `--opt` flag | ZSQOI (QOI, no magic-bytes, zstd) | RGBA8 |
| ASTC, DDS, PKM | stored raw | raw bytes |

## Format spec

See [INFO.md](INFO.md) for wire format details.
