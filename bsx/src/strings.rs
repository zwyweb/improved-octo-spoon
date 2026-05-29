//! String pool encoder / decoder (BSMS format).
//!
//! ## Flatbuffer-like layout (after XOR + stripped-zstd decode)
//!
//! ```text
//! [0..4]              count    u32 LE
//! [4 .. 4+count*4]    offsets  u32 LE × count   — byte offset into DATA
//! [4+count*4 ..]      data     concatenated raw UTF-8, no separators
//! ```
//!
//! O(1) access: `get(i)` = `data[offset[i] .. offset[i+1]]`
//!
//! ## Obfuscation
//!
//! ```text
//! stored[i] = plain[i] ^ key[i % key_len] ^ SHIFT
//! ```
//! where `SHIFT = 0xBD` (= -67i8 as u8).
//! When key is empty, only SHIFT is applied.

use anyhow::{anyhow, Context, Result};

const SHIFT: u8 = 0xBD; // (-67i8) as u8

// ─── XOR ─────────────────────────────────────────────────────────────────────

fn xor_apply(data: &[u8], key: &[u8]) -> Vec<u8> {
    let kl = key.len();
    data.iter().enumerate().map(|(i, &b)| {
        let k = if kl == 0 { 0u8 } else { key[i % kl] };
        b ^ k ^ SHIFT
    }).collect()
}

// ─── Pool layout ─────────────────────────────────────────────────────────────

/// Build the raw flatbuffer-layout bytes from a list of strings.
pub fn build_pool_bytes(strings: &[String]) -> Vec<u8> {
    let count = strings.len() as u32;
    let mut offsets: Vec<u32> = Vec::with_capacity(strings.len());
    let mut data: Vec<u8> = Vec::new();
    for s in strings {
        offsets.push(data.len() as u32);
        data.extend_from_slice(s.as_bytes());
    }
    let mut raw = Vec::with_capacity(4 + offsets.len() * 4 + data.len());
    raw.extend_from_slice(&count.to_le_bytes());
    for &o in &offsets { raw.extend_from_slice(&o.to_le_bytes()); }
    raw.extend_from_slice(&data);
    raw
}

/// Parse the raw flatbuffer layout into strings.
pub fn parse_pool_bytes(raw: &[u8]) -> Result<Vec<String>> {
    if raw.len() < 4 { return Err(anyhow!("pool too small ({} B)", raw.len())); }
    let count     = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
    let table_end = 4 + count * 4;
    if raw.len() < table_end {
        return Err(anyhow!("offset table truncated (need {table_end} B, have {})", raw.len()));
    }
    let data_region = &raw[table_end..];
    let mut offsets = Vec::with_capacity(count);
    for i in 0..count {
        let o = 4 + i * 4;
        offsets.push(u32::from_le_bytes(raw[o..o+4].try_into().unwrap()) as usize);
    }
    let mut strings = Vec::with_capacity(count);
    for (i, &start) in offsets.iter().enumerate() {
        let end = if i + 1 < count { offsets[i+1] } else { data_region.len() };
        if end > data_region.len() || start > end {
            return Err(anyhow!("string {i}: offset out of range ({start}..{end})"));
        }
        let s = std::str::from_utf8(&data_region[start..end])
            .with_context(|| format!("string {i} is not valid UTF-8"))?;
        strings.push(s.to_owned());
    }
    Ok(strings)
}

// ─── High-level encode / decode ───────────────────────────────────────────────

/// Encode a slice of strings into obfuscated pool bytes ready to embed in BSX.
///
/// Pipeline: strings → flat pool → stripped-zstd → XOR
#[cfg(feature = "full")]
pub fn encode_pool(strings: &[String], key: &str) -> Result<Vec<u8>> {
    let pool_raw   = build_pool_bytes(strings);
    let compressed = crate::stripped_zstd::encode(&pool_raw, 19)
        .context("zstd compress string pool")?;
    Ok(xor_apply(&compressed, key.as_bytes()))
}

/// Decode obfuscated pool bytes back into strings.
///
/// Pipeline: XOR → stripped-zstd → pool parse
pub fn decode_pool(data: &[u8], key: &str) -> Result<Vec<String>> {
    let deobf    = xor_apply(data, key.as_bytes());
    let pool_raw = crate::stripped_zstd::decode(&deobf)
        .context("zstd decompress pool — wrong key or corrupted")?;
    parse_pool_bytes(&pool_raw)
        .context("parse pool")
}

/// Load strings from a `.pool` text file (one UTF-8 string per line).
/// The literal sequence `\n` in a line is converted to a real newline.
pub fn load_pool_file(path: &std::path::Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read pool file {:?}", path))?;
    let strings: Vec<String> = text.lines()
        .map(|l| l.replace("\\n", "\n"))
        .collect();
    if strings.is_empty() {
        return Err(anyhow!("pool file is empty: {:?}", path));
    }
    Ok(strings)
}
