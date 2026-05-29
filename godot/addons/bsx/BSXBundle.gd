## BSXBundle — ML:EKU Studio asset bundle loader.
##
## Usage:
##   var bsx := BSXBundle.new()
##   bsx.open("res://assets.bsx")
##
##   # Get texture (returns ImageTexture, or null on error)
##   var tex := bsx.get_texture("ui/button")
##
##   # Get audio (returns AudioStreamWAV)
##   var sfx := bsx.get_audio("sfx/boom")
##
##   # Get raw bytes (PackedByteArray)
##   var json_bytes := bsx.get_bytes("data/config.json")
##
##   # List all assets
##   var names := bsx.list()
##
##   # String pool
##   bsx.unlock_strings("my_key")
##   var s := bsx.string(0)

class_name BSXBundle
extends Resource

# ─── Constants ────────────────────────────────────────────────────────────────

const MAGIC      := PackedByteArray([0x42, 0x53, 0x58, 0x01])  # "BSX\x01"
const HEADER_SZ  := 32

# ─── Private state ────────────────────────────────────────────────────────────

var _data   : PackedByteArray
var _toc    : Dictionary   # parsed TOC
var _strings: Array[String]

# ─── Public API ───────────────────────────────────────────────────────────────

## Load a .bsx file. Returns OK or an error code.
func open(path: String) -> Error:
    var f := FileAccess.open(path, FileAccess.READ)
    if not f:
        push_error("BSXBundle: cannot open '%s'" % path)
        return ERR_FILE_CANT_OPEN
    _data = f.get_buffer(f.get_length())
    f.close()
    return _parse_toc()

## Load from a PackedByteArray (useful with ResourceLoader or HTTP).
func open_bytes(bytes: PackedByteArray) -> Error:
    _data = bytes
    return _parse_toc()

## Returns an ImageTexture for a stored texture asset, null on error.
func get_texture(asset_path: String) -> ImageTexture:
    var raw := get_bytes(asset_path)
    if raw.is_empty():
        return null
    var img := Image.new()
    var err : Error
    # QOI (current ZSQOI2RGBA pipeline) — magic "qoif" = 0x71 0x6F 0x69 0x66
    if raw.size() >= 4 and raw[0] == 0x71 and raw[1] == 0x6F and raw[2] == 0x69 and raw[3] == 0x66:
        err = img.load_qoi_from_buffer(raw)
        if err == OK:
            return ImageTexture.create_from_image(img)
        push_error("BSXBundle: QOI decode failed for '%s'" % asset_path)
        return null
    # Legacy guard: bsx ≤ 2.0 stored raw KTX2 bytes as TextureRaw; re-pack required.
    if raw.size() >= 12 and raw.slice(0, 4) == PackedByteArray([0xAB, 0x4B, 0x54, 0x58]):
        push_warning("BSXBundle: legacy KTX2 payload in '%s' — re-pack with bsx ≥ 2.1 to migrate to QOI." % asset_path)
        return null
    # PNG fallback (very old bundles)
    err = img.load_png_from_buffer(raw)
    if err != OK:
        err = img.load_webp_from_buffer(raw)
    if err != OK:
        push_error("BSXBundle: failed to decode image '%s'" % asset_path)
        return null
    return ImageTexture.create_from_image(img)

## Returns an AudioStreamWAV for a stored audio track, null on error.
func get_audio(asset_path: String) -> AudioStreamWAV:
    var raw := get_bytes(asset_path)
    if raw.is_empty():
        return null
    # WAV: RIFF....WAVE
    if raw.size() >= 12 \
       and raw[0] == 0x52 and raw[1] == 0x49 and raw[2] == 0x46 and raw[3] == 0x46 \
       and raw[8] == 0x57 and raw[9] == 0x41 and raw[10] == 0x56 and raw[11] == 0x45:
        return _parse_wav(raw)
    push_error("BSXBundle: '%s' is not WAV PCM" % asset_path)
    return null

## Returns raw bytes for any asset. Handles decompression.
func get_bytes(asset_path: String) -> PackedByteArray:
    if _toc.is_empty():
        push_error("BSXBundle: not loaded")
        return PackedByteArray()
    var result := _lookup_asset(asset_path)
    if result.is_empty():
        push_error("BSXBundle: '%s' not found" % asset_path)
    return result

