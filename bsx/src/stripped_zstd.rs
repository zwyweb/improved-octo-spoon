//! Stripped zstd: removes the 4-byte magic `[0x28 0xB5 0x2F 0xFD]` before
//! writing and restores it before decoding.
//! Output bytes match no known file signature — safe to store inside BSX
//! without confusing file sniffers.
//!
//! On native targets, uses the `zstd` crate (C, full compress + decompress).
//! On wasm32 targets, also uses `zstd` (C via Emscripten, decompress + dict support).
//!
//! ## No-alloc magic prepend
//!
//! Instead of allocating `Vec::with_capacity(4 + data.len())` just to prepend
//! the 4-byte magic, `decode` and `decode_any` use `std::io::Read::chain` to
//! present `[magic bytes] ++ data` as a single reader.  The decompressor
//! consumes the chain without ever materialising the concatenation.

use anyhow::{Context as _, Result};

/// Returns the ZSTD frame magic `[0x28, 0xB5, 0x2F, 0xFD]` at runtime.
///
/// **Why a function instead of a const?**
/// A `const [u8; 4]` embeds the literal 4-byte sequence in the compiled
/// binary / WASM blob.  When that binary is packed inside a BSX bundle,
/// tools like `binwalk` scan the entire file for known signatures and
/// produce false-positive ZSTD hits at the offset of this constant.
///
/// `std::hint::black_box` prevents the optimiser from constant-folding
/// the XOR back into `[0x28, 0xB5, 0x2F, 0xFD]`, so only the obfuscated
/// bytes `[0x82, 0x1F, 0x85, 0x57]` appear in the compiled output.
/// At runtime the cost is four XOR instructions — effectively zero.
#[inline(always)]
fn magic() -> [u8; 4] {
    // 0x28^0xAA=0x82  0xB5^0xAA=0x1F  0x2F^0xAA=0x85  0xFD^0xAA=0x57
    const OBF: [u8; 4] = [0x82, 0x1F, 0x85, 0x57];
    let src = std::hint::black_box(OBF);
    [src[0] ^ 0xAA, src[1] ^ 0xAA, src[2] ^ 0xAA, src[3] ^ 0xAA]
}

// ─── Compress (native only) ───────────────────────────────────────────────────

/// Compress with zstd and strip the leading 4-byte magic.
#[cfg(feature = "full")]
pub fn encode(data: &[u8], level: i32) -> Result<Vec<u8>> {
    let compressed = zstd::encode_all(data, level).context("zstd encode")?;
    if compressed.len() < 4 || compressed[0..4] != magic() {
        return Err(anyhow::anyhow!("unexpected zstd output (no magic prefix)"));
    }
    Ok(compressed[4..].to_vec())
}

/// Try to compress; returns original bytes unchanged if compression doesn't help.
#[cfg(feature = "full")]
pub fn try_encode(data: &[u8], level: i32) -> (Vec<u8>, bool) {
    match encode(data, level) {
        Ok(c) if c.len() < data.len() => (c, true),
        _                             => (data.to_vec(), false),
    }
}

// ─── Decompress (all targets) ─────────────────────────────────────────────────

/// Decompress data produced by [`encode`] (magic prefix absent).
///
/// Uses a chain-reader to prepend the 4-byte magic without allocating a
/// combined buffer — the decompressor sees `[magic] ++ data` as a stream.
pub fn decode(data: &[u8]) -> Result<Vec<u8>> {
    let m = magic();
    decompress_impl(&m, data)
}

/// Accept both stripped (no magic) and legacy full-frame (with magic) blobs.
pub fn decode_any(data: &[u8]) -> Result<Vec<u8>> {
    if data.starts_with(&magic()) {
        // Full frame — pass directly, no prepend needed.
        decompress_impl(&[], data)
    } else {
        // Stripped — prepend magic via chain-reader (zero allocation).
        let m = magic();
        decompress_impl(&m, data)
    }
}

// ─── Backend ──────────────────────────────────────────────────────────────────

/// Core decompression: `prefix ++ data` → decompressed bytes.
///
/// When `prefix` is empty the data is passed directly (no chain overhead).
/// When `prefix` is non-empty a `Chain` reader presents both slices as one
/// contiguous stream without any intermediate allocation.

fn decompress_impl(prefix: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    if prefix.is_empty() {
        zstd::decode_all(data).context("zstd decode")
    } else {
        // Chain: magic bytes followed by the stripped payload.
        // zstd::decode_all accepts any `impl Read`.
        let reader = prefix.chain(data);
        zstd::decode_all(reader).context("zstd decode (chain)")
    }
}

// ─── Dict-aware decompress ────────────────────────────────────────────────────

/// Decompress a full zstd frame that was compressed with a trained dictionary.
/// Uses the `zstd` C library on all targets (native and wasm32).
pub fn decode_with_dict(data: &[u8], dict: &[u8]) -> Result<Vec<u8>> {
    let mut dec = zstd::bulk::Decompressor::with_dictionary(dict)
        .context("zstd dict decompressor init")?;
    dec.decompress(data, 64 * 1024 * 1024).context("zstd dict decompress")
}