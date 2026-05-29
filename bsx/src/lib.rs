//! BSX v2 — Game Asset Bundle
#![allow(clippy::result_large_err)]

pub mod asset;
#[cfg(feature = "full")]
pub mod audio;
pub mod codec;
pub mod format;
pub mod fx;
#[cfg(feature = "full")]
pub mod mesh;
pub mod strings;
pub mod stripped_lz4;
pub mod stripped_zstd;
pub mod util;
#[cfg(feature = "wasm")]
pub mod wasm;

use anyhow::Context;
use asset::{crc32, file_ext, is_system_file, lz4_decompress, output_ext, zstd_decompress,
            classify, EXT_AUDIO};
#[cfg(feature = "full")]
use asset::pack_asset;
#[cfg(feature = "full")]
use audio::pack_audio_tracks;
use format::{
    AssetEntry, AssetKind, BinmapAssetEntry, BinmapToc, BinmapTrackEntry,
    BmapIndex, BsxToc, LoadGroup, PackResult, Packing, Preset, StringPoolDesc,
    HEADER_SIZE, MAGIC, VERSION,
};
#[cfg(feature = "full")]
use rayon::prelude::*;
use std::{
    collections::HashMap,
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{atomic::{AtomicU64, AtomicUsize, Ordering}, Arc},
};
#[cfg(feature = "full")]
use std::time::Instant;
#[cfg(feature = "full")]
use walkdir::WalkDir;

// ─── XOR keys ────────────────────────────────────────────────────────────────

#[inline(always)]
fn toc_xor_key() -> [u8; 16] {
    const K: [u8; 16] = [
        0x8B,0xF8,0xEB,0xBA,0x9F,0xAB,0xEF,0xEB,
        0x5A,0xA7,0x50,0x64,0x54,0x47,0x70,0x70,
    ];
    let k = std::hint::black_box(K);
    let mut out = [0u8; 16];
    let mut i = 0usize;
    while i < 8  { out[i] = k[i] ^ 0x55; i += 1; }
    while i < 16 { out[i] = k[i] ^ 0xAA; i += 1; }
    out
}

#[inline(always)]
fn blob_xor_key() -> [u8; 16] {
    const K: [u8; 16] = [
        0x40,0xF1,0xAC,0x29,0x7D,0xE6,0xB4,0x08,
        0x3C,0xAE,0x60,0x94,0xD5,0x2B,0xE1,0x78,
    ];
    let k = std::hint::black_box(K);
    let mut out = [0u8; 16];
    let mut i = 0usize;
    while i < 8  { out[i] = k[i] ^ 0x33; i += 1; }
    while i < 16 { out[i] = k[i] ^ 0xCC; i += 1; }
    out
}

fn toc_xor(data: &[u8]) -> Vec<u8> {
    let key    = toc_xor_key();
    let key128 = u128::from_ne_bytes(key);
    let mut out = data.to_vec();
    let (prefix, chunks, suffix) = unsafe { out.align_to_mut::<u128>() };
    for (i, b) in prefix.iter_mut().enumerate() { *b ^= key[i]; }
    for chunk in chunks.iter_mut() { *chunk ^= key128; }
    let suf_off = prefix.len() + chunks.len() * 16;
    for (i, b) in suffix.iter_mut().enumerate() { *b ^= key[(suf_off + i) % 16]; }
    out
}

pub(crate) fn blob_xor_at(data: &[u8], blob_off: usize) -> Vec<u8> {
    let key   = blob_xor_key();
    let start = blob_off % 16;
    let mut rk = [0u8; 16];
    for i in 0..16 { rk[i] = key[(start + i) % 16]; }
    let key128 = u128::from_ne_bytes(rk);

    let mut out = data.to_vec();
    // SAFETY: u128 is valid for any bit pattern; align_to_mut is sound on owned Vec.
    let (prefix, chunks, suffix) = unsafe { out.align_to_mut::<u128>() };
    for (i, b) in prefix.iter_mut().enumerate() { *b ^= rk[i]; }
    for chunk in chunks.iter_mut() { *chunk ^= key128; }
    let suf_off = prefix.len() + chunks.len() * 16;
    for (i, b) in suffix.iter_mut().enumerate() { *b ^= rk[(suf_off + i) % 16]; }
    out
}

// ─── BsxBundle ────────────────────────────────────────────────────────────────

pub struct BsxBundle {
    pub data:       Vec<u8>,
    pub toc:        BsxToc,
    pub asset_map:  HashMap<String, usize>,
    pub track_map:  HashMap<String, usize>,
    bm_asset:       HashMap<String, usize>,
    bm_track:       HashMap<String, usize>,
    pool:           Option<Vec<String>>,
    /// Pre-decrypted binmap blob (entire region, XOR applied once at load).
    /// `None` when binmap is absent.
    plain_blob:     Option<Vec<u8>>,
}

