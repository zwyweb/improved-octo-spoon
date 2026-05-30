use crate::fx::FxParams;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const MAGIC: &[u8; 4] = b"BSX\x01";
pub const VERSION: u16     = 2;
pub const HEADER_SIZE: usize = 32;

// ─── Preset ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Preset {
    Fast,    // lz4_flex
    Mid,     // zstd level 8
    Slow,    // zstd level 22
    Default, // zstd level 16 (если --preset не передан)
}

impl Preset {
    pub fn zstd_level(self) -> i32 {
        match self {
            Preset::Fast    => 0,
            Preset::Mid     => 8,
            Preset::Slow    => 22,
            Preset::Default => 16,
        }
    }
    pub fn is_lz4(self) -> bool { matches!(self, Preset::Fast) }
}

// ─── ChannelMode ──────────────────────────────────────────────────────────────

/// Controls how many colour channels are stored for raster textures.
///
/// | Mode      | Opaque texture  | Transparent texture           |
/// |-----------|-----------------|-------------------------------|
/// | Auto      | RGB  (3 bpp)    | RGBA (4 bpp)                  |
/// | ForceRgba | RGBA (4 bpp)    | RGBA (4 bpp)                  |
/// | ForceRgb  | RGB  (3 bpp)    | RGB  (3 bpp, alpha discarded) |
///
/// RGB saves ~20–25 % on QOI payload and compresses better downstream
/// because there is no constant 0xFF run in the alpha channel.
///
/// Decoded textures are always returned as RGBA8 regardless of storage
/// (decoder expands RGB → RGBA with alpha = 255 on the fly, zero extra
/// allocation: the expansion happens in a single pre-allocated pass).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ChannelMode {
    /// Per-image detection: RGB if source has no alpha channel, RGBA if it does.
    /// Mixed projects (some opaque, some transparent) work correctly in this mode.
    #[default]
    Auto,
    /// Always store 4 channels. Use when your renderer unconditionally
    /// expects RGBA and you want to skip the alpha-detection overhead.
    ForceRgba,
    /// Always store 3 channels. Alpha is discarded even when present.
    /// Safe for fully-opaque asset sets (backgrounds, tilesets, UI without
    /// transparency). Gives the best compression ratio.
    ForceRgb,
}

// ─── Asset types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AssetKind {
    TextureRaw { width: u32, height: u32 },
    TextureGpu { width: u32, height: u32 },
    Audio { codec: String, sample_rate: u32, channels: u8, duration_ms: u32 },
    /// GLB/GLTF mesh — vertex-cache-optimized and optionally simplified.
    ///
    /// Storage: optimized GLB bytes, compressed with BSX preset.
    /// On depack: extracted as-is (.glb), readable by any GLTF loader.
    ///
    /// `quantized = true` when --opt was used (meshopt simplify applied).
    /// The output is still a valid GLB; accessor.count is updated in JSON.
    Mesh { vertex_count: u32, triangle_count: u32, quantized: bool },
    Blob,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Packing { Raw, Lz4, Zstd }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetEntry {
    pub name:        String,
    pub kind:        AssetKind,
    pub offset:      u64,
    pub packed_size: u64,
    pub orig_size:   u64,
    pub packing:     Packing,
    pub crc32:       u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTrackEntry {
    pub path:          String,
    pub offset:        u64,
    pub size:          u64,
    pub channels:      u8,
    pub sample_rate:   u32,
    pub total_samples: u64,
    pub bitrate_kbps:  u32,
    #[serde(default)]
    pub fx:            FxParams,
}

impl AudioTrackEntry {
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 || self.channels == 0 { return 0.0; }
        self.total_samples as f64 / (self.sample_rate as f64 * self.channels as f64)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StringPoolDesc {
    pub offset: u64,
    pub size:   u64,
    pub count:  u32,
}

pub type BmapIndex = HashMap<String, HashMap<String, String>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinmapAssetEntry {
    pub name:      String,
    pub kind:      AssetKind,
    pub offset:    u64,
    pub size:      u64,
    pub orig_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinmapTrackEntry {
    pub path:          String,
    pub offset:        u64,
    pub size:          u64,
    pub channels:      u8,
    pub sample_rate:   u32,
    pub total_samples: u64,
    pub bitrate_kbps:  u32,
    #[serde(default)]
    pub fx:            FxParams,
}

impl BinmapTrackEntry {
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 || self.channels == 0 { return 0.0; }
        self.total_samples as f64 / (self.sample_rate as f64 * self.channels as f64)
    }
}

/// One async-load group — all entries load in parallel; groups are sequential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadGroup {
    /// Asset paths or directories ending with '/'.
    pub entries: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BinmapToc {
    pub assets:     Vec<BinmapAssetEntry>,
    pub tracks:     Vec<BinmapTrackEntry>,
    pub blob_start: u64,
    pub blob_size:  u64,
    pub strings:    Option<StringPoolDesc>,
    /// zstd dictionary trained on texture (QOI) payloads at pack time (--binmap only).
    #[serde(default)]
    pub dict:       Option<Vec<u8>>,
    /// zstd dictionary trained on audio (RawOpus) payloads at pack time (--binmap only).
    #[serde(default)]
    pub dict_audio: Option<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct BsxToc {
    pub assets:  Vec<AssetEntry>,
    pub tracks:  Vec<AudioTrackEntry>,
    pub strings: Option<StringPoolDesc>,
    pub bmap:    BmapIndex,
    #[serde(default)]
    pub binmap:  Option<BinmapToc>,
    /// Async load order from index.bmap. Empty if file absent.
    #[serde(default)]
    pub index:   Vec<LoadGroup>,
}

pub struct PackResult {
    pub assets:       u64,
    pub textures:     u64,
    pub tracks:       u64,
    pub strings:      u32,
    pub orig_bytes:   u64,
    pub packed_bytes: u64,
    pub elapsed_ms:   u64,
}