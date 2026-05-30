//! WASM bindings for BSX v2
//!
//! Public API:
//!
//!   const bsx = await BSX.load(arrayBuffer)
//!
//!   bsx.has(path)
//!   bsx.list()
//!   bsx.type(path)       → "IMG" | "AUDIO" | "BLOB"
//!   bsx.size(path)
//!   bsx.get(path)        → ImageData | Uint8Array
//!   bsx.url(path)        → object URL  (cached — same URL on repeated calls)
//!
//!   bsx.key(password)    → number
//!   bsx.string(index)    → string | undefined
//!
//!   bsx.index            → string[][]
//!
//! ── index.bmap streaming ──────────────────────────────────────────────────
//!
//!   bsx.chunks           → total number of chunks (0 = no index.bmap)
//!   bsx.chunk()          → Array<string> of paths now ready, or null if done

#![cfg(feature = "wasm")]

use std::collections::HashMap;

use wasm_bindgen::prelude::*;
use wasm_bindgen::Clamped;

use js_sys::{Array, Uint8Array};
use web_sys::{Blob, BlobPropertyBag, ImageData, Url};

use crate::{
    BsxBundle,
    asset::encode_rgba_to_png,
    format::{HEADER_SIZE, LoadGroup},
};

fn jerr(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

// ─── caches ───────────────────────────────────────────────────────────────────

/// Decoded bytes: RGBA8 for textures, OGG for audio, raw for blobs.
/// Populated eagerly for chunk-0 (and each subsequent chunk() call),
/// and lazily on get()/url() for anything not in index.bmap.
type ByteCache = HashMap<String, Vec<u8>>;

/// Object URL cache — avoids re-encoding PNG and re-creating Blob on every
/// url() call for the same asset.  Entries are revoked when BSX is dropped.
type UrlCache = HashMap<String, String>;

// ─── BSX ─────────────────────────────────────────────────────────────────────

#[wasm_bindgen]
pub struct BSX {
    inner:       BsxBundle,
    bytes:       ByteCache,
    urls:        UrlCache,
    next_chunk:  usize,
    index_val:   JsValue,
    string_pool: Option<Vec<String>>,
}

// ─── Drop: revoke all object URLs ─────────────────────────────────────────────

impl Drop for BSX {
    fn drop(&mut self) {
        for url in self.urls.values() {
            let _ = Url::revoke_object_url(url);
        }
    }
}

// ─── chunk helpers ────────────────────────────────────────────────────────────

/// Decode one LoadGroup into the byte cache.
///
/// Iterates group entries directly against the bundle's internal hashmaps —
/// no list_all() allocation, no Vec<String>.
///
/// Directory entries (ending with '/') are expanded by scanning the bundle's
/// asset_map and track_map keys, which are already interned.
fn decode_group(group: &LoadGroup, bundle: &BsxBundle, cache: &mut ByteCache) {
    for entry in &group.entries {
        if entry.ends_with('/') {
            // Expand directory: check every asset/track key for the prefix.
            // bundle.asset_names() / track_names() avoids a full list_all().
            for name in bundle.asset_names().chain(bundle.track_names()) {
                if name.starts_with(entry.as_str()) && !cache.contains_key(name) {
                    if let Ok(bytes) = bundle.get(name) {
                        cache.insert(name.to_string(), bytes);
                    }
                }
            }
        } else if !cache.contains_key(entry.as_str()) {
            if let Ok(bytes) = bundle.get(entry) {
                cache.insert(entry.clone(), bytes);
            }
        }
    }
}

/// Paths expanded from a LoadGroup (for returning to JS).
/// Same logic as decode_group but just collects names.
fn group_paths(group: &LoadGroup, bundle: &BsxBundle) -> Vec<String> {
    let mut out = Vec::new();
    for entry in &group.entries {
        if entry.ends_with('/') {
            for name in bundle.asset_names().chain(bundle.track_names()) {
                if name.starts_with(entry.as_str()) {
                    out.push(name.to_string());
                }
            }
        } else {
            out.push(entry.clone());
        }
    }
    out
}

// ─── url() helper ─────────────────────────────────────────────────────────────

fn make_object_url(bytes: &[u8], mime: &str) -> Result<String, JsValue> {
    let arr   = Uint8Array::from(bytes);
    let parts = Array::new();
    parts.push(&arr);
    let blob = Blob::new_with_u8_array_sequence_and_options(
        &parts,
        BlobPropertyBag::new().type_(mime),
    )?;
    Url::create_object_url_with_blob(&blob)
}

// ─── impl ─────────────────────────────────────────────────────────────────────

#[wasm_bindgen]
impl BSX {

    // ── load ──────────────────────────────────────────────────────────────────

    #[wasm_bindgen(js_name = load)]
    pub async fn load(buffer: js_sys::ArrayBuffer) -> Result<BSX, JsValue> {
        let bytes  = Uint8Array::new(&buffer).to_vec();
        let bundle = BsxBundle::from_bytes(bytes).map_err(jerr)?;

        let index_val = serde_wasm_bindgen::to_value(
            &bundle.toc.index.iter().map(|g| g.entries.clone()).collect::<Vec<_>>()
        )?;

        let mut byte_cache = ByteCache::new();
        let mut next_chunk = 0usize;

        if !bundle.toc.index.is_empty() {
            decode_group(&bundle.toc.index[0], &bundle, &mut byte_cache);
            next_chunk = 1;
        }

        Ok(BSX {
            inner:      bundle,
            bytes:      byte_cache,
            urls:       UrlCache::new(),
            next_chunk,
                index_val,
            string_pool: None,
        })
    }

    // ── streaming ─────────────────────────────────────────────────────────────

    /// Total chunks from index.bmap (0 if absent).
    #[wasm_bindgen(getter)]
    pub fn chunks(&self) -> u32 {
        self.inner.toc.index.len() as u32
    }

    /// Decode the next chunk into cache.
    /// Returns Array<string> of paths now available, or null if all done.
    #[wasm_bindgen(js_name = chunk)]
    pub fn chunk(&mut self) -> JsValue {
        if self.next_chunk >= self.inner.toc.index.len() {
            return JsValue::null();
        }
        let group = self.inner.toc.index[self.next_chunk].clone();
        decode_group(&group, &self.inner, &mut self.bytes);
        let paths: Array = group_paths(&group, &self.inner)
            .iter()
            .map(|p| JsValue::from_str(p))
            .collect();
        self.next_chunk += 1;
        paths.into()
    }

    // ── getters ───────────────────────────────────────────────────────────────

    #[wasm_bindgen(getter)]
    pub fn index(&self) -> JsValue { self.index_val.clone() }

    // ── basic API ─────────────────────────────────────────────────────────────

    #[wasm_bindgen]
    pub fn has(&self, path: &str) -> bool { self.inner.has(path) }

    #[wasm_bindgen]
    pub fn list(&self) -> Array {
        self.inner.list_all().into_iter().map(|s| JsValue::from_str(&s)).collect()
    }

    #[wasm_bindgen(js_name = type)]
    pub fn asset_type(&self, path: &str) -> String {
        match self.inner.asset_type(path) {
            "IMG"   => "IMG".into(),
            "AUDIO" => "AUDIO".into(),
            _       => "BLOB".into(),
        }
    }

    #[wasm_bindgen]
    pub fn size(&self, path: &str) -> u32 { self.inner.size(path) as u32 }

    // ── get() ─────────────────────────────────────────────────────────────────

    #[wasm_bindgen]
    pub fn get(&self, path: &str) -> Result<JsValue, JsValue> {
        let bytes = match self.bytes.get(path) {
            Some(b) => b.clone(),
            None    => self.inner.get(path).map_err(jerr)?,
        };
        match self.inner.asset_type(path) {
            "IMG" => {
                let (w, h) = self.inner.dimensions(path)
                    .ok_or_else(|| JsValue::from_str("no dimensions"))?;
                Ok(ImageData::new_with_u8_clamped_array_and_sh(Clamped(&bytes), w, h)?.into())
            }
            _ => Ok(Uint8Array::from(&bytes[..]).into()),
        }
    }

    // ── url() — cached object URL ─────────────────────────────────────────────
    //
    // First call for a given path: encode (PNG for IMG, passthrough for AUDIO/BLOB),
    // create Blob, create object URL, store in url_cache.
    // Subsequent calls: return cached URL instantly — no re-encoding, no new Blob.

    #[wasm_bindgen]
    pub fn url(&mut self, path: &str) -> Result<String, JsValue> {
        // Cache hit — free.
        if let Some(u) = self.urls.get(path) {
            return Ok(u.clone());
        }

        let bytes = match self.bytes.get(path) {
            Some(b) => b.clone(),
            None    => self.inner.get(path).map_err(jerr)?,
        };

        let url = match self.inner.asset_type(path) {
            "IMG" => {
                let (w, h) = self.inner.dimensions(path)
                    .ok_or_else(|| JsValue::from_str("no dimensions"))?;
                // encode_rgba_to_png uses Compression::Fast (level 1).
                let png = encode_rgba_to_png(&bytes, w, h).map_err(jerr)?;
                make_object_url(&png, "image/png")?
            }
            "AUDIO" => make_object_url(&bytes, "audio/ogg")?,
            _       => make_object_url(&bytes, "application/octet-stream")?,
        };

        self.urls.insert(path.to_string(), url.clone());
        Ok(url)
    }

    // ── string pool ───────────────────────────────────────────────────────────

    #[wasm_bindgen]
    pub fn key(&mut self, password: &str) -> Result<u32, JsValue> {
        let desc = self.inner.toc.strings.as_ref()
            .or_else(|| self.inner.toc.binmap.as_ref().and_then(|b| b.strings.as_ref()))
            .ok_or_else(|| JsValue::from_str("no string pool"))?;
        let start = HEADER_SIZE + desc.offset as usize;
        let end   = start + desc.size as usize;
        let data  = self.inner.data_slice(start, end).map_err(jerr)?;
        let pool  = crate::strings::decode_pool(data, password).map_err(jerr)?;
        let count = pool.len() as u32;
        self.string_pool = Some(pool);
        Ok(count)
    }

    #[wasm_bindgen]
    pub fn string(&self, index: u32) -> Option<String> {
        self.string_pool.as_ref()?.get(index as usize).cloned()
    }
}