impl BsxBundle {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        #[cfg(feature = "full")]
        {
            use memmap2::Mmap;
            let file = std::fs::File::open(path.as_ref())?;
            let mmap = unsafe { Mmap::map(&file)? };
            return Self::from_bytes(mmap.to_vec());
        }
        #[cfg(not(feature = "full"))]
        {
            let data = fs::read(path.as_ref())?;
            Self::from_bytes(data)
        }
    }

    pub fn from_bytes(data: Vec<u8>) -> anyhow::Result<Self> {
        let toc = load_toc(&data)?;
        let asset_map = toc.assets.iter().enumerate().map(|(i,e)| (e.name.clone(), i)).collect();
        let track_map = toc.tracks.iter().enumerate().map(|(i,t)| (t.path.clone(), i)).collect();
        let (bm_asset, bm_track, plain_blob) = if let Some(bm) = &toc.binmap {
            let start = bm.blob_start as usize;
            let end   = start + bm.blob_size as usize;
            let plain = if end <= data.len() {
                // Decrypt full blob in one pass — amortised cost across all asset reads.
                Some(blob_xor_at(&data[start..end], 0))
            } else {
                None
            };
            (bm.assets.iter().enumerate().map(|(i,e)| (e.name.clone(), i)).collect(),
             bm.tracks.iter().enumerate().map(|(i,t)| (t.path.clone(), i)).collect(),
             plain)
        } else { (HashMap::new(), HashMap::new(), None) };
        Ok(BsxBundle { data, toc, asset_map, track_map, bm_asset, bm_track, pool: None, plain_blob })
    }

    pub fn key(&mut self, key: &str) -> anyhow::Result<()> {
        let desc = self.toc.strings.as_ref()
            .or_else(|| self.toc.binmap.as_ref().and_then(|b| b.strings.as_ref()))
            .ok_or_else(|| anyhow::anyhow!("no string pool"))?;
        let start = HEADER_SIZE + desc.offset as usize;
        let end   = start + desc.size as usize;
        if end > self.data.len() { return Err(anyhow::anyhow!("pool OOB")); }
        self.pool = Some(strings::decode_pool(&self.data[start..end], key)?);
        Ok(())
    }

    pub fn string(&self, i: usize) -> anyhow::Result<&str> {
        let p = self.pool.as_ref().ok_or_else(|| anyhow::anyhow!("call key() first"))?;
        p.get(i).map(|s| s.as_str()).ok_or_else(|| anyhow::anyhow!("idx {i} OOB"))
    }

    pub fn strings_count(&self) -> anyhow::Result<usize> {
        self.pool.as_ref().map(|p| p.len()).ok_or_else(|| anyhow::anyhow!("call key() first"))
    }

    pub fn get(&self, path: &str) -> anyhow::Result<Vec<u8>> {
        let ep = self.resolve_path(path);
        if self.toc.binmap.is_some() {
            if let Some(&i) = self.bm_asset.get(ep) { return self.read_binmap_asset(i); }
            if let Some(&i) = self.bm_track.get(strip_audio_ext(ep)) { return self.read_binmap_track(i); }
        }
        if let Some(&i) = self.asset_map.get(ep) { return self.read_asset(i); }
        if let Some(&i) = self.track_map.get(strip_audio_ext(ep)) { return self.read_track(i); }
        Err(anyhow::anyhow!("not found: '{path}'"))
    }

    pub fn get_alias(&self, cat: &str, alias: &str) -> anyhow::Result<Vec<u8>> {
        let p = self.toc.bmap.get(cat).and_then(|c| c.get(alias))
            .ok_or_else(|| anyhow::anyhow!("alias '{alias}' not in '{cat}'"))?;
        self.get(p)
    }

    pub fn has(&self, path: &str) -> bool {
        let ep = self.resolve_path(path);
        self.bm_asset.contains_key(ep) || self.bm_track.contains_key(strip_audio_ext(ep))
        || self.asset_map.contains_key(ep) || self.track_map.contains_key(strip_audio_ext(ep))
    }

    /// Iterator over asset names — no allocation, used by decode_group in wasm.
    pub fn asset_names(&self) -> impl Iterator<Item = &str> {
        let bm = self.toc.binmap.as_ref().map(|b| b.assets.iter().map(|e| e.name.as_str()));
        let toc = self.toc.assets.iter().map(|e| e.name.as_str());
        // Return bm if present, otherwise toc. Use either/or via chain with empty.
        let use_bm = self.toc.binmap.is_some();
        bm.into_iter().flatten().filter(move |_| use_bm)
            .chain(toc.filter(move |_| !use_bm))
    }

    /// Iterator over track paths — no allocation, used by decode_group in wasm.
    pub fn track_names(&self) -> impl Iterator<Item = &str> {
        let bm = self.toc.binmap.as_ref().map(|b| b.tracks.iter().map(|t| t.path.as_str()));
        let toc = self.toc.tracks.iter().map(|t| t.path.as_str());
        let use_bm = self.toc.binmap.is_some();
        bm.into_iter().flatten().filter(move |_| use_bm)
            .chain(toc.filter(move |_| !use_bm))
    }

    pub fn list_all(&self) -> Vec<String> {
        if let Some(bm) = &self.toc.binmap {
            let mut v: Vec<_> = bm.assets.iter().map(|e| e.name.clone()).collect();
            for t in &bm.tracks { v.push(t.path.clone()); }
            return v;
        }
        let mut v: Vec<_> = self.toc.assets.iter().map(|e| e.name.clone()).collect();
        for t in &self.toc.tracks { v.push(t.path.clone()); }
        v
    }

    pub fn tracks(&self) -> Vec<String> {
        if let Some(bm) = &self.toc.binmap { return bm.tracks.iter().map(|t| t.path.clone()).collect(); }
        self.toc.tracks.iter().map(|t| t.path.clone()).collect()
    }

    pub fn categories(&self) -> Vec<String> { self.toc.bmap.keys().cloned().collect() }

    pub fn list_category(&self, c: &str) -> Vec<String> {
        self.toc.bmap.get(c).map(|m| m.keys().cloned().collect()).unwrap_or_default()
    }


    /// Packed byte size of the asset (alias kept for WASM API).
    pub fn size(&self, path: &str) -> u32 {
        self.packed_size(path)
    }

    /// Bundle filename hint — always empty unless set externally.
    pub fn filename(&self) -> &str { "" }

    /// Return a slice of the raw bundle bytes (for WASM string-pool access).
    pub fn data_slice(&self, start: usize, end: usize) -> anyhow::Result<&[u8]> {
        if end > self.data.len() { return Err(anyhow::anyhow!("data_slice OOB")); }
        Ok(&self.data[start..end])
    }

    /// Returns "IMG" | "AUDIO" | "BLOB" for a given asset path.
    pub fn asset_type(&self, path: &str) -> &'static str {
        let ep = self.resolve_path(path);
        if let Some(&i) = self.asset_map.get(ep) {
            return match &self.toc.assets[i].kind {
                AssetKind::TextureRaw{..} | AssetKind::TextureGpu{..} => "IMG",
                AssetKind::Audio{..} => "AUDIO",
                AssetKind::Mesh{..}  => "MESH",
                AssetKind::Blob => "BLOB",
            };
        }
        if let Some(bm) = &self.toc.binmap {
            if let Some(&i) = self.bm_asset.get(ep) {
                return match &bm.assets[i].kind {
                    AssetKind::TextureRaw{..} | AssetKind::TextureGpu{..} => "IMG",
                    AssetKind::Audio{..} => "AUDIO",
                    AssetKind::Mesh{..}  => "MESH",
                    AssetKind::Blob => "BLOB",
                };
            }
            if self.bm_track.contains_key(strip_audio_ext(ep)) { return "AUDIO"; }
        }
        if self.track_map.contains_key(strip_audio_ext(ep)) { return "AUDIO"; }
        "BLOB"
    }

    /// Returns (width, height) for texture assets, None otherwise.
    pub fn dimensions(&self, path: &str) -> Option<(u32, u32)> {
        let ep = self.resolve_path(path);
        let kind = if let Some(&i) = self.asset_map.get(ep) {
            Some(&self.toc.assets[i].kind)
        } else if let Some(bm) = &self.toc.binmap {
            self.bm_asset.get(ep).and_then(|&i| bm.assets.get(i).map(|e| &e.kind))
        } else { None };
        match kind? {
            AssetKind::TextureRaw { width, height } |
            AssetKind::TextureGpu { width, height } => Some((*width, *height)),
            _ => None,
        }
    }

    /// Packed byte size of the asset.
    pub fn packed_size(&self, path: &str) -> u32 {
        let ep   = self.resolve_path(path);
        let bare = strip_audio_ext(ep);
        if let Some(&i) = self.asset_map.get(ep) {
            return self.toc.assets[i].packed_size as u32;
        }
        if let Some(&i) = self.track_map.get(bare) {
            return self.toc.tracks[i].size as u32;
        }
        if let Some(bm) = &self.toc.binmap {
            if let Some(&i) = self.bm_asset.get(ep) { return bm.assets[i].size as u32; }
            if let Some(&i) = self.bm_track.get(bare) { return bm.tracks[i].size as u32; }
        }
        0
    }

    fn resolve_path<'a>(&self, path: &'a str) -> &'a str {
        if path.ends_with(".png") {
            let s = &path[..path.len() - 4];
            let has_s = self.bm_asset.contains_key(s) || self.asset_map.contains_key(s);
            let has_p = self.bm_asset.contains_key(path) || self.asset_map.contains_key(path);
            if has_s && !has_p { return s; }
        }
        path
    }

    fn read_asset(&self, idx: usize) -> anyhow::Result<Vec<u8>> {
        let e = &self.toc.assets[idx];
        let s   = HEADER_SIZE + e.offset as usize;
        let end = s + e.packed_size as usize;
        if end > self.data.len() { return Err(anyhow::anyhow!("'{}' OOB", e.name)); }
        let packed = &self.data[s..end];
        if crc32(packed) != e.crc32 { return Err(anyhow::anyhow!("CRC32 '{}' failed", e.name)); }
        let payload: Vec<u8> = match &e.packing {
            Packing::Raw  => packed.to_vec(),
            Packing::Lz4  => lz4_decompress(packed).map_err(|x| anyhow::anyhow!("lz4 '{}': {x}", e.name))?,
            Packing::Zstd => zstd_decompress(packed).map_err(|x| anyhow::anyhow!("zstd '{}': {x}", e.name))?,
        };
        asset::decode_rgba8(&payload, &e.kind)
            .map_err(|x| anyhow::anyhow!("decode '{}': {x}", e.name))
    }

    fn read_track(&self, idx: usize) -> anyhow::Result<Vec<u8>> {
        let t = &self.toc.tracks[idx];
        let s   = HEADER_SIZE + t.offset as usize;
        let end = s + t.size as usize;
        if end > self.data.len() { return Err(anyhow::anyhow!("track '{}' OOB", t.path)); }
        let raw = &self.data[s..end];
        codec::raw_opus_to_ogg(raw)
            .map_err(|x| anyhow::anyhow!("audio '{}': {x}", t.path))
    }

    fn binmap_bytes(&self, blob_off: u64, size: u64) -> anyhow::Result<Vec<u8>> {
        let bm    = self.toc.binmap.as_ref().unwrap();
        let start = blob_off as usize;
        let end   = start + size as usize;

        // Use pre-decrypted blob when available (fast path — no allocation).
        let slice: &[u8] = if let Some(ref pb) = self.plain_blob {
            if end > pb.len() { return Err(anyhow::anyhow!("binmap plain_blob OOB")); }
            &pb[start..end]
        } else {
            // Fallback: decrypt on the fly (should not happen after from_bytes).
            let abs = bm.blob_start as usize + start;
            let abs_end = abs + size as usize;
            if abs_end > self.data.len() { return Err(anyhow::anyhow!("binmap flat OOB")); }
            // SAFETY: only reached if plain_blob was not populated.
            return {
                let xored = blob_xor_at(&self.data[abs..abs_end], blob_off as usize);
                if let Some(dict) = &bm.dict {
                    #[cfg(feature = "full")]
                    {
                        let mut dec = zstd::bulk::Decompressor::with_dictionary(dict)
                            .context("dict decompressor init")?;
                        dec.decompress(&xored, 64 * 1024 * 1024)
                            .context("dict decompress binmap payload")
                    }
                    #[cfg(not(feature = "full"))]
                    {
                        stripped_zstd::decode_with_dict(&xored, dict)
                            .context("wasm binmap decompress")
                    }
                } else {
                    Ok(xored)
                }
            };
        };

        self.decompress_binmap_slice(slice, &bm.dict)
    }

    /// Decompress a binmap slice with an explicit optional dict.
    fn decompress_binmap_slice(&self, slice: &[u8], dict: &Option<Vec<u8>>)
        -> anyhow::Result<Vec<u8>>
    {
        if let Some(d) = dict {
            #[cfg(feature = "full")]
            {
                let mut dec = zstd::bulk::Decompressor::with_dictionary(d)
                    .context("dict decompressor init")?;
                return dec.decompress(slice, 64 * 1024 * 1024)
                    .context("dict decompress binmap payload");
            }
            #[cfg(not(feature = "full"))]
            {
                return stripped_zstd::decode_with_dict(slice, d)
                    .context("wasm binmap decompress");
            }
        }
        Ok(slice.to_vec())
    }

    fn read_binmap_asset(&self, idx: usize) -> anyhow::Result<Vec<u8>> {
        let bm = self.toc.binmap.as_ref().unwrap();
        let e  = &bm.assets[idx];
        let start = e.offset as usize;
        let end   = start + e.size as usize;
        let max_out = e.orig_size as usize + 64; // +64 B guard against rounding

        let slice: &[u8] = if let Some(ref pb) = self.plain_blob {
            if end > pb.len() { return Err(anyhow::anyhow!("'{}' plain_blob OOB", e.name)); }
            &pb[start..end]
        } else {
            let abs = bm.blob_start as usize + start;
            if abs + e.size as usize > self.data.len() {
                return Err(anyhow::anyhow!("'{}' OOB", e.name));
            }
            let xored = blob_xor_at(&self.data[abs..abs + e.size as usize], e.offset as usize);
            let payload = if let Some(d) = &bm.dict {
                #[cfg(feature = "full")]
                { zstd::bulk::Decompressor::with_dictionary(d)?.decompress(&xored, max_out)? }
                #[cfg(not(feature = "full"))]
                { stripped_zstd::decode_with_dict(&xored, d)? }
            } else { xored };
            return asset::decode_rgba8(&payload, &e.kind)
                .map_err(|x| anyhow::anyhow!("decode '{}': {x}", e.name));
        };

        let payload = if let Some(d) = &bm.dict {
            #[cfg(feature = "full")]
            {
                let mut dec = zstd::bulk::Decompressor::with_dictionary(d)
                    .context("dict decompressor")?;
                dec.decompress(slice, max_out).context("decompress asset")?
            }
            #[cfg(not(feature = "full"))]
            {
                stripped_zstd::decode_with_dict(slice, d).context("decompress asset")?
            }
        } else {
            slice.to_vec()
        };
        asset::decode_rgba8(&payload, &e.kind)
            .map_err(|x| anyhow::anyhow!("decode '{}': {x}", e.name))
    }

    fn read_binmap_track(&self, idx: usize) -> anyhow::Result<Vec<u8>> {
        let bm = self.toc.binmap.as_ref().unwrap();
        let t  = &bm.tracks[idx];
        let start = t.offset as usize;
        let end   = start + t.size as usize;
        let slice: &[u8] = if let Some(ref pb) = self.plain_blob {
            if end > pb.len() { return Err(anyhow::anyhow!("binmap track OOB")); }
            &pb[start..end]
        } else {
            let abs = bm.blob_start as usize + start;
            let abs_end = abs + t.size as usize;
            if abs_end > self.data.len() { return Err(anyhow::anyhow!("binmap track data OOB")); }
            // Temporary — should not happen after from_bytes.
            let xored = blob_xor_at(&self.data[abs..abs_end], t.offset as usize);
            let payload = self.decompress_binmap_slice(&xored, &bm.dict_audio)?;
            return codec::raw_opus_to_ogg(&payload)
                .map_err(|x| anyhow::anyhow!("audio '{}': {x}", t.path));
        };
        let payload = self.decompress_binmap_slice(slice, &bm.dict_audio)?;
        codec::raw_opus_to_ogg(&payload)
            .map_err(|x| anyhow::anyhow!("audio '{}': {x}", t.path))
    }
}

