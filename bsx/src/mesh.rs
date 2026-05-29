//! GLB mesh optimization pipeline.
//!
//! Lossless (always):
//!   - Parse GLB binary blob
//!   - For each mesh primitive: optimize_vertex_cache (reorder indices for GPU cache)
//!
//! Lossy (mesh_opt = true, --opt flag):
//!   - Additionally: meshopt::simplify (50% triangle reduction, ≤1% geometric error)
//!   - Updates accessor.count in the GLTF JSON accordingly

use serde_json::{json, Value};

// ─── Public result ────────────────────────────────────────────────────────────

pub struct MeshOptResult {
    pub data:           Vec<u8>,  // optimized GLB bytes (uncompressed; caller compresses)
    pub vertex_count:   u32,
    pub triangle_count: u32,
    pub quantized:      bool,     // true if --opt lossy passes were applied
}

// ─── GLB parse / build ────────────────────────────────────────────────────────

/// Parse GLB v2 → (json_value, bin_blob).
/// Returns None if not a valid GLB.
fn parse_glb(raw: &[u8]) -> Option<(Value, Vec<u8>)> {
    if raw.len() < 20 { return None; }
    let magic = u32::from_le_bytes(raw[0..4].try_into().ok()?);
    if magic != 0x46546C67 { return None; } // 'glTF'

    let json_chunk_len  = u32::from_le_bytes(raw[12..16].try_into().ok()?) as usize;
    let json_chunk_type = u32::from_le_bytes(raw[16..20].try_into().ok()?);
    if json_chunk_type != 0x4E4F534A { return None; } // must be 'JSON'
    if 20 + json_chunk_len > raw.len() { return None; }

    let json: Value = serde_json::from_slice(&raw[20..20 + json_chunk_len]).ok()?;

    // BIN chunk starts after JSON chunk, aligned to 4 bytes.
    let bin_start = (20 + json_chunk_len + 3) & !3;
    let bin = if bin_start + 8 <= raw.len() {
        let bin_len  = u32::from_le_bytes(raw[bin_start..bin_start + 4].try_into().ok()?) as usize;
        let bin_type = u32::from_le_bytes(raw[bin_start + 4..bin_start + 8].try_into().ok()?);
        if bin_type != 0x004E4942 {
            // Not BIN\0 — some GLBs have no binary blob (all uri-based)
            return Some((json, vec![]));
        }
        let end = (bin_start + 8 + bin_len).min(raw.len());
        raw[bin_start + 8..end].to_vec()
    } else {
        vec![]
    };

    Some((json, bin))
}

/// Reconstruct GLB bytes from potentially-modified JSON and binary blob.
fn build_glb(json: &Value, bin: &[u8]) -> Vec<u8> {
    let json_bytes = serde_json::to_vec(json).unwrap_or_default();
    // JSON chunk must be padded to 4 bytes with spaces (0x20).
    let json_pad = (4 - (json_bytes.len() & 3)) & 3;
    // BIN chunk pads with zero bytes.
    let bin_pad  = (4 - (bin.len() & 3)) & 3;

    let has_bin = !bin.is_empty();
    let total: usize = 12                                   // GLB header
        + 8 + json_bytes.len() + json_pad                  // JSON chunk
        + if has_bin { 8 + bin.len() + bin_pad } else { 0 }; // BIN chunk

    let mut out = Vec::with_capacity(total);

    // Header
    out.extend_from_slice(&0x46546C67u32.to_le_bytes()); // magic 'glTF'
    out.extend_from_slice(&2u32.to_le_bytes());           // version 2
    out.extend_from_slice(&(total as u32).to_le_bytes()); // total length

    // JSON chunk
    out.extend_from_slice(&((json_bytes.len() + json_pad) as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F534Au32.to_le_bytes()); // chunk type 'JSON'
    out.extend_from_slice(&json_bytes);
    out.extend(core::iter::repeat(0x20u8).take(json_pad)); // pad with spaces

    // BIN chunk (optional)
    if has_bin {
        out.extend_from_slice(&((bin.len() + bin_pad) as u32).to_le_bytes());
        out.extend_from_slice(&0x004E4942u32.to_le_bytes()); // chunk type 'BIN\0'
        out.extend_from_slice(bin);
        out.extend(core::iter::repeat(0u8).take(bin_pad));
    }

    out
}

// ─── GLTF JSON helpers ────────────────────────────────────────────────────────

fn as_usize(v: &Value) -> Option<usize> {
    v.as_u64().map(|n| n as usize).or_else(|| v.as_i64().map(|n| n as usize))
}

// ─── Index extraction ─────────────────────────────────────────────────────────

/// componentType constants
const CT_UBYTE:  usize = 5121;
const CT_USHORT: usize = 5123;
const CT_UINT:   usize = 5125;
const CT_FLOAT:  usize = 5126;

fn extract_indices(bin: &[u8], offset: usize, count: usize, ctype: usize) -> Vec<u32> {
    match ctype {
        CT_UBYTE  => (0..count).map(|i| bin[offset + i] as u32).collect(),
        CT_USHORT => (0..count).map(|i| {
            let o = offset + i * 2;
            u16::from_le_bytes(bin[o..o+2].try_into().unwrap_or([0;2])) as u32
        }).collect(),
        CT_UINT   => (0..count).map(|i| {
            let o = offset + i * 4;
            u32::from_le_bytes(bin[o..o+4].try_into().unwrap_or([0;4]))
        }).collect(),
        _ => vec![],
    }
}

fn write_indices(bin: &mut [u8], offset: usize, max_slots: usize, indices: &[u32], ctype: usize) {
    let write_count = indices.len().min(max_slots);
    match ctype {
        CT_UBYTE  => for (i, &idx) in indices[..write_count].iter().enumerate() {
            bin[offset + i] = idx as u8;
        },
        CT_USHORT => for (i, &idx) in indices[..write_count].iter().enumerate() {
            let o = offset + i * 2;
            bin[o..o+2].copy_from_slice(&(idx as u16).to_le_bytes());
        },
        CT_UINT   => for (i, &idx) in indices[..write_count].iter().enumerate() {
            let o = offset + i * 4;
            bin[o..o+4].copy_from_slice(&idx.to_le_bytes());
        },
        _ => {},
    }
    // Zero out any trailing slots if we wrote fewer (simplified).
    match ctype {
        CT_USHORT => for i in write_count..max_slots {
            let o = offset + i * 2;
            bin[o..o+2].fill(0);
        },
        CT_UINT   => for i in write_count..max_slots {
            let o = offset + i * 4;
            bin[o..o+4].fill(0);
        },
        _ => {},
    }
}

/// Extract f32 XYZ positions (stride = 12 bytes or bufferView.byteStride).
fn extract_positions(bin: &[u8], bv_offset: usize, stride: usize, count: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(count * 3);
    for vi in 0..count {
        let base = bv_offset + vi * stride;
        if base + 12 > bin.len() { out.extend_from_slice(&[0.0f32, 0.0, 0.0]); continue; }
        out.push(f32::from_le_bytes(bin[base..base+4].try_into().unwrap_or([0;4])));
        out.push(f32::from_le_bytes(bin[base+4..base+8].try_into().unwrap_or([0;4])));
        out.push(f32::from_le_bytes(bin[base+8..base+12].try_into().unwrap_or([0;4])));
    }
    out
}

// ─── Main entry point ─────────────────────────────────────────────────────────

/// Optimize a GLB mesh and return a new GLB byte vector.
///
/// Lossless passes (always):
///   - meshopt::optimize_vertex_cache — reorders indices for GPU post-transform cache
///
/// Lossy passes (mesh_opt = true):
///   - meshopt::simplify — reduces triangle count by ~50% (≤1% geometric error)
///   - Updates GLTF accessor.count in JSON to match
///
/// Returns None on parse failure or if the GLB has no embedded binary chunk
/// (external/uri-based buffers are not supported — passed through as-is).
pub fn optimize_glb(raw: &[u8], mesh_opt: bool) -> Option<MeshOptResult> {
    let (mut json, mut bin) = parse_glb(raw)?;

    // If there's no embedded binary blob we can't optimize.
    if bin.is_empty() {
        return Some(MeshOptResult {
            data: raw.to_vec(),
            vertex_count: 0,
            triangle_count: 0,
            quantized: false,
        });
    }

    let accessors   = json["accessors"].as_array()?.clone();
    let buffer_views = json["bufferViews"].as_array()?.clone();
    let meshes       = json["meshes"].as_array()?.clone();

    let mut total_verts = 0u32;
    let mut total_tris  = 0u32;

    // Deferred JSON updates: (accessor_index, new_count).
    // Applied after the primitive loop to avoid borrow issues.
    let mut count_updates: Vec<(usize, usize)> = Vec::new();

    for mesh in &meshes {
        let prims = match mesh["primitives"].as_array() {
            Some(p) => p,
            None    => continue,
        };
        for prim in prims {
            // ── Locate INDICES accessor ────────────────────────────────────────
            let idx_acc_i = match as_usize(&prim["indices"]) {
                Some(i) => i,
                None    => continue, // non-indexed mesh, skip
            };
            if idx_acc_i >= accessors.len() { continue; }
            let idx_acc   = &accessors[idx_acc_i];
            let idx_bv_i  = match as_usize(&idx_acc["bufferView"]) {
                Some(i) => i,
                None    => continue,
            };
            if idx_bv_i >= buffer_views.len() { continue; }
            let idx_bv     = &buffer_views[idx_bv_i];
            let idx_bv_off = as_usize(&idx_bv["byteOffset"]).unwrap_or(0);
            let idx_acc_off = as_usize(&idx_acc["byteOffset"]).unwrap_or(0);
            let idx_offset  = idx_bv_off + idx_acc_off;
            let idx_count   = match as_usize(&idx_acc["count"]) {
                Some(c) => c,
                None    => continue,
            };
            let idx_ctype   = match as_usize(&idx_acc["componentType"]) {
                Some(t) => t,
                None    => continue,
            };
            if idx_count == 0 || idx_count % 3 != 0 { continue; }

            // ── Locate POSITION accessor ───────────────────────────────────────
            let pos_acc_i = match as_usize(&prim["attributes"]["POSITION"]) {
                Some(i) => i,
                None    => continue,
            };
            if pos_acc_i >= accessors.len() { continue; }
            let pos_acc   = &accessors[pos_acc_i];
            let vertex_count = match as_usize(&pos_acc["count"]) {
                Some(c) => c,
                None    => continue,
            };
            if vertex_count == 0 { continue; }

            // Check POSITION is f32 VEC3 (componentType 5126, type "VEC3").
            if as_usize(&pos_acc["componentType"]) != Some(CT_FLOAT) { continue; }
            if pos_acc["type"].as_str() != Some("VEC3") { continue; }

            let pos_bv_i = match as_usize(&pos_acc["bufferView"]) {
                Some(i) => i,
                None    => continue,
            };
            if pos_bv_i >= buffer_views.len() { continue; }
            let pos_bv     = &buffer_views[pos_bv_i];
            let pos_bv_off = as_usize(&pos_bv["byteOffset"]).unwrap_or(0);
            // byteStride: interleaved stride (optional; default for VEC3 f32 = 12)
            let pos_stride = as_usize(&pos_bv["byteStride"]).unwrap_or(12);

            // ── Extract indices ────────────────────────────────────────────────
            if idx_offset + idx_count * index_byte_size(idx_ctype) > bin.len() { continue; }
            let mut indices = extract_indices(&bin, idx_offset, idx_count, idx_ctype);
            if indices.is_empty() { continue; }

            // ── Lossless: Vertex Cache Optimization ───────────────────────────
            indices = meshopt::optimize_vertex_cache(&indices, vertex_count);

            // ── Lossy: Simplify (--opt) ────────────────────────────────────────
            let final_indices = if mesh_opt {
                let positions = extract_positions(&bin, pos_bv_off, pos_stride, vertex_count);
let target = (idx_count / 2) & !2;
let target = target.max(3);

let pos_bytes = unsafe {
    std::slice::from_raw_parts(
        positions.as_ptr() as *const u8,
        positions.len() * std::mem::size_of::<f32>(),
    )
};
let vertex_data = meshopt::VertexDataAdapter::new(pos_bytes, 12, 0)
    .expect("VertexDataAdapter");
let simplified = meshopt::simplify(
    &indices,
    &vertex_data,
    target,
    0.01,
    meshopt::SimplifyOptions::empty(),
    None,
);
                // Only accept if we actually reduced (simplify may return full set on error)
                if simplified.len() < indices.len() && !simplified.is_empty() {
                    simplified
                } else {
                    indices
                }
            } else {
                indices
            };

            let final_idx_count = final_indices.len();

            // ── Write back optimized indices ───────────────────────────────────
            let max_slots = idx_count; // never exceed original buffer space
            if final_idx_count > max_slots { continue; } // sanity check
            write_indices(&mut bin, idx_offset, max_slots, &final_indices, idx_ctype);

            // Schedule accessor.count update if simplified
            if final_idx_count != idx_count {
                count_updates.push((idx_acc_i, final_idx_count));
            }

            total_verts += vertex_count as u32;
            total_tris  += (final_idx_count / 3) as u32;
        }
    }

    // Apply deferred JSON count updates (for simplify)
    if let Some(accs) = json["accessors"].as_array_mut() {
        for (acc_i, new_count) in &count_updates {
            if let Some(acc) = accs.get_mut(*acc_i) {
                acc["count"] = json!(*new_count);
            }
        }
    }

    let optimized = build_glb(&json, &bin);
    Some(MeshOptResult {
        data: optimized,
        vertex_count: total_verts,
        triangle_count: total_tris,
        quantized: mesh_opt,
    })
}

fn index_byte_size(ctype: usize) -> usize {
    match ctype {
        CT_UBYTE  => 1,
        CT_USHORT => 2,
        CT_UINT   => 4,
        _         => 0,
    }
}