# BSX Godot Plugin

## Setup

1. Copy `addons/bsx/` into your Godot project's `addons/` folder.
2. Enable `BSX` in **Project → Project Settings → Plugins**.

## Native builds (optional, fast zstd)

Run `build_native.sh` from the project root to compile platform binaries.
Copy the output `.so` / `.dll` / `.dylib` / `.wasm` files into
`addons/bsx/bin/`.

Without native binaries, zstd decompression falls back to spawning the
`bsx` CLI tool. Install it and ensure it's on `PATH`.

## Usage

```gdscript
var bsx := BSXBundle.new()
bsx.open("res://assets.bsx")

# Texture (returns ImageTexture; decoded via ZSQOI2RGBA pipeline)
var tex : ImageTexture = bsx.get_texture("ui/button")
$Sprite2D.texture = tex

# Audio (WAV PCM — works directly in AudioStreamPlayer)
var sfx : AudioStreamWAV = bsx.get_audio("sfx/explosion")
$AudioStreamPlayer.stream = sfx
$AudioStreamPlayer.play()

# Raw bytes (JSON, shader, binary data…)
var json_bytes : PackedByteArray = bsx.get_bytes("data/config.json")
var json : Dictionary = JSON.parse_string(json_bytes.get_string_from_utf8())

# List all assets
for name in bsx.list():
    print(name)

# String pool
bsx.unlock_strings("my_secret_key")
print(bsx.string(0))
```

## Texture pipeline

All raster images are stored internally as ZSQOI (QOI without magic-bytes, zstd
compressed) regardless of the original file extension.  `get_texture()` decodes
them transparently via the ZSQOI2RGBA pipeline and returns an `ImageTexture`.

GPU-compressed formats (ASTC, DDS, PKM) are stored raw and returned via
`get_bytes()` for manual GPU upload.

## Web export

The WASM build (`build_wasm.sh`) produces `bsx.wasm` + `bsx_wasm.js`.
These are automatically used when running a web export via the
`BSXNative.gdextension` configuration.