## Returns all asset paths in the bundle.
func list() -> Array[String]:
    var out : Array[String] = []
    if _toc.has("assets"):
        for e in _toc["assets"]:
            out.append(e["name"])
    if _toc.has("tracks"):
        for t in _toc["tracks"]:
            out.append(t["path"])
    if _toc.has("binmap") and _toc["binmap"] != null:
        var bm : Dictionary = _toc["binmap"]
        if bm.has("assets"):
            for e in bm["assets"]: out.append(e["name"])
        if bm.has("tracks"):
            for t in bm["tracks"]: out.append(t["path"])
    return out

## Check if an asset exists.
func has(asset_path: String) -> bool:
    return not _lookup_asset(asset_path).is_empty()

## Decrypt and load the string pool.
func unlock_strings(key: String) -> void:
    if not _toc.has("strings") or _toc["strings"] == null:
        push_warning("BSXBundle: no string pool in this bundle")
        return
    var desc : Dictionary = _toc["strings"]
    var off  : int = HEADER_SZ + int(desc["offset"])
    var sz   : int = int(desc["size"])
    if off + sz > _data.size():
        push_error("BSXBundle: string pool OOB")
        return
    var enc := _data.slice(off, off + sz)
    _strings = _decode_string_pool(enc, key)

## Get string by index (call unlock_strings first).
func string(idx: int) -> String:
    if idx < 0 or idx >= _strings.size():
        push_error("BSXBundle: string index %d out of range" % idx)
        return ""
    return _strings[idx]

func string_count() -> int:
    return _strings.size()

# ─── Private: TOC parsing ─────────────────────────────────────────────────────

# TOC XOR key (mirrors Rust toc_xor_key())
func _toc_xor_key() -> PackedByteArray:
    var K := PackedByteArray([0x8B,0xF8,0xEB,0xBA,0x9F,0xAB,0xEF,0xEB,
                               0x5A,0xA7,0x50,0x64,0x54,0x47,0x70,0x70])
    var masks := PackedByteArray([0x55,0x55,0x55,0x55,0x55,0x55,0x55,0x55,
                                   0xAA,0xAA,0xAA,0xAA,0xAA,0xAA,0xAA,0xAA])
    var out := PackedByteArray()
    out.resize(16)
    for i in 16:
        out[i] = K[i] ^ masks[i]
    return out

func _toc_xor(data: PackedByteArray) -> PackedByteArray:
    var key := _toc_xor_key()
    var out  := data.duplicate()
    for i in out.size():
        out[i] = out[i] ^ key[i % 16]
    return out

func _parse_toc() -> Error:
    if _data.size() < HEADER_SZ:
        push_error("BSXBundle: file too small")
        return ERR_FILE_CORRUPT
    for i in 4:
        if _data[i] != MAGIC[i]:
            push_error("BSXBundle: bad magic")
            return ERR_FILE_CORRUPT

    # Read toc_offset (u64 LE at bytes 8..16) and toc_size (u32 LE at 16..20)
    var toc_off : int = _read_u64(_data, 8)
    var toc_sz  : int = _read_u32(_data, 16)
    if toc_off + toc_sz > _data.size():
        push_error("BSXBundle: TOC OOB")
        return ERR_FILE_CORRUPT

    var toc_xored   := _data.slice(toc_off, toc_off + toc_sz)
    var toc_stripped := _toc_xor(toc_xored)

    # zstd decompress (Godot has no built-in zstd, use FileAccess workaround or tmp file)
    var toc_raw := _zstd_decompress(toc_stripped)
    if toc_raw.is_empty():
        push_error("BSXBundle: TOC decompression failed")
        return ERR_FILE_CORRUPT

    # Parse bincode TOC
    _toc = _parse_bincode_toc(toc_raw)
    if _toc.is_empty():
        push_error("BSXBundle: TOC parse failed")
        return ERR_FILE_CORRUPT
    return OK

