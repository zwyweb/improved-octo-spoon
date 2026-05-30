//! Audio track packing — flat list, no bank hierarchy.
//!
//! All audio files under the root directory (any depth) are collected,
//! encoded to RawOpus, and stored as a flat list of tracks.
//!
//! ## FxParams resolution order per track
//!
//!   1. All `__master__.fx` from root down to the track's parent directory (additive)
//!   2. `__dir__.fx` in the track's parent directory (additive)
//!   3. Track-specific `.fx` sidecar (additive)
//!
//! Numeric ranges are summed (min+min, max+max) via `FxParams::merge`.
//! `bitrate_kbps` is taken from the most-specific source that defines it.

#![allow(dead_code)]

use crate::{
    codec,
    format::AudioTrackEntry,
    fx::{load_fx, FxParams},
};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::{
    path::{Path, PathBuf},
    sync::{atomic::{AtomicUsize, Ordering}, Arc},
};
use walkdir::WalkDir;

pub const EXT_AUDIO: &[&str] = &[
    "wav","mp3","ogg","opus","flac","aac","m4a","aiff","aif",
];

// ─── Result type ─────────────────────────────────────────────────────────────

pub struct TrackBlobs {
    pub tracks:    Vec<AudioTrackEntry>,
    /// Concatenated RawOpus payloads in track order.
    pub blob_data: Vec<u8>,
}

// ─── FxParams chain builder ──────────────────────────────────────────────────

/// Build merged FxParams for a track at `abs_path` under `root`.
///
/// Chain:
///   root/__master__.fx → root/a/__master__.fx → root/a/b/__master__.fx
///   → root/a/b/__dir__.fx → root/a/b/track.fx
fn resolve_track_fx(root: &Path, abs_path: &Path) -> FxParams {
    let rel = abs_path.strip_prefix(root).unwrap_or(abs_path);
    let parent = rel.parent().unwrap_or(Path::new(""));

    let mut merged = FxParams::default();

    // Walk from root → parent, collecting __master__.fx additively.
    let mut cur = root.to_path_buf();
    let master_name = "__master__.fx";
    let dir_name    = "__dir__.fx";

    // Root-level master
    let root_master = cur.join(master_name);
    if root_master.exists() {
        merged = merged.merge(&load_fx(&root_master));
    }

    // Each component of parent (root/a, root/a/b, …)
    for component in parent.components() {
        cur.push(component);
        let m = cur.join(master_name);
        if m.exists() { merged = merged.merge(&load_fx(&m)); }
    }

    // __dir__.fx in immediate parent
    let dir_fx = abs_path.parent().map(|d| d.join(dir_name));
    if let Some(p) = dir_fx.filter(|p| p.exists()) {
        merged = merged.merge(&load_fx(&p));
    }

    // Track-specific .fx
    let file_fx = abs_path.with_extension("fx");
    if file_fx.exists() {
        merged = merged.merge(&load_fx(&file_fx));
    }

    merged
}

// ─── Main packer ─────────────────────────────────────────────────────────────

pub fn pack_audio_tracks(
    root: &Path,
    data_offset: u64,
    progress_cb: &dyn Fn(&str),
) -> Result<TrackBlobs> {
    let mut audio_files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| {
            let ext = p.extension()
                .and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            EXT_AUDIO.contains(&ext.as_str())
        })
        .collect();
    audio_files.sort();

    if audio_files.is_empty() {
        return Ok(TrackBlobs { tracks: vec![], blob_data: vec![] });
    }

    let counter = Arc::new(AtomicUsize::new(0));

    struct Encoded {
        idx:           usize,
        track_path:    String,
        payload:       Vec<u8>,
        channels:      u8,
        sample_rate:   u32,
        total_samples: u64,
        bitrate_kbps:  u32,
        fx:            FxParams,
    }

    let mut results: Vec<Encoded> = audio_files
        .par_iter()
        .enumerate()
        .map(|(idx, path)| {
            let track_path = path.strip_prefix(root)
                .unwrap_or(path.as_path())
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");

            let ext = path.extension()
                .and_then(|e| e.to_str()).unwrap_or("").to_lowercase();

            // Resolve merged FxParams (master + dir + specific)
            let fx = resolve_track_fx(root, path);
            let fx_stored = fx.clone();

            let payload: Vec<u8> = (|| -> Result<Vec<u8>> {
                let bytes = std::fs::read(path)
                    .with_context(|| format!("read {:?}", path))?;

                if ext == "ogg" || ext == "opus" {
                    if let Ok(raw) = codec::strip_ogg_to_raw_opus(&bytes) {
                        return Ok(raw);
                    }
                }

                let audio = codec::decode_file(path)
                    .with_context(|| format!("decode {:?}", path))?;
                let duration = audio.samples.len() as f64
                    / (audio.sample_rate as f64 * audio.channels as f64);
                let kbps = fx.bitrate_kbps
                    .unwrap_or_else(|| codec::auto_bitrate_kbps(audio.channels, duration));
                codec::encode_raw_opus(&audio.samples, audio.sample_rate, audio.channels, kbps)
                    .with_context(|| format!("encode {:?}", path))
            })().unwrap_or_else(|e| {
                eprintln!("  warn: skip {:?}: {e}", path);
                vec![]
            });

            let (channels, sample_rate, total_samples, bitrate_kbps) =
                if payload.is_empty() { (0, 0, 0, 0) }
                else { probe_raw_opus(&payload) };

            counter.fetch_add(1, Ordering::Relaxed);

            Encoded { idx, track_path, payload, channels, sample_rate, total_samples, bitrate_kbps, fx: fx_stored }
        })
        .collect();

    results.sort_by_key(|e| e.idx);

    let mut blob_data: Vec<u8>          = Vec::new();
    let mut tracks:    Vec<AudioTrackEntry> = Vec::new();

    let mut cursor = data_offset;
    for enc in results {
        if enc.payload.is_empty() { continue; }
        progress_cb(&enc.track_path);
        let size = enc.payload.len() as u64;
        tracks.push(AudioTrackEntry {
            path:          enc.track_path,
            offset:        cursor,
            size,
            channels:      enc.channels,
            sample_rate:   enc.sample_rate,
            total_samples: enc.total_samples,
            bitrate_kbps:  enc.bitrate_kbps,
            fx:            enc.fx,
        });
        blob_data.extend_from_slice(&enc.payload);
        cursor += size;
    }

    Ok(TrackBlobs { tracks, blob_data })
}

// ─── RawOpus quick probe ─────────────────────────────────────────────────────

fn probe_raw_opus(data: &[u8]) -> (u8, u32, u64, u32) {
    if data.len() < 9 { return (0, 0, 0, 0); }
    if data[0] != 2   { return (0, 0, 0, 0); }
    let channels  = data[1];
    let num_pkts  = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as u64;
    let sr: u32   = 48_000;
    let total     = num_pkts * 960 * channels as u64;
    let dur_s     = total as f64 / (sr as f64 * channels as f64);
    let kbps = if dur_s > 0.01 {
        (data.len() as f64 * 8.0 / (dur_s * 1000.0)) as u32
    } else { 0 };
    (channels, sr, total, kbps)
}