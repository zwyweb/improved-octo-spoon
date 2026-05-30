//! Stripped LZ4: XOR-obfuscates only the 4-byte prepended-size field produced
//! by `lz4_flex::compress_prepend_size`.
//!
//! `lz4_flex` uses LZ4 *block* format (no frame magic `0x184D2204`), so
//! binwalk won't fire on the raw body.  We only obfuscate the size prefix to
//! defeat heuristic size-header fingerprinting — full-body XOR is unnecessary
//! and was the dominant hot-path cost in decode.
//!
//! Wire format: `[4 B: XOR(orig_size_le, KEY)][lz4_block_data]`
//!
//! ⚠ Breaking change vs old body-XOR format: re-pack existing bundles.

/// Key for size-prefix XOR (obfuscated at compile time).
#[inline(always)]
fn size_key() -> [u8; 4] {
    // 0xBE^0xDE=0x60  0xEF^0xDE=0x31  0xCA^0xDE=0x14  0xFE^0xDE=0x20
    const OBF: [u8; 4] = [0x60, 0x31, 0x14, 0x20];
    let s = std::hint::black_box(OBF);
    [s[0] ^ 0xDE, s[1] ^ 0xDE, s[2] ^ 0xDE, s[3] ^ 0xDE]
}

/// Compress with LZ4 and obfuscate the 4-byte size prefix.
/// The LZ4 block body is stored verbatim (no body XOR).
pub fn encode(data: &[u8]) -> Vec<u8> {
    let mut compressed = lz4_flex::compress_prepend_size(data);
    let k = size_key();
    compressed[0] ^= k[0];
    compressed[1] ^= k[1];
    compressed[2] ^= k[2];
    compressed[3] ^= k[3];
    compressed
}

/// Decompress data produced by [`encode`].
///
/// Zero intermediate allocations — XOR 4 bytes of prefix on the stack,
/// pass `data[4..]` directly to `lz4_flex::decompress`.
pub fn decode(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 4 {
        return Err("stripped_lz4: too short".into());
    }
    let k = size_key();
    let orig_size = u32::from_le_bytes([
        data[0] ^ k[0],
        data[1] ^ k[1],
        data[2] ^ k[2],
        data[3] ^ k[3],
    ]) as usize;

    lz4_flex::decompress(&data[4..], orig_size).map_err(|e| e.to_string())
}