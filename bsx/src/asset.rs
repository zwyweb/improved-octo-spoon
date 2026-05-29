//! Asset packing and reconstruction.
//!
//! ## Texture storage strategy
//!
//! All raster images are stored as ZSQOI (QOI without magic-bytes and zstd/lz4 compressed)
//! internally. get() always returns:
//!   TextureRaw → raw RGBA8 bytes (w×h×4, row-major)  ← always RGBA8 regardless of storage
//!   TextureGpu → raw ASTC / DDS bytes as-is
//!   Audio      → Ogg Opus bytes
//!   Blob       → raw bytes
//!
//! ## Channel storage
//!
//! QOI channels are chosen by ChannelMode (see format.rs):
//!   Auto      — RGB if source has no alpha, RGBA if it does
//!   ForceRgba — always RGBA
//!   ForceRgb  — always RGB (alpha discarded)
//!
//! On decode, RGB QOI payloads are transparently expanded to RGBA8
//! (alpha = 255) so callers always receive w×h×4 bytes.

use crate::format::{AssetKind, ChannelMode, Packing, Preset};
#[cfg(feature = "full")]
use image::GenericImageView;

// ─── Extension lists ──────────────────────────────────────────────────────────

pub const EXT_TEX_RAW: &[&str] = &["png","jpg","jpeg","webp","bmp","tiff","tga","gif"];
pub const EXT_TEX_GPU: &[&str] = &["astc","dds","pvr","pkm"];
pub const EXT_AUDIO:   &[&str] = &["wav","mp3","ogg","opus","flac","aac","m4a","xm","mod","it","s3m"];
pub const EXT_MESH:    &[&str] = &["glb","gltf"];
pub const EXT_TEXT:    &[&str] = &[
    "svg","json","xml","html","htm","css","js","ts",
    "glsl","hlsl","wgsl","vert","frag","comp","metal",
    "lua","py","toml","yaml","yml","ini","cfg","txt","md","csv",
];

pub fn file_ext(p: &std::path::Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
}

pub fn classify(p: &std::path::Path) -> &'static str {
    let e = file_ext(p);
    if EXT_TEX_RAW.contains(&e.as_str()) { return "tex_raw"; }
    if EXT_TEX_GPU.contains(&e.as_str()) { return "tex_gpu"; }
    if EXT_AUDIO.contains(&e.as_str())   { return "audio"; }
    if EXT_MESH.contains(&e.as_str())    { return "mesh"; }
    if EXT_TEXT.contains(&e.as_str())    { return "text"; }
    "blob"
}

// ─── Packed asset ─────────────────────────────────────────────────────────────

pub struct PackedAsset {
    pub kind:      AssetKind,
    pub payload:   Vec<u8>,
    pub packing:   Packing,
    pub orig_size: u64,
}

// ─── Compression with preset ──────────────────────────────────────────────────

#[cfg(feature = "full")]
pub fn compress_with_preset(data: &[u8], preset: Preset) -> (Vec<u8>, Packing) {
    if preset.is_lz4() {
        return (crate::stripped_lz4::encode(data), Packing::Lz4);
    }
    let level = preset.zstd_level();
    match crate::stripped_zstd::encode(data, level) {
        Ok(c) if c.len() < data.len() => (c, Packing::Zstd),
        _                             => (data.to_vec(), Packing::Raw),
    }
}

// ─── Pack entry point ─────────────────────────────────────────────────────────

#[cfg(feature = "full")]
pub fn pack_asset(raw: &[u8], class: &str, name: &str,
                  preset: Preset, channel_mode: ChannelMode,
                  mesh_opt: bool) -> PackedAsset {
    let orig_size = raw.len() as u64;
    let (kind, payload, packing) = match class {
        "tex_raw" => pack_texture_raw(raw, name, preset, channel_mode),
        "tex_gpu" => pack_texture_gpu(raw),
        "audio"   => pack_audio(raw, name),
        "mesh"    => pack_mesh(raw, preset, mesh_opt),
        "text"    => pack_text(raw, preset),
        _         => pack_blob(raw, preset),
    };
    PackedAsset { kind, payload, packing, orig_size }
}

// ─── QOI pack ─────────────────────────────────────────────────────────────────

#[cfg(feature = "full")]
fn pack_texture_raw(raw: &[u8], name: &str, preset: Preset, channel_mode: ChannelMode)
    -> (AssetKind, Vec<u8>, Packing)
{
    let img = match image::load_from_memory(raw) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("  warn: can't decode {name}: {e} — storing as blob");
            let (p, pk) = compress_with_preset(raw, preset);
            return (AssetKind::Blob, p, pk);
        }
    };
    let (w, h) = img.dimensions();

    // Decide channel count.
    //   Auto      → RGB if source has no alpha, RGBA if it does.
    //   ForceRgb  → always 3 channels (alpha discarded).
    //   ForceRgba → always 4 channels.
    //
    // qoi::encode_to_vec infers channels from data.len() / (w * h):
    //   3 bytes/px → RGB, 4 bytes/px → RGBA.
    let use_rgba = match channel_mode {
        ChannelMode::ForceRgba => true,
        ChannelMode::ForceRgb  => false,
        ChannelMode::Auto      => img.color().has_alpha(),
    };

    let qoi_result = if use_rgba {
        let rgba = img.into_rgba8();
        qoi::encode_to_vec(rgba.as_raw(), w, h)
    } else {
        // into_rgb8 drops alpha if present; produces w*h*3 bytes.
        let rgb = img.into_rgb8();
        qoi::encode_to_vec(rgb.as_raw(), w, h)
    };

    let qoi_bytes = match qoi_result {
        Ok(b) => b,
        Err(e) => {
            eprintln!("  warn: QOI encode failed for {name}: {e} — storing as blob");
            let (p, pk) = compress_with_preset(raw, preset);
            return (AssetKind::Blob, p, pk);
        }
    };

    let (payload, packing) = compress_with_preset(&qoi_bytes, preset);
    (AssetKind::TextureRaw { width: w, height: h }, payload, packing)
}

#[cfg(feature = "full")]
fn pack_texture_gpu(raw: &[u8]) -> (AssetKind, Vec<u8>, Packing) {
    let (w, h) = parse_gpu_dimensions(raw);
    (AssetKind::TextureGpu { width: w, height: h }, raw.to_vec(), Packing::Raw)
}

/// Parse width/height from DDS or ASTC headers.
/// Returns (0, 0) if the format is unrecognised or the header is too short.
#[cfg(feature = "full")]
fn parse_gpu_dimensions(raw: &[u8]) -> (u32, u32) {
    // DDS: magic "DDS " + 4-byte dwSize, then dwFlags, dwHeight, dwWidth
    if raw.len() >= 20 && raw.starts_with(b"DDS ") {
        let h = u32::from_le_bytes(raw[12..16].try_into().unwrap());
        let w = u32::from_le_bytes(raw[16..20].try_into().unwrap());
        return (w, h);
    }
    // ASTC: magic 0x5CA1AB13 (LE), then block_w(1), block_h(1), block_d(1),
    //       dim_x(3 LE), dim_y(3 LE)
    if raw.len() >= 16 && raw[0..4] == [0x13, 0xAB, 0xA1, 0x5C] {
        let w = u32::from_le_bytes([raw[7],  raw[8],  raw[9],  0]);
        let h = u32::from_le_bytes([raw[10], raw[11], raw[12], 0]);
        return (w, h);
    }
    (0, 0)
}

#[cfg(feature = "full")]
fn pack_audio(raw: &[u8], name: &str) -> (AssetKind, Vec<u8>, Packing) {
    let ext = std::path::Path::new(name)
        .extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let codec = match ext.as_str() {
        "wav" => "pcm16", "ogg" => "vorbis", "mp3" => "mp3",
        "opus" => "opus", "flac" => "flac", "aac" => "aac", "m4a" => "m4a",
        _ => "unknown",
    };
    let meta = if ext == "wav" { wav_meta(raw) } else { (0, 0, 0) };
    (AssetKind::Audio { codec: codec.into(), sample_rate: meta.0,
                        channels: meta.1, duration_ms: meta.2 },
     raw.to_vec(), Packing::Raw)
}

#[cfg(feature = "full")]
fn pack_blob(raw: &[u8], preset: Preset) -> (AssetKind, Vec<u8>, Packing) {
    let (payload, packing) = compress_with_preset(raw, preset);
    (AssetKind::Blob, payload, packing)
}

#[cfg(feature = "full")]
fn pack_text(raw: &[u8], preset: Preset) -> (AssetKind, Vec<u8>, Packing) {
    // Text always zstd regardless of preset (already compressible).
    let lvl = if preset.is_lz4() { 8 } else { preset.zstd_level().clamp(1, 22) };
    match crate::stripped_zstd::encode(raw, lvl) {
        Ok(c) => (AssetKind::Blob, c, Packing::Zstd),
        Err(_) => (AssetKind::Blob, raw.to_vec(), Packing::Raw),
    }
}

// ─── Mesh pack ────────────────────────────────────────────────────────────────

/// Pack a GLB/GLTF mesh file.
///
/// Pipeline:
///   1. meshopt::optimize_vertex_cache — reorder indices for GPU post-transform cache
///   2. With mesh_opt=true: meshopt::simplify (~50% triangle reduction, ≤1% error)
///   3. Compress the resulting GLB with zstd at the highest level the preset allows.
///      Meshes are always zstd (never lz4) because the optimized vertex data
///      benefits from long-range matching in zstd more than from lz4 speed.
///
/// Fallback: if the file is not a valid embedded-buffer GLB, stored as blob.
#[cfg(feature = "full")]
pub fn pack_mesh(raw: &[u8], preset: Preset, mesh_opt: bool) -> (AssetKind, Vec<u8>, Packing) {
    match crate::mesh::optimize_glb(raw, mesh_opt) {
        Some(result) => {
            // Always compress meshes with zstd.
            // Use at least level 16 even on --preset fast (lz4 gives poor ratio on GLB).
            let level = if preset.is_lz4() { 16 } else { preset.zstd_level() };
            let payload = match crate::stripped_zstd::encode(&result.data, level) {
                Ok(c) if c.len() < result.data.len() => c,
                _ => result.data.clone(),
            };
            let packing = if payload.len() < result.data.len() {
                Packing::Zstd
            } else {
                Packing::Raw
            };
            (AssetKind::Mesh {
                vertex_count:   result.vertex_count,
                triangle_count: result.triangle_count,
                quantized:      result.quantized,
            }, payload, packing)
        }
        None => {
            // Not a valid GLB — fall back to blob compression.
            let (payload, packing) = compress_with_preset(raw, preset);
            (AssetKind::Blob, payload, packing)
        }
    }
}

// ─── Reconstruction ───────────────────────────────────────────────────────────

/// Decode TextureRaw payload → raw RGBA8 bytes (w×h×4).
///
/// Handles:
///   - QOI RGB  (3 bpp) → expanded to RGBA8 with alpha = 255
///   - QOI RGBA (4 bpp) → returned as-is
///   - Legacy stripped-WebP (very old bundles)
///
/// Note: Legacy KTX2/Basis bundles created with bsx ≤ 2.0 cannot be decoded
/// by this build (ktx2-rw removed). Re-pack those bundles with bsx ≥ 2.1.
pub fn decode_rgba8(payload: &[u8], kind: &AssetKind) -> Result<Vec<u8>, String> {
    match kind {
        AssetKind::TextureRaw { .. } => {
            if payload.len() >= 4 && &payload[0..4] == b"qoif" {
                return decode_qoi_rgba8(payload);
            }
            if payload.len() >= 12 && &payload[0..12] == b"\xabKTX 20\xbb\r\n\x1a\n" {
                return Err("legacy KTX2/Basis texture: re-pack with bsx ≥ 2.1".into());
            }
            Err("unknown TextureRaw payload format: re-pack bundle".into())
        }
        _ => Ok(payload.to_vec()),
    }
}

/// QOI → RGBA8 decode.
///
/// If the QOI stream was encoded as RGB (3 bpp), the pixels are expanded
/// in-place to RGBA8 (alpha = 255) so callers always receive w×h×4 bytes.
/// The expansion is a single pre-allocated pass: O(n), zero extra allocations.
fn decode_qoi_rgba8(data: &[u8]) -> Result<Vec<u8>, String> {
    let (header, pixels) = qoi::decode_to_vec(data)
        .map_err(|e| format!("QOI decode: {e}"))?;

    // header.channels: 3 = RGB, 4 = RGBA.
    if header.channels == qoi::Channels::Rgb {
        // Expand RGB → RGBA (alpha = 255).
        // Pre-allocate the exact output size to avoid reallocs.
        let n_pixels = pixels.len() / 3;
        let mut rgba = Vec::with_capacity(n_pixels * 4);
        for chunk in pixels.chunks_exact(3) {
            rgba.extend_from_slice(chunk);
            rgba.push(255u8);
        }
        Ok(rgba)
    } else {
        // Already RGBA — return as-is (no copy).
        Ok(pixels)
    }
}

// ─── Output filename for depack ───────────────────────────────────────────────

pub fn output_ext(name: &str, kind: &AssetKind) -> String {
    match kind {
        AssetKind::TextureRaw { .. } => crate::util::with_ext(name, "png"),
        _ => name.to_string(),
    }
}

// ─── Compression helpers ──────────────────────────────────────────────────────

pub fn lz4_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    crate::stripped_lz4::decode(data)
}

#[cfg(feature = "full")]
pub fn zstd_compress(data: &[u8], level: i32) -> Vec<u8> {
    zstd::encode_all(data, level).unwrap_or_else(|_| data.to_vec())
}

pub fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    crate::stripped_zstd::decode_any(data).map_err(|e| e.to_string())
}

// ─── CRC32 ───────────────────────────────────────────────────────────────────

pub fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

// ─── JPEG quality detection ───────────────────────────────────────────────────

#[cfg(feature = "full")]
pub fn detect_jpeg_quality(data: &[u8]) -> Option<u8> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 { return None; }
    let mut i = 2usize;
    while i + 3 < data.len() {
        if data[i] != 0xFF { break; }
        let marker  = data[i + 1];
        if marker == 0xDA { break; }
        let seg_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if seg_len < 2 || i + 2 + seg_len > data.len() { break; }
        if marker == 0xDB {
            let j    = i + 4;
            if j >= data.len() { break; }
            let prec = (data[j] >> 4) & 0x0F;
            let step = if prec == 0 { 1usize } else { 2 };
            if j + 1 + step > data.len() { break; }
            let dc_val: u32 = if prec == 0 { data[j + 1] as u32 }
                              else { u16::from_be_bytes([data[j + 1], data[j + 2]]) as u32 };
            let scale   = dc_val.saturating_mul(100) / 16;
            let quality: u32 = if scale == 0 { 100 }
                               else if scale >= 100 { (200u32.saturating_sub(scale)) / 2 }
                               else { 5000 / scale };
            return Some(quality.clamp(1, 100) as u8);
        }
        i += 2 + seg_len;
    }
    None
}

// ─── Metadata helpers ─────────────────────────────────────────────────────────

#[cfg(feature = "full")]
pub fn wav_meta(data: &[u8]) -> (u32, u8, u32) {
    if data.len() < 44 { return (0, 0, 0); }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" { return (0, 0, 0); }
    let channels    = u16::from_le_bytes(data[22..24].try_into().unwrap_or([0;2])) as u8;
    let sample_rate = u32::from_le_bytes(data[24..28].try_into().unwrap_or([0;4]));
    let bit_depth   = u16::from_le_bytes(data[34..36].try_into().unwrap_or([2;2]));
    let data_size   = u32::from_le_bytes(data[40..44].try_into().unwrap_or([0;4]));
    let bps         = (bit_depth / 8).max(1) as u32;
    let samples     = data_size / (channels as u32 * bps);
    let duration_ms = if sample_rate > 0 { samples * 1000 / sample_rate } else { 0 };
    (sample_rate, channels, duration_ms)
}

pub fn is_system_file(path: &std::path::Path) -> bool {
    for component in path.components() {
        let s = component.as_os_str().to_string_lossy();
        if matches!(s.as_ref(), "__MACOSX" | ".Spotlight-V100" | ".Trashes" | ".fseventsd") {
            return true;
        }
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(name, ".DS_Store" | "Thumbs.db" | "Desktop.ini") || name.starts_with("._")
}

// ─── Display helpers ──────────────────────────────────────────────────────────

pub fn kind_to_str(kind: &AssetKind) -> String {
    match kind {
        AssetKind::TextureRaw { width, height } => format!("IMG  {width}×{height}"),
        AssetKind::TextureGpu { width, height } => format!("GPU  {width}×{height}"),
        AssetKind::Audio { codec, sample_rate, channels, duration_ms } => {
            if *sample_rate > 0 {
                format!("{codec} {sample_rate}Hz/{channels}ch {duration_ms}ms")
            } else {
                format!("AUDIO/{codec}")
            }
        }
        AssetKind::Mesh { vertex_count, triangle_count, quantized } => {
            let tag = if *quantized { "MESH~" } else { "MESH " };
            format!("{tag} {vertex_count}v {triangle_count}t")
        }
        AssetKind::Blob => "BLOB".into(),
    }
}

pub fn packing_to_str(p: &Packing) -> &'static str {
    match p { Packing::Raw => "raw", Packing::Lz4 => "lz4", Packing::Zstd => "zstd" }
}

// ─── PNG encode helper (for url() in WASM) ────────────────────────────────────

/// Encode raw RGBA8 bytes to PNG in-memory.
///
/// Uses Compression::Fast (level 1) — for URL blob creation, speed beats size.
/// The browser discards the PNG after decoding it to a GPU texture anyway.
pub fn encode_rgba_to_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    use png::{BitDepth, ColorType, Compression, Encoder};
    let mut buf = Vec::with_capacity(rgba.len() / 2);
    let mut enc = Encoder::new(&mut buf, w, h);
    enc.set_color(ColorType::Rgba);
    enc.set_depth(BitDepth::Eight);
    enc.set_compression(Compression::Fast);
    let mut writer = enc.write_header().map_err(|e| format!("PNG header: {e}"))?;
    writer.write_image_data(rgba).map_err(|e| format!("PNG encode: {e}"))?;
    drop(writer);
    Ok(buf)
}