# Godot doesn't have native zstd. We use a temp file + external bsx CLI if available,
# or fall back to the GDExtension native lib.
func _zstd_decompress(stripped: PackedByteArray) -> PackedByteArray:
    # Re-prepend zstd magic
    var magic := PackedByteArray([0x28, 0xB5, 0x2F, 0xFD])
    var full  := magic + stripped

    # Try GDExtension native path first
    if ClassDB.class_exists("BSXNative"):
        return BSXNative.zstd_decompress(full)

    # Fallback: write to tmp, call bsx CLI
    var tmp_in  := "user://bsx_toc_in.zst"
    var tmp_out := "user://bsx_toc_out.bin"
    var f := FileAccess.open(tmp_in, FileAccess.WRITE)
    if not f: return PackedByteArray()
    f.store_buffer(full); f.close()

    var os_name := OS.get_name()
    var cli     := "bsx"
    if os_name == "Windows": cli = "bsx.exe"
    var ret := OS.execute(cli, ["zstd-d", tmp_in, tmp_out])
    if ret != 0:
        push_error("BSXBundle: zstd decompression failed. Install bsx CLI or BSXNative GDExtension.")
        return PackedByteArray()

    var fo := FileAccess.open(tmp_out, FileAccess.READ)
    if not fo: return PackedByteArray()
    var out := fo.get_buffer(fo.get_length()); fo.close()
    DirAccess.remove_absolute(ProjectSettings.globalize_path(tmp_in))
    DirAccess.remove_absolute(ProjectSettings.globalize_path(tmp_out))
    return out

# ─── Private: bincode TOC reader ─────────────────────────────────────────────

func _parse_bincode_toc(raw: PackedByteArray) -> Dictionary:
    var pos : int = 0

    var n_assets := _rb_u64(raw, pos); pos += 8
    var assets   : Array[Dictionary] = []
    for _i in n_assets:
        var name  := _rb_str(raw, pos); pos = name[1]
        var kind  := _rb_kind(raw, pos); pos = kind[1]
        var off   := _rb_u64(raw, pos); pos += 8
        var psz   := _rb_u64(raw, pos); pos += 8
        var osz   := _rb_u64(raw, pos); pos += 8
        var pack  := _rb_packing(raw, pos); pos = pack[1]
        var crc32 := _rb_u32(raw, pos); pos += 4
        assets.append({"name": name[0], "kind": kind[0], "offset": off,
            "packed_size": psz, "orig_size": osz, "packing": pack[0], "crc32": crc32})

    var n_tracks := _rb_u64(raw, pos); pos += 8
    var tracks   : Array[Dictionary] = []
    for _i in n_tracks:
        var path  := _rb_str(raw, pos); pos = path[1]
        var off   := _rb_u64(raw, pos); pos += 8
        var sz    := _rb_u64(raw, pos); pos += 8
        var ch    := raw[pos]; pos += 1
        var sr    := _rb_u32(raw, pos); pos += 4
        var tot   := _rb_u64(raw, pos); pos += 8
        var kbps  := _rb_u32(raw, pos); pos += 4
        # skip FxParams (serde flatten, default fields — skip with 0 approach)
        pos = _skip_fx(raw, pos)
        tracks.append({"path": path[0], "offset": off, "size": sz,
            "channels": ch, "sample_rate": sr, "total_samples": tot, "bitrate_kbps": kbps})

    # strings Option
    var str_tag := raw[pos]; pos += 1
    var strings_desc = null
    if str_tag == 1:
        var so  := _rb_u64(raw, pos); pos += 8
        var ss  := _rb_u64(raw, pos); pos += 8
        var sc  := _rb_u32(raw, pos); pos += 4
        strings_desc = {"offset": so, "size": ss, "count": sc}

    # bmap HashMap (skip for now — not needed for runtime get())
    var bmap_count := _rb_u64(raw, pos); pos += 8
    for _i in bmap_count:
        var _k := _rb_str(raw, pos); pos = _k[1]
        var inner := _rb_u64(raw, pos); pos += 8
        for _j in inner:
            var _ik := _rb_str(raw, pos); pos = _ik[1]
            var _iv := _rb_str(raw, pos); pos = _iv[1]

    # binmap Option
    var bm_tag := raw[pos]; pos += 1
    var binmap = null
    if bm_tag == 1:
        var n_bma := _rb_u64(raw, pos); pos += 8
        var bm_assets : Array[Dictionary] = []
        for _i in n_bma:
            var name := _rb_str(raw, pos); pos = name[1]
            var kind := _rb_kind(raw, pos); pos = kind[1]
            var off  := _rb_u64(raw, pos); pos += 8
            var sz   := _rb_u64(raw, pos); pos += 8
            var osz  := _rb_u64(raw, pos); pos += 8
            bm_assets.append({"name": name[0], "kind": kind[0], "offset": off, "size": sz, "orig_size": osz})
        var n_bmt := _rb_u64(raw, pos); pos += 8
        var bm_tracks : Array[Dictionary] = []
        for _i in n_bmt:
            var path := _rb_str(raw, pos); pos = path[1]
            var off  := _rb_u64(raw, pos); pos += 8
            var sz   := _rb_u64(raw, pos); pos += 8
            var ch   := raw[pos]; pos += 1
            var sr   := _rb_u32(raw, pos); pos += 4
            var tot  := _rb_u64(raw, pos); pos += 8
            var kbps := _rb_u32(raw, pos); pos += 4
            pos = _skip_fx(raw, pos)
            bm_tracks.append({"path": path[0], "offset": off, "size": sz,
                "channels": ch, "sample_rate": sr, "total_samples": tot, "bitrate_kbps": kbps})
        var blob_start := _rb_u64(raw, pos); pos += 8
        var blob_size  := _rb_u64(raw, pos); pos += 8
        var bs_tag := raw[pos]; pos += 1
        var bm_strings = null
        if bs_tag == 1:
            var so := _rb_u64(raw, pos); pos += 8
            var ss := _rb_u64(raw, pos); pos += 8
            var sc := _rb_u32(raw, pos); pos += 4
            bm_strings = {"offset": so, "size": ss, "count": sc}
        binmap = {"assets": bm_assets, "tracks": bm_tracks,
                  "blob_start": blob_start, "blob_size": blob_size, "strings": bm_strings}

    return {"assets": assets, "tracks": tracks, "strings": strings_desc, "binmap": binmap}