fn strip_audio_ext(path: &str) -> &str {
    if let Some(dot) = path.rfind('.') {
        if EXT_AUDIO.contains(&path[dot+1..].to_ascii_lowercase().as_str()) { return &path[..dot]; }
    }
    path
}

// ─── Binmap mode ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum BinmapMode { None, Flat }


// ─── index.bmap parser ────────────────────────────────────────────────────────

pub fn parse_index_bmap(content: &str) -> Vec<LoadGroup> {
    // Format: one chunk per line, entries separated by commas.
    // Entries may contain spaces (e.g. "Flying Beagle.mp3").
    // Lines starting with '#' are comments.
    content.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|line| LoadGroup {
            entries: line.split(',')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_string)
                .collect(),
        })
        .filter(|g| !g.entries.is_empty())
        .collect()
}

// ─── pack_core ────────────────────────────────────────────────────────────────

#[cfg(feature = "full")]
fn compress_ldm(data: &[u8], level: i32, dict: Option<&[u8]>) -> Vec<u8> {
    use zstd::zstd_safe as zs;
    let Some(mut cctx) = zs::CCtx::try_create() else {
        return zstd::encode_all(data, level).unwrap_or_else(|_| data.to_vec());
    };
    let _ = cctx.set_parameter(zs::CParameter::CompressionLevel(level));
    let _ = cctx.set_parameter(zs::CParameter::EnableLongDistanceMatching(true));
    let _ = cctx.set_parameter(zs::CParameter::LdmHashLog(26));
    let _ = cctx.set_parameter(zs::CParameter::WindowLog(27));
    if let Some(d) = dict.filter(|d| !d.is_empty()) {
        let _ = cctx.load_dictionary(d);
    }
    let bound = zs::compress_bound(data.len());
    let mut out = vec![0u8; bound];
    match cctx.compress2(&mut out, data) {
        Ok(n) if n < data.len() => { out.truncate(n); out }
        _ => data.to_vec(),
    }
}

#[cfg(feature = "full")]
pub fn pack_core(
    dir: &Path,
    out_path: &Path,
    preset: Preset,
    pool_path: Option<&Path>,
    pool_key: &str,
    legal: Option<&str>,
    binmap_mode: BinmapMode,
    channel_mode: format::ChannelMode,
    mesh_opt: bool,
    progress_cb: Option<Arc<dyn Fn(usize, usize, &str) + Send + Sync>>,
) -> anyhow::Result<PackResult> {
    let t0 = Instant::now();

    let mut all_files: Vec<PathBuf> = WalkDir::new(dir).into_iter()
        .filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).map(|e| e.into_path())
        .filter(|p| !is_system_file(p.strip_prefix(dir).unwrap_or(p))).collect();
    all_files.sort();

    // Separate bmap files (exclude index.bmap — parsed separately)
    let (bmap_files, rest): (Vec<_>, Vec<_>) = all_files.into_iter().partition(|p| {
        file_ext(p) == "bmap"
            && p.file_name().map(|n| n != "index.bmap").unwrap_or(true)
    });
    let (_, rest): (Vec<_>, Vec<_>) = rest.into_iter().partition(|p| file_ext(p) == "fx");
    let (_, rest): (Vec<_>, Vec<_>) = rest.into_iter().partition(|p| file_ext(p) == "pool");
    let (_, rest): (Vec<_>, Vec<_>) = rest.into_iter()
        .partition(|p| file_ext(p) == "bmap"); // catch index.bmap in rest
    let (_, asset_files): (Vec<_>, Vec<_>) = rest.into_iter()
        .partition(|p| EXT_AUDIO.contains(&file_ext(p).as_str()));

    // Parse index.bmap
    let index_bmap_path = dir.join("index.bmap");
    let toc_index = if index_bmap_path.exists() {
        let content = fs::read_to_string(&index_bmap_path).unwrap_or_default();
        parse_index_bmap(&content)
    } else {
        vec![]
    };

    let mut combined_bmap: BmapIndex = HashMap::new();
    for bp in &bmap_files { merge_bmap(&mut combined_bmap, parse_bmap_file(bp, dir)); }

    let mut f = std::io::BufWriter::new(fs::File::create(out_path)
        .with_context(|| format!("create {:?}", out_path))?);
    f.write_all(&[0u8; HEADER_SIZE]).context("write header")?;
    let mut data_cursor: u64 = 0;

    let total_assets = asset_files.len();
    let asset_counter = Arc::new(AtomicUsize::new(0));

    struct NamedAsset { name: String, packed: asset::PackedAsset }

    let named_results: Vec<NamedAsset> = {
        let mut results: Vec<(usize, NamedAsset)> = asset_files.par_iter().enumerate()
            .map(|(idx, file)| {
                let raw = match fs::read(file) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("  skip {:?}: {e}", file);
                        return (idx, NamedAsset {
                            name: String::new(),
                            packed: asset::PackedAsset {
                                kind: format::AssetKind::Blob,
                                payload: vec![],
                                packing: Packing::Raw,
                                orig_size: 0,
                            },
                        });
                    }
                };
                let class = classify(file);
                let name  = file.strip_prefix(dir)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default().to_string();
                let packed = pack_asset(&raw, class, &name, preset, channel_mode, mesh_opt);
                let n = asset_counter.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(ref cb) = progress_cb { cb(n, total_assets, &name); }
                (idx, NamedAsset { name, packed })
            })
            .collect();
        results.sort_by_key(|(i, _)| *i);
        results.into_iter()
            .map(|(_, a)| a)
            .filter(|a| !a.name.is_empty() && !a.packed.payload.is_empty())
            .collect()
    };

    let s_orig: u64 = named_results.iter().map(|a| a.packed.orig_size).sum();
    let s_tex:  u64 = named_results.iter().filter(|a|
        matches!(a.packed.kind, format::AssetKind::TextureRaw{..}|format::AssetKind::TextureGpu{..})
    ).count() as u64;
    let n_assets = named_results.len() as u64;

    let audio_data_offset: u64 = named_results.iter().map(|a| a.packed.payload.len() as u64).sum();
    let pb_fn: &dyn Fn(&str) = &|n| {
        if let Some(ref cb) = progress_cb { cb(total_assets, total_assets, &format!("audio: {n}")); }
    };
    let track_blobs = pack_audio_tracks(dir, audio_data_offset, pb_fn)?;
    let n_tracks = track_blobs.tracks.len() as u64;
    let s_orig = s_orig + track_blobs.blob_data.len() as u64;



    let toc_level = if preset.is_lz4() { 8 } else { preset.zstd_level().clamp(1, 22) };

    let (toc, n_strings) = if binmap_mode == BinmapMode::None {
        let mut asset_entries: Vec<AssetEntry> = Vec::new();
        for a in named_results {
            let csum = crc32(&a.packed.payload);
            let psz  = a.packed.payload.len() as u64;
            asset_entries.push(AssetEntry {
                name: a.name, kind: a.packed.kind, offset: data_cursor,
                packed_size: psz, orig_size: a.packed.orig_size,
                packing: a.packed.packing, crc32: csum,
            });
            f.write_all(&a.packed.payload).context("write asset")?;
            data_cursor += psz;
        }
        f.write_all(&track_blobs.blob_data).context("write audio")?;
        data_cursor += track_blobs.blob_data.len() as u64;

        let mut sd = None; let mut ns = 0u32;
        if let Some(pp) = pool_path {
            let ps = strings::load_pool_file(pp)?;
            ns = ps.len() as u32;
            let enc = strings::encode_pool(&ps, pool_key)?;
            sd = Some(StringPoolDesc { offset: data_cursor, size: enc.len() as u64, count: ns });
            f.write_all(&enc)?;
            data_cursor += enc.len() as u64;
        }
        (BsxToc {
            assets: asset_entries, tracks: track_blobs.tracks,
            strings: sd, bmap: combined_bmap, binmap: None, index: toc_index,
        }, ns)
    } else {
        // Flat binmap — collect all payloads, train zstd dict, compress individually.
        let level = toc_level;

        // Gather texture payloads (QOI) and audio payloads (RawOpus) separately.
        let tex_payloads: Vec<Vec<u8>> = named_results.iter()
            .filter(|a| matches!(a.packed.kind,
                format::AssetKind::TextureRaw{..} | format::AssetKind::TextureGpu{..}))
            .map(|a| a.packed.payload.clone())
            .collect();

        let chunk_size = (track_blobs.blob_data.len()
            / track_blobs.tracks.len().max(1)).max(1);
        let audio_payloads: Vec<Vec<u8>> = track_blobs.blob_data
            .chunks(chunk_size)
            .map(|c| c.to_vec())
            .collect();

        // Adaptive dict size: ~1 % of total sample data, clamped 64 KB – 512 KB.
        fn make_dict(samples: &[Vec<u8>]) -> Vec<u8> {
            if samples.is_empty() { return vec![]; }
            let total: usize = samples.iter().map(|s| s.len()).sum();
            let dict_size = (total / 100).clamp(64 * 1024, 512 * 1024);
            if total < dict_size { return vec![]; }
            let refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
            zstd::dict::from_samples(&refs, dict_size).unwrap_or_default()
        }

        let dict       = make_dict(&tex_payloads);
        let dict_audio = make_dict(&audio_payloads);

        let mut blob: Vec<u8> = Vec::new();
        let mut bm_assets: Vec<BinmapAssetEntry> = Vec::new();
        let mut bm_tracks: Vec<BinmapTrackEntry> = Vec::new();

        // Sort: textures first (QOI payloads cluster well), then everything else.
        let mut named_results = named_results;
        named_results.sort_by_key(|a| match &a.packed.kind {
            format::AssetKind::TextureRaw{..} | format::AssetKind::TextureGpu{..} => 0u8,
            format::AssetKind::Mesh{..}  => 1,
            format::AssetKind::Blob => 2,
            format::AssetKind::Audio{..} => 3,
        });

        // Compress assets in parallel, preserve order via index.
        const COMPRESS_THRESHOLD: usize = 256;
        let compressed_assets: Vec<(String, format::AssetKind, u64, Vec<u8>)> =
            named_results.into_par_iter().map(|a| {
                let raw: Vec<u8> = {
                    let pl = a.packed.payload;
                    match a.packed.packing {
                        Packing::Raw  => pl,
                        Packing::Lz4  => lz4_decompress(&pl).unwrap_or(pl),
                        Packing::Zstd => zstd_decompress(&pl).ok().unwrap_or(pl),
                    }
                };
                let is_tex = matches!(a.packed.kind,
                    format::AssetKind::TextureRaw{..} | format::AssetKind::TextureGpu{..});
                let payload = if raw.len() < COMPRESS_THRESHOLD {
                    raw
                } else if is_tex {
                    compress_ldm(&raw, level, Some(dict.as_slice()))
                } else {
                    compress_ldm(&raw, level, Some(dict_audio.as_slice()))
                };
                (a.name, a.packed.kind, a.packed.orig_size, payload)
            }).collect();

        for (name, kind, orig_size, payload) in compressed_assets {
            let off = blob.len() as u64;
            let sz  = payload.len() as u64;
            bm_assets.push(BinmapAssetEntry { name, kind, offset: off, size: sz, orig_size });
            blob.extend_from_slice(&payload);
        }

        // Audio tracks: compress in parallel, concat in order.
        let track_slices: Vec<&[u8]> = {
            let mut cursor = 0usize;
            track_blobs.tracks.iter().map(|t| {
                let s = &track_blobs.blob_data[cursor..cursor + t.size as usize];
                cursor += t.size as usize;
                s
            }).collect()
        };
        let compressed_tracks: Vec<Vec<u8>> = track_slices.into_par_iter()
            .map(|raw| {
                if raw.len() < COMPRESS_THRESHOLD {
                    raw.to_vec()
                } else {
                    compress_ldm(raw, level, Some(dict_audio.as_slice()))
                }
            }).collect();

        for (t, payload) in track_blobs.tracks.iter().zip(compressed_tracks) {
            let off = blob.len() as u64;
            let sz  = payload.len() as u64;
            bm_tracks.push(BinmapTrackEntry {
                path: t.path.clone(), offset: off, size: sz,
                channels: t.channels, sample_rate: t.sample_rate,
                total_samples: t.total_samples, bitrate_kbps: t.bitrate_kbps,
                fx: t.fx.clone(),
            });
            blob.extend_from_slice(&payload);
        }

        let xblob = blob_xor_at(&blob, 0);
        let blob_start = HEADER_SIZE as u64 + data_cursor;
        f.write_all(&xblob).context("write flat blob")?;
        data_cursor += xblob.len() as u64;

        let mut sd = None; let mut ns = 0u32;
        if let Some(pp) = pool_path {
            let ps = strings::load_pool_file(pp)?;
            ns = ps.len() as u32;
            let enc = strings::encode_pool(&ps, pool_key)?;
            sd = Some(StringPoolDesc { offset: data_cursor, size: enc.len() as u64, count: ns });
            f.write_all(&enc)?;
            data_cursor += enc.len() as u64;
        }

        let bm_toc = BinmapToc {
            assets: bm_assets, tracks: bm_tracks,
            blob_start, blob_size: xblob.len() as u64, strings: sd,
            dict:       if dict.is_empty()       { None } else { Some(dict) },
            dict_audio: if dict_audio.is_empty() { None } else { Some(dict_audio) },
        };
        (BsxToc {
            assets: vec![], tracks: vec![], strings: None,
            bmap: combined_bmap, binmap: Some(bm_toc), index: toc_index,
        }, ns)
    };

    // Serialize and write TOC
    let toc_raw  = postcard::to_allocvec(&toc).context("postcard TOC")?;
    let toc_comp = toc_xor(&stripped_zstd::encode(&toc_raw, toc_level).context("compress TOC")?);
    let toc_sz   = toc_comp.len() as u32;
    let toc_off  = HEADER_SIZE as u64 + data_cursor;
    f.write_all(&toc_comp).context("write TOC")?;

    if let Some(notice) = legal {
        let dir_hash = crc32(dir.to_string_lossy().as_bytes());
        let tag = format!("\n<sec>{dir_hash:08X}</sec>\n<copyright>{notice}</copyright>\n");
        f.write_all(tag.as_bytes())?;
    }
    f.flush()?;

    let mut file = f.into_inner().map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut hdr = [0u8; HEADER_SIZE];
    hdr[0..4].copy_from_slice(MAGIC);
    hdr[4..6].copy_from_slice(&VERSION.to_le_bytes());
    hdr[8..16].copy_from_slice(&toc_off.to_le_bytes());
    hdr[16..20].copy_from_slice(&toc_sz.to_le_bytes());
    let c = crc32(&hdr[0..24]).to_le_bytes();
    hdr[24..28].copy_from_slice(&c);
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&hdr)?;

    let packed_bytes = fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
    Ok(PackResult {
        assets: n_assets, textures: s_tex, tracks: n_tracks, strings: n_strings,
        orig_bytes: s_orig, packed_bytes, elapsed_ms: t0.elapsed().as_millis() as u64,
    })
}

// ─── depack_core ──────────────────────────────────────────────────────────────

#[cfg(feature = "full")]
pub fn depack_core(bsx_path: &Path, out_dir: &Path, _conv: bool) -> anyhow::Result<u64> {

    let data   = fs::read(bsx_path)?;
    let bundle = BsxBundle::from_bytes(data)?;
    fs::create_dir_all(out_dir)?;
    let count = Arc::new(AtomicU64::new(0));
    use dashmap::DashSet;
    let created: Arc<DashSet<PathBuf>> = Arc::new(DashSet::new());

    // Normal assets — parallel
    if !bundle.toc.assets.is_empty() {
        let c = count.clone();
        let mut norm_order: Vec<usize> = (0..bundle.toc.assets.len()).collect();
        norm_order.sort_unstable_by_key(|&i| std::cmp::Reverse(bundle.toc.assets[i].orig_size));
        norm_order.par_iter().try_for_each(|&i| -> anyhow::Result<()> {
            let e = &bundle.toc.assets[i];
            let s   = HEADER_SIZE + e.offset as usize;
            let end = s + e.packed_size as usize;
            if end > bundle.data.len() { return Ok(()); }
            let packed = &bundle.data[s..end];
            let payload: Vec<u8> = match &e.packing {
                Packing::Raw  => packed.to_vec(),
                Packing::Lz4  => lz4_decompress(packed).unwrap_or_default(),
                Packing::Zstd => zstd_decompress(packed).unwrap_or_default(),
            };
            let out_bytes = match &e.kind {
                AssetKind::TextureRaw { width, height } => {
                    let rgba = asset::decode_rgba8(&payload, &e.kind)
                        .map_err(|x| anyhow::anyhow!("{x}"))?;
                    let img = image::RgbaImage::from_raw(*width, *height, rgba)
                        .ok_or_else(|| anyhow::anyhow!("RgbaImage failed for '{}'", e.name))?;
                    let out_bytes_rgba = img.into_raw();
                    let mut buf = Vec::new();
                    {
                        use png::{BitDepth, ColorType, Compression, Encoder};
                        let mut enc = Encoder::new(&mut buf, *width, *height);
                        enc.set_color(ColorType::Rgba);
                        enc.set_depth(BitDepth::Eight);
                        enc.set_compression(Compression::Fast);
                        enc.set_filter(png::FilterType::NoFilter);
                        let mut wr = enc.write_header()
                            .map_err(|x| anyhow::anyhow!("png header '{}': {x}", e.name))?;
                        wr.write_image_data(&out_bytes_rgba)
                            .map_err(|x| anyhow::anyhow!("png data '{}': {x}", e.name))?;
                    }
                    buf
                }
                _ => payload,
            };
            let out_name = output_ext(&e.name, &e.kind);
            let dest = out_dir.join(&out_name);
            if let Some(p) = dest.parent() {
                if !created.contains(p) {
                    let _ = fs::create_dir_all(p);
                    created.insert(p.to_path_buf());
                }
            }
            fs::write(&dest, &out_bytes)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?;
    }

    // Audio tracks — parallel
    if !bundle.toc.tracks.is_empty() {
        let c = count.clone();
        let created = created.clone();
        bundle.toc.tracks.par_iter().try_for_each(|t| -> anyhow::Result<()> {
            let s   = HEADER_SIZE + t.offset as usize;
            let end = s + t.size as usize;
            if end > bundle.data.len() { return Ok(()); }
            let raw = &bundle.data[s..end];
            let ogg = codec::raw_opus_to_ogg(raw).unwrap_or_else(|_| raw.to_vec());
            let dest = out_dir.join(format!("{}.ogg", t.path));
            if let Some(p) = dest.parent() {
                if !created.contains(p) {
                    let _ = fs::create_dir_all(p);
                    created.insert(p.to_path_buf());
                }
            }
            fs::write(&dest, &ogg)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?;
    }

    // Binmap — parallel
    // ── Thread-local dict decompressor pool ───────────────────────────────────
    // One Decompressor per rayon thread, keyed by dict fingerprint (first 8 bytes).
    // SAFETY: BsxBundle outlives all par_iter closures; dict pointer stays valid.
    #[cfg(feature = "full")]
    use std::cell::RefCell;
    #[cfg(feature = "full")]
    thread_local! {
        static CACHED_DEC: RefCell<Option<(u64, zstd::bulk::Decompressor<'static>)>> =
            RefCell::new(None);
    }

    #[cfg(feature = "full")]
    fn decompress_cached(dict: &[u8], data: &[u8], max_out: usize)
        -> anyhow::Result<Vec<u8>>
    {
        let fp = if dict.len() >= 8 {
            u64::from_le_bytes(dict[..8].try_into().unwrap())
        } else { 0 };
        CACHED_DEC.with(|slot| {
            let mut s = slot.borrow_mut();
            let stale = s.as_ref().map(|(f, _)| *f != fp).unwrap_or(true);
            if stale {
                let d: &'static [u8] = unsafe {
                    std::slice::from_raw_parts(dict.as_ptr(), dict.len())
                };
                let dec = zstd::bulk::Decompressor::with_dictionary(d)
                    .context("thread_local dict decomp init")?;
                *s = Some((fp, dec));
            }
            s.as_mut().unwrap().1
                .decompress(data, max_out)
                .context("thread_local dict decompress")
        })
    }

    if let Some(bm) = &bundle.toc.binmap {
        let c = count.clone();
        let created = created.clone();
        // Sort largest-first for optimal rayon work-stealing.
        let mut asset_order: Vec<usize> = (0..bm.assets.len()).collect();
        asset_order.sort_unstable_by_key(|&i| std::cmp::Reverse(bm.assets[i].orig_size));

        asset_order.par_iter().try_for_each(|&i| -> anyhow::Result<()> {
            let e = &bm.assets[i];
            let start = e.offset as usize;
            let end   = start + e.size as usize;
            let slice = bundle.plain_blob.as_ref()
                .and_then(|pb| pb.get(start..end))
                .ok_or_else(|| anyhow::anyhow!("binmap asset OOB '{}'", e.name))?;
            let payload: Vec<u8> = if let Some(dict) = bm.dict.as_deref() {
                #[cfg(feature = "full")]
                { decompress_cached(dict, slice, e.orig_size as usize + 64)? }
                #[cfg(not(feature = "full"))]
                { stripped_zstd::decode_with_dict(slice, dict)? }
            } else {
                slice.to_vec()
            };
            let out_bytes = match &e.kind {
                AssetKind::TextureRaw { width, height } => {

                    let rgba = asset::decode_rgba8(&payload, &e.kind)
                        .map_err(|x| anyhow::anyhow!("{x}"))?;
                    let img = image::RgbaImage::from_raw(*width, *height, rgba)
                        .ok_or_else(|| anyhow::anyhow!("RgbaImage failed for '{}'", e.name))?;
                    let out_bytes_rgba = img.into_raw();
                    let mut buf = Vec::new();
                    {
                        use png::{BitDepth, ColorType, Compression, Encoder};
                        let mut enc = Encoder::new(&mut buf, *width, *height);
                        enc.set_color(ColorType::Rgba);
                        enc.set_depth(BitDepth::Eight);
                        enc.set_compression(Compression::Fast);
                        enc.set_filter(png::FilterType::NoFilter);
                        let mut wr = enc.write_header()
                            .map_err(|x| anyhow::anyhow!("png header '{}': {x}", e.name))?;
                        wr.write_image_data(&out_bytes_rgba)
                            .map_err(|x| anyhow::anyhow!("png data '{}': {x}", e.name))?;
                    }
                    buf
                }
                _ => payload,
            };
            let out_name = output_ext(&e.name, &e.kind);
            let dest = out_dir.join(&out_name);
            if let Some(p) = dest.parent() {
                if !created.contains(p) {
                    let _ = fs::create_dir_all(p);
                    created.insert(p.to_path_buf());
                }
            }
            fs::write(&dest, &out_bytes)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?;

        bm.tracks.par_iter().try_for_each(|t| -> anyhow::Result<()> {
            let start = t.offset as usize;
            let end   = start + t.size as usize;
            let slice = bundle.plain_blob.as_ref()
                .and_then(|pb| pb.get(start..end))
                .ok_or_else(|| anyhow::anyhow!("binmap track OOB '{}'", t.path))?;
            let payload: Vec<u8> = if let Some(dict) = bm.dict_audio.as_deref() {
                #[cfg(feature = "full")]
                { decompress_cached(dict, slice, t.size as usize * 4)? }
                #[cfg(not(feature = "full"))]
                { stripped_zstd::decode_with_dict(slice, dict)? }
            } else {
                slice.to_vec()
            };
            let ogg = codec::raw_opus_to_ogg(&payload).unwrap_or_else(|_| payload.clone());
            let dest = out_dir.join(format!("{}.ogg", t.path));
            if let Some(p) = dest.parent() {
                if !created.contains(p) {
                    let _ = fs::create_dir_all(p);
                    created.insert(p.to_path_buf());
                }
            }
            fs::write(&dest, &ogg)?;
            count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?;
    }

    // String pool passthrough
    let sd = bundle.toc.strings.as_ref()
        .or_else(|| bundle.toc.binmap.as_ref().and_then(|b| b.strings.as_ref()));
    if let Some(desc) = sd {
        let s = HEADER_SIZE + desc.offset as usize;
        let e = s + desc.size as usize;
        if e <= bundle.data.len() {
            fs::write(out_dir.join("strings.bsms"), &bundle.data[s..e])?;
        }
    }
    if !bundle.toc.bmap.is_empty() {
        fs::write(out_dir.join("bundle.bmap"),
            serde_json::to_string_pretty(&bundle.toc.bmap).unwrap_or_default())?;
    }
    Ok(count.load(Ordering::Relaxed))
}

// ─── info / load_toc ─────────────────────────────────────────────────────────

pub struct BsxInfo {
    pub file_size:   u64,
    pub toc:         BsxToc,
    pub toc_raw_sz:  usize,
    pub toc_comp_sz: usize,
}

#[cfg(feature = "full")]
pub fn info_core(bsx_path: &Path) -> anyhow::Result<BsxInfo> {
    let buf       = fs::read(bsx_path)?;
    let file_size = fs::metadata(bsx_path).map(|m| m.len()).unwrap_or(0);
    let to = u64::from_le_bytes(buf[8..16].try_into().unwrap()) as usize;
    let ts = u32::from_le_bytes(buf[16..20].try_into().unwrap()) as usize;
    let toc_raw = stripped_zstd::decode_any(&toc_xor(&buf[to..to+ts]))?;
    let toc: BsxToc = postcard::from_bytes(&toc_raw).context("TOC")?;
    Ok(BsxInfo { file_size, toc, toc_raw_sz: toc_raw.len(), toc_comp_sz: ts })
}

pub fn load_toc(buf: &[u8]) -> anyhow::Result<BsxToc> {
    if buf.len() < HEADER_SIZE { return Err(anyhow::anyhow!("file too small")); }
    if &buf[0..4] != MAGIC { return Err(anyhow::anyhow!("bad magic")); }
    let fv = u16::from_le_bytes(buf[4..6].try_into().unwrap());
    if fv > VERSION { return Err(anyhow::anyhow!("bundle v{fv} > reader v{VERSION}")); }
    let sc = u32::from_le_bytes(buf[24..28].try_into().unwrap());
    if sc != crc32(&buf[0..24]) { return Err(anyhow::anyhow!("header CRC32 mismatch")); }
    let to = u64::from_le_bytes(buf[8..16].try_into().unwrap()) as usize;
    let ts = u32::from_le_bytes(buf[16..20].try_into().unwrap()) as usize;
    if to + ts > buf.len() { return Err(anyhow::anyhow!("TOC OOB")); }
    let raw = stripped_zstd::decode_any(&toc_xor(&buf[to..to+ts]))?;
    Ok(postcard::from_bytes(&raw).context("TOC deserialize")?)
}

#[cfg(feature = "full")]
fn parse_bmap_file(p: &Path, root: &Path) -> BmapIndex {
    let c = match fs::read_to_string(p) { Ok(s) => s, Err(_) => return HashMap::new() };
    let raw: HashMap<String, HashMap<String, String>> = match serde_json::from_str(&c) {
        Ok(v) => v, Err(_) => return HashMap::new(),
    };
    let rel = p.parent().unwrap_or(Path::new("")).strip_prefix(root).unwrap_or(Path::new(""))
        .to_string_lossy().replace('\\', "/");
    let mut m: BmapIndex = HashMap::new();
    for (cat, map) in raw {
        let e = m.entry(cat.clone()).or_default();
        for (f, a) in map {
            e.insert(a, if rel.is_empty() { format!("{cat}/{f}") } else { format!("{rel}/{cat}/{f}") });
        }
    }
    m
}

#[cfg(feature = "full")]
fn merge_bmap(dst: &mut BmapIndex, src: BmapIndex) {
    for (cat, aliases) in src { dst.entry(cat).or_default().extend(aliases); }
}

pub use util::human;