# FxParams bincode skip — all fields are Option<FxRange> (optional u8 + 2×f32) + Option<u32>
# 9 × Option<FxRange> (each: 1B tag + 8B f32×2) + 1 × Option<u32> (1B tag + 4B)
func _skip_fx(raw: PackedByteArray, pos: int) -> int:
    for _i in 9:  # pitch, speed, spitch, volume_db, pan, lowpass_hz, highpass_hz, reverb_wet, start_offset
        var tag := raw[pos]; pos += 1
        if tag == 1: pos += 8  # min: f32 + max: f32
    var tag := raw[pos]; pos += 1  # bitrate_kbps Option<u32>
    if tag == 1: pos += 4
    return pos

# ─── Private: bincode primitives ─────────────────────────────────────────────

func _rb_u32(b: PackedByteArray, off: int) -> int:
    return b[off] | (b[off+1] << 8) | (b[off+2] << 16) | (b[off+3] << 24)

func _rb_u64(b: PackedByteArray, off: int) -> int:
    var lo : int = _rb_u32(b, off)
    var hi : int = _rb_u32(b, off + 4)
    return hi * 4294967296 + lo

func _rb_str(b: PackedByteArray, pos: int) -> Array:  # [String, new_pos]
    var len : int = _rb_u64(b, pos); pos += 8
    var s   := b.slice(pos, pos + len).get_string_from_utf8()
    return [s, pos + len]

func _rb_kind(b: PackedByteArray, pos: int) -> Array:  # [Dictionary, new_pos]
    var variant := _rb_u32(b, pos); pos += 4
    match variant:
        0:  # TextureRaw
            var w := _rb_u32(b, pos); var h := _rb_u32(b, pos+4); pos += 8
            return [{"type": "tex_raw", "w": w, "h": h}, pos]
        1:  # TextureGpu
            var w := _rb_u32(b, pos); var h := _rb_u32(b, pos+4); pos += 8
            return [{"type": "tex_gpu", "w": w, "h": h}, pos]
        2:  # Audio
            var codec := _rb_str(b, pos); pos = codec[1]
            var sr    := _rb_u32(b, pos); pos += 4
            var ch    := b[pos];          pos += 1
            var dur   := _rb_u32(b, pos); pos += 4
            return [{"type": "audio", "codec": codec[0], "sr": sr, "ch": ch, "dur": dur}, pos]
        _:  # Blob
            return [{"type": "blob"}, pos]

func _rb_packing(b: PackedByteArray, pos: int) -> Array:  # [String, new_pos]
    var v := _rb_u32(b, pos)
    return [["raw", "lz4", "zstd"][v] if v < 3 else "raw", pos + 4]

# ─── Private: asset lookup ────────────────────────────────────────────────────

func _lookup_asset(path: String) -> PackedByteArray:
    # Binmap mode
    if _toc.has("binmap") and _toc["binmap"] != null:
        var bm : Dictionary = _toc["binmap"]
        for e : Dictionary in bm.get("assets", []):
            if e["name"] == path:
                return _read_binmap_bytes(bm, e["offset"], e["size"])
        var bare := _strip_audio_ext(path)
        for t : Dictionary in bm.get("tracks", []):
            if t["path"] == bare or t["path"] == path:
                return _read_binmap_bytes(bm, t["offset"], t["size"])

    # Normal TOC
    var bare := _strip_audio_ext(path)
    for t : Dictionary in _toc.get("tracks", []):
        if t["path"] == bare or t["path"] == path:
            return _read_track(t)
    var ep := path.trim_suffix(".png")
    for e : Dictionary in _toc.get("assets", []):
        if e["name"] == ep or e["name"] == path:
            return _read_asset(e)
    return PackedByteArray()

func _read_asset(e: Dictionary) -> PackedByteArray:
    var off  : int = HEADER_SZ + int(e["offset"])
    var psz  : int = int(e["packed_size"])
    if off + psz > _data.size(): return PackedByteArray()
    var packed := _data.slice(off, off + psz)
    match e["packing"]:
        "lz4":  return _lz4_decompress(packed)
        "zstd": return _zstd_decompress(packed)
        _:      return packed

func _read_track(t: Dictionary) -> PackedByteArray:
    var off : int = HEADER_SZ + int(t["offset"])
    var sz  : int = int(t["size"])
    if off + sz > _data.size(): return PackedByteArray()
    return _data.slice(off, off + sz)  # already WAV PCM

func _read_binmap_bytes(bm: Dictionary, off_in_blob: int, size: int) -> PackedByteArray:
    var blob_start : int = int(bm["blob_start"])
    var abs        : int = blob_start + off_in_blob
    if abs + size > _data.size(): return PackedByteArray()
    var raw := _data.slice(abs, abs + size)
    return _blob_xor(raw, off_in_blob)

# ─── Private: XOR / crypto ────────────────────────────────────────────────────

func _blob_xor_key() -> PackedByteArray:
    var K     := PackedByteArray([0x40,0xF1,0xAC,0x29,0x7D,0xE6,0xB4,0x08,
                                   0x3C,0xAE,0x60,0x94,0xD5,0x2B,0xE1,0x78])
    var masks := PackedByteArray([0x33,0x33,0x33,0x33,0x33,0x33,0x33,0x33,
                                   0xCC,0xCC,0xCC,0xCC,0xCC,0xCC,0xCC,0xCC])
    var out := K.duplicate()
    for i in 16:
        out[i] = K[i] ^ masks[i]
    return out

func _blob_xor(data: PackedByteArray, blob_off: int) -> PackedByteArray:
    var key := _blob_xor_key()
    var out  := data.duplicate()
    for i in out.size():
        out[i] = out[i] ^ key[(i + blob_off) % 16]
    return out

# ─── Private: LZ4 decompressor (pure GDScript) ───────────────────────────────

func _lz4_decompress(data: PackedByteArray) -> PackedByteArray:
    if data.size() < 4: return PackedByteArray()
    var skey := PackedByteArray([0xBE, 0xEF, 0xCA, 0xFE])
    var orig_sz : int = (data[0] ^ skey[0]) \
        | ((data[1] ^ skey[1]) << 8) \
        | ((data[2] ^ skey[2]) << 16) \
        | ((data[3] ^ skey[3]) << 24)
    var bkey := PackedByteArray([0xA3,0x5F,0xC1,0x77,0x2B,0x98,0xE4,0x0D])
    var block := data.slice(4)
    for i in block.size():
        block[i] = block[i] ^ bkey[i % 8]
    var out := PackedByteArray(); out.resize(orig_sz)
    var ip : int = 0; var op : int = 0
    while ip < block.size():
        var token   : int = block[ip]; ip += 1
        var lit_len : int = (token >> 4) & 0xF
        if lit_len == 15:
            var extra : int = block[ip]; ip += 1
            while extra == 255:
                lit_len += extra; extra = block[ip]; ip += 1
            lit_len += extra
        for _l in lit_len:
            if op < orig_sz: out[op] = block[ip]; op += 1; ip += 1
        if ip >= block.size(): break
        var offset : int = block[ip] | (block[ip+1] << 8); ip += 2
        var match_len : int = (token & 0xF) + 4
        if (token & 0xF) == 15:
            var extra : int = block[ip]; ip += 1
            while extra == 255:
                match_len += extra; extra = block[ip]; ip += 1
            match_len += extra
        var src : int = op - offset
        for _m in match_len:
            if op < orig_sz: out[op] = out[src]; op += 1; src += 1
    return out.slice(0, op)

# ─── Private: WAV parse → AudioStreamWAV ─────────────────────────────────────

func _parse_wav(data: PackedByteArray) -> AudioStreamWAV:
    if data.size() < 44: return null
    var channels   : int = data[22] | (data[23] << 8)
    var sample_rate: int = data[24] | (data[25]<<8) | (data[26]<<16) | (data[27]<<24)
    var audio := AudioStreamWAV.new()
    audio.data        = data
    audio.format      = AudioStreamWAV.FORMAT_16_BITS
    audio.stereo      = (channels == 2)
    audio.mix_rate    = sample_rate
    audio.loop_mode   = AudioStreamWAV.LOOP_DISABLED
    return audio

# ─── Private: string pool ────────────────────────────────────────────────────

const _POOL_SHIFT : int = 0xBD

func _decode_string_pool(enc: PackedByteArray, key: String) -> Array[String]:
    var key_bytes := key.to_utf8_buffer()
    var kl        := key_bytes.size()
    # XOR
    var plain := enc.duplicate()
    for i in plain.size():
        var k : int = key_bytes[i % kl] if kl > 0 else 0
        plain[i] = enc[i] ^ k ^ _POOL_SHIFT
    # zstd decompress
    plain = _zstd_decompress(plain)
    if plain.is_empty(): return []
    # flatbuffer layout: u32 count + u32 offsets[] + data
    var count : int = plain[0] | (plain[1]<<8) | (plain[2]<<16) | (plain[3]<<24)
    var result : Array[String] = []
    for i in count:
        var base : int = 4 + i * 4
        var off  : int = plain[base] | (plain[base+1]<<8) | (plain[base+2]<<16) | (plain[base+3]<<24)
        var end_off : int
        if i + 1 < count:
            var nb : int = base + 4
            end_off = plain[nb] | (plain[nb+1]<<8) | (plain[nb+2]<<16) | (plain[nb+3]<<24)
        else:
            end_off = plain.size() - (4 + count * 4)
        var data_base : int = 4 + count * 4
        result.append(plain.slice(data_base + off, data_base + end_off).get_string_from_utf8())
    return result

# ─── Helpers ─────────────────────────────────────────────────────────────────

func _read_u64(b: PackedByteArray, off: int) -> int:
    return _rb_u64(b, off)

func _read_u32(b: PackedByteArray, off: int) -> int:
    return _rb_u32(b, off)

func _strip_audio_ext(path: String) -> String:
    var exts := ["wav","mp3","ogg","opus","flac","aac","m4a"]
    for e in exts:
        if path.ends_with("." + e):
            return path.trim_suffix("." + e)
    return path
