//! Audio codec helpers for BSX.
//!
//! ## Codec strategy at pack time
//!
//!   .ogg Ogg Opus  → strip Ogg container   → RawOpus  (lossless)
//!   .ogg Ogg Vorbis → symphonia PCM         → encode Opus → RawOpus
//!   .wav / .mp3 / .flac / .aac / …
//!                   → symphonia PCM         → encode Opus → RawOpus
//!
//! ## RawOpus v2 wire format (stored payload)
//!
//! ```text
//! [0]      version     u8 = 2
//! [1]      channels    u8
//! [2..4]   pre_skip    u16 LE   — Opus encoder delay in samples
//! [4..8]   num_packets u32 LE
//! [8]      cbr_flag    u8       — 1 = one VarInt; 0 = N VarInts
//! [9..]    length_table         — unsigned LEB128
//! [...]    raw Opus frame data
//! ```

use anyhow::{anyhow, Context, Result};
use std::io::Cursor;
use std::path::Path;

// ─── AudioData ────────────────────────────────────────────────────────────────

pub struct AudioData {
    pub samples:     Vec<i16>,
    pub sample_rate: u32,
    pub channels:    u8,
}

// ─── Bitrate heuristic ───────────────────────────────────────────────────────

pub fn auto_bitrate_kbps(channels: u8, duration_secs: f64) -> u32 {
    let is_sfx   = duration_secs < 4.0;
    let is_music = duration_secs > 30.0;
    match (channels, is_sfx, is_music) {
        (1, true,  _    ) => 32,
        (1, false, false) => 56,
        (1, false, true ) => 64,
        (2, true,  _    ) => 48,
        (2, false, false) => 80,
        _                 => 96,
    }
}

// ─── Decode entry point ───────────────────────────────────────────────────────

#[cfg(feature = "full")]
pub fn decode_file(path: &Path) -> Result<AudioData> {
    let ext = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let bytes = std::fs::read(path)
        .with_context(|| format!("read {:?}", path))?;
    if ext == "ogg" || ext == "opus" {
        if let Ok(a) = decode_ogg_opus(&bytes) { return Ok(a); }
    }
    decode_symphonia_bytes(&bytes, &ext)
}

// ─── Ogg Opus strip (lossless) ────────────────────────────────────────────────

pub fn strip_ogg_to_raw_opus(data: &[u8]) -> Result<Vec<u8>> {
    use ogg::reading::PacketReader;
    let cursor  = Cursor::new(data);
    let mut rdr = PacketReader::new(cursor);
    let head    = rdr.read_packet_expected()
        .map_err(|e| anyhow!("ogg read OpusHead: {e}"))?;
    if head.data.len() < 19 || &head.data[0..8] != b"OpusHead" {
        return Err(anyhow!("not Ogg Opus"));
    }
    let channels = head.data[9];
    let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]);
    rdr.read_packet_expected()
        .map_err(|e| anyhow!("ogg read OpusTags: {e}"))?;
    let mut lengths:  Vec<u32> = Vec::new();
    let mut pkt_data: Vec<u8>  = Vec::new();
    loop {
        match rdr.read_packet() {
            Ok(None)      => break,
            Ok(Some(pkt)) => {
                if pkt.data.is_empty() { continue; }
                lengths.push(pkt.data.len() as u32);
                pkt_data.extend_from_slice(&pkt.data);
            }
            Err(_) => break,
        }
    }
    Ok(pack_raw_opus(pre_skip, channels, &lengths, &pkt_data))
}

// ─── RawOpus encode ──────────────────────────────────────────────────────────

#[cfg(feature = "full")]
pub fn encode_raw_opus(samples: &[i16], from_rate: u32, channels: u8,
                       bitrate_kbps: u32) -> Result<Vec<u8>> {
    const TARGET_RATE: u32  = 48_000;
    const FRAME_SIZE: usize = 960;   // 20 ms @ 48 kHz per channel
    const PRE_SKIP:   u16   = 312;

    let pcm = if from_rate != TARGET_RATE {
        resample_i16(samples, from_rate, TARGET_RATE, channels)
    } else {
        samples.to_vec()
    };

    let ch_enum = opus_channels(channels)?;
    let mut enc = opus::Encoder::new(TARGET_RATE, ch_enum, opus::Application::Audio)
        .map_err(|e| anyhow!("create Opus encoder: {e}"))?;
    enc.set_bitrate(opus::Bitrate::Bits((bitrate_kbps * 1_000) as i32))
        .map_err(|e| anyhow!("set bitrate: {e}"))?;
    enc.set_vbr(true)
        .map_err(|e| anyhow!("set vbr: {e}"))?;
    enc.set_vbr_constraint(false)
        .map_err(|e| anyhow!("set vbr_constraint: {e}"))?;
    enc.set_complexity(10)
        .map_err(|e| anyhow!("set complexity: {e}"))?;

    let frame_len = FRAME_SIZE * channels as usize;
    let mut padded = pcm;
    let rem = padded.len() % frame_len;
    if rem != 0 { padded.extend(std::iter::repeat(0i16).take(frame_len - rem)); }

    let mut enc_buf = vec![0u8; 4096];
    let mut lengths:  Vec<u32> = Vec::new();
    let mut pkt_data: Vec<u8>  = Vec::new();
    for chunk in padded.chunks(frame_len) {
        let n = enc.encode(chunk, &mut enc_buf)
            .map_err(|e| anyhow!("opus encode: {e}"))?;
        lengths.push(n as u32);
        pkt_data.extend_from_slice(&enc_buf[..n]);
    }
    Ok(pack_raw_opus(PRE_SKIP, channels, &lengths, &pkt_data))
}

// ─── RawOpus v2 wire format ───────────────────────────────────────────────────

fn write_varint(buf: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 { buf.push(byte); break; }
        buf.push(byte | 0x80);
    }
}

fn read_varint(data: &[u8], pos: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(*pos)?;
        *pos += 1;
        v |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 { return Some(v); }
        shift += 7;
        if shift >= 35 { return None; }
    }
}

fn pack_raw_opus(pre_skip: u16, channels: u8, lengths: &[u32], pkt_data: &[u8]) -> Vec<u8> {
    let num     = lengths.len() as u32;
    let is_cbr  = lengths.windows(2).all(|w| w[0] == w[1]);
    let one_len = lengths.first().copied().unwrap_or(0);

    let mut out = Vec::with_capacity(10 + lengths.len() * 2 + pkt_data.len());
    out.push(2u8); // version
    out.push(channels);
    out.extend_from_slice(&pre_skip.to_le_bytes());
    out.extend_from_slice(&num.to_le_bytes());
    out.push(if is_cbr { 1u8 } else { 0u8 });
    if is_cbr { write_varint(&mut out, one_len); }
    else       { for &l in lengths { write_varint(&mut out, l); } }
    out.extend_from_slice(pkt_data);
    out
}

fn parse_raw_opus_header(data: &[u8]) -> Result<(usize, u8, Vec<usize>, usize)> {
    if data.len() < 8 {
        return Err(anyhow!("raw opus: payload too short ({} B)", data.len()));
    }
    if data[0] == 2 {
        // v2: VarInt lengths
        let channels = data[1];
        let pre_skip = u16::from_le_bytes([data[2], data[3]]) as usize;
        let num_pkts = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        if data.len() < 9 { return Err(anyhow!("raw opus v2: truncated")); }
        let cbr_flag = data[8];
        let mut pos  = 9usize;
        let lengths: Vec<usize> = if cbr_flag == 1 {
            let l = read_varint(data, &mut pos)
                .ok_or_else(|| anyhow!("raw opus v2: cbr varint read failed"))? as usize;
            vec![l; num_pkts]
        } else {
            (0..num_pkts).map(|_| {
                read_varint(data, &mut pos)
                    .map(|v| v as usize)
                    .ok_or_else(|| anyhow!("raw opus v2: varint read failed"))
            }).collect::<Result<Vec<_>>>()?
        };
        Ok((pre_skip, channels, lengths, pos))
    } else {
        // v1: u32 lengths (legacy)
        let pre_skip = u16::from_le_bytes([data[0], data[1]]) as usize;
        let channels = data[2];
        let num_pkts = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        let hdr_end  = 8 + num_pkts * 4;
        if data.len() < hdr_end {
            return Err(anyhow!("raw opus v1: truncated lengths table"));
        }
        let lengths: Vec<usize> = (0..num_pkts)
            .map(|i| u32::from_le_bytes(data[8+i*4..8+i*4+4].try_into().unwrap()) as usize)
            .collect();
        Ok((pre_skip, channels, lengths, hdr_end))
    }
}

// ─── RawOpus decode ──────────────────────────────────────────────────────────

#[cfg(feature = "full")]
pub fn decode_raw_opus(data: &[u8]) -> Result<AudioData> {
    let (pre_skip, channels, lengths, mut pos) = parse_raw_opus_header(data)?;
    let ch_enum = opus_channels(channels)?;
    let mut dec = opus::Decoder::new(48_000, ch_enum)
        .map_err(|e| anyhow!("create Opus decoder: {e}"))?;
    let max_frame = 5760 * channels as usize;
    let mut pcm_buf = vec![0i16; max_frame];
    let mut all: Vec<i16> = Vec::new();
    let mut skipped = 0usize;
    for len in lengths {
        let end = pos + len;
        if end > data.len() { break; }
        let pkt = &data[pos..end];
        pos = end;
        let n = dec.decode(pkt, &mut pcm_buf, false)
            .map_err(|e| anyhow!("opus decode: {e}"))?;
        let decoded = &pcm_buf[..n * channels as usize];
        if skipped < pre_skip {
            let skip_f = (pre_skip - skipped).min(n);
            skipped   += skip_f;
            let skip_s = skip_f * channels as usize;
            if skip_s < decoded.len() { all.extend_from_slice(&decoded[skip_s..]); }
        } else {
            all.extend_from_slice(decoded);
        }
    }
    if all.is_empty() { return Err(anyhow!("RawOpus: decoded 0 samples")); }
    Ok(AudioData { samples: all, sample_rate: 48_000, channels })
}

// ─── RawOpus → Ogg Opus (lossless re-wrap for depack output) ─────────────────

pub fn raw_opus_to_ogg(data: &[u8]) -> Result<Vec<u8>> {
    use ogg::writing::{PacketWriteEndInfo, PacketWriter};
    let (pre_skip, channels, lengths, mut pos) = parse_raw_opus_header(data)?;
    let serial: u32 = 0x42_53_58_01; // "BSX\x01" as u32

    let mut head = Vec::<u8>::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); head.push(channels);
    head.extend_from_slice(&(pre_skip as u16).to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0); // channel mapping family 0

    let vendor = b"BSX/1.0";
    let mut tags = Vec::<u8>::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes());

    let mut out = Vec::new();
    {
        let cur = Cursor::new(&mut out);
        let mut pw = PacketWriter::new(cur);
        pw.write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| anyhow!("write OpusHead: {e}"))?;
        pw.write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| anyhow!("write OpusTags: {e}"))?;
        let last = lengths.len().saturating_sub(1);
        let mut granule: u64 = pre_skip as u64;
        for (i, &len) in lengths.iter().enumerate() {
            let end = pos + len;
            if end > data.len() { break; }
            granule += 960;
            let end_info = if i == last          { PacketWriteEndInfo::EndStream }
                           else if i % 50 == 49  { PacketWriteEndInfo::EndPage   }
                           else                   { PacketWriteEndInfo::NormalPacket };
            pw.write_packet(data[pos..end].to_vec(), serial, end_info, granule)
                .map_err(|e| anyhow!("write packet {i}: {e}"))?;
            pos = end;
        }
    }
    Ok(out)
}

// ─── Ogg Opus decode (legacy / passthrough) ───────────────────────────────────

#[cfg(feature = "full")]
pub fn decode_ogg_opus(data: &[u8]) -> Result<AudioData> {
    use ogg::reading::PacketReader;
    let cursor  = Cursor::new(data);
    let mut rdr = PacketReader::new(cursor);
    let head    = rdr.read_packet_expected()
        .map_err(|e| anyhow!("read OpusHead: {e}"))?;
    if head.data.len() < 19 || &head.data[0..8] != b"OpusHead" {
        return Err(anyhow!("not an Ogg Opus stream"));
    }
    let channels = head.data[9];
    let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]) as usize;
    rdr.read_packet_expected()
        .map_err(|e| anyhow!("read OpusTags: {e}"))?;
    let ch_enum = opus_channels(channels)?;
    let mut dec = opus::Decoder::new(48_000, ch_enum)
        .map_err(|e| anyhow!("create Opus decoder: {e}"))?;
    let max_frame = 5760 * channels as usize;
    let mut pcm_buf = vec![0i16; max_frame];
    let mut all: Vec<i16> = Vec::new();
    let mut skipped = 0usize;
    loop {
        match rdr.read_packet().map_err(|e| anyhow!("ogg read: {e}"))? {
            None      => break,
            Some(pkt) => {
                if pkt.data.is_empty() { continue; }
                let n = dec.decode(&pkt.data, &mut pcm_buf, false)
                    .map_err(|e| anyhow!("opus decode: {e}"))?;
                let decoded = &pcm_buf[..n * channels as usize];
                if skipped < pre_skip {
                    let sf = (pre_skip - skipped).min(n);
                    skipped += sf;
                    let ss = sf * channels as usize;
                    if ss < decoded.len() { all.extend_from_slice(&decoded[ss..]); }
                } else {
                    all.extend_from_slice(decoded);
                }
            }
        }
    }
    if all.is_empty() { return Err(anyhow!("Ogg Opus: decoded 0 samples")); }
    Ok(AudioData { samples: all, sample_rate: 48_000, channels })
}

// ─── Symphonia decode ────────────────────────────────────────────────────────

#[cfg(feature = "full")]
fn decode_symphonia_bytes(data: &[u8], ext: &str) -> Result<AudioData> {
    use symphonia::core::{
        audio::SampleBuffer,
        codecs::{DecoderOptions, CODEC_TYPE_NULL},
        formats::FormatOptions,
        io::MediaSourceStream,
        meta::MetadataOptions,
        probe::Hint,
    };
    let cursor = Cursor::new(data.to_vec());
    let mss    = MediaSourceStream::new(Box::new(cursor), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(ext);
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| anyhow!("symphonia probe: {e}"))?;
    let mut reader     = probed.format;
    let track          = reader.tracks().iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("no audio track found"))?;
    let codec_params = track.codec_params.clone();
    let track_id     = track.id;
    let sample_rate  = codec_params.sample_rate.unwrap_or(44_100);
    let channels     = codec_params.channels.map(|c| c.count() as u8).unwrap_or(2);
    let mut decoder  = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| anyhow!("make decoder: {e}"))?;
    let mut all: Vec<i16> = Vec::new();
    loop {
        let packet = match reader.next_packet() { Ok(p) => p, Err(_) => break };
        if packet.track_id() != track_id { continue; }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let mut buf = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
                buf.copy_interleaved_ref(decoded);
                all.extend_from_slice(buf.samples());
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(anyhow!("decode: {e}")),
        }
    }
    if all.is_empty() { return Err(anyhow!("decoded 0 samples")); }
    Ok(AudioData { samples: all, sample_rate, channels })
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

#[cfg(feature = "full")]
fn opus_channels(channels: u8) -> Result<opus::Channels> {
    match channels {
        1 => Ok(opus::Channels::Mono),
        2 => Ok(opus::Channels::Stereo),
        n => Err(anyhow!("Opus supports only 1–2 channels, got {n}")),
    }
}

pub fn resample_i16(input: &[i16], from_rate: u32, to_rate: u32, channels: u8) -> Vec<i16> {
    if from_rate == to_rate { return input.to_vec(); }
    let ch         = channels as usize;
    let in_frames  = input.len() / ch;
    let out_frames = ((in_frames as u64 * to_rate as u64 + from_rate as u64 - 1)
                      / from_rate as u64) as usize;
    let mut out = vec![0i16; out_frames * ch];
    for i in 0..out_frames {
        let pos = i as f64 * from_rate as f64 / to_rate as f64;
        let lo  = pos.floor() as usize;
        let hi  = (lo + 1).min(in_frames.saturating_sub(1));
        let t   = pos - pos.floor();
        for c in 0..ch {
            let a = input.get(lo*ch+c).copied().unwrap_or(0) as f64;
            let b = input.get(hi*ch+c).copied().unwrap_or(0) as f64;
            out[i*ch+c] = (a + (b-a)*t).round() as i16;
        }
    }
    out
}

// ─── PCM → WAV bytes ─────────────────────────────────────────────────────────

/// Encode f32 interleaved samples to a standard WAV (PCM 16-bit LE).
pub fn f32_to_wav(samples: &[f32], channels: u8, sample_rate: u32) -> Vec<u8> {
    let num_samples  = samples.len();
    let byte_count   = num_samples * 2; // i16 = 2 bytes
    let data_size    = byte_count as u32;
    let file_size    = 36 + data_size;
    let byte_rate    = sample_rate * channels as u32 * 2;
    let block_align  = channels as u16 * 2;

    let mut out = Vec::with_capacity(44 + byte_count);

    // RIFF header
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    // fmt chunk
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());         // chunk size
    out.extend_from_slice(&1u16.to_le_bytes());          // PCM
    out.extend_from_slice(&(channels as u16).to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());         // bits per sample
    // data chunk
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    // samples
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Full pipeline: RawOpus payload → decode → apply fx → WAV PCM bytes.
#[cfg(feature = "full")]
pub fn raw_opus_to_pcm_wav(
    data: &[u8],
    fx:   &crate::fx::FxParams,
) -> Result<Vec<u8>> {
    let audio = decode_raw_opus(data)?;
    let (processed, out_ch, out_sr) = crate::fx::apply_fx_to_pcm(
        &audio.samples, audio.channels, audio.sample_rate, fx,
    );
    Ok(f32_to_wav(&processed, out_ch, out_sr))
}

/// Lightweight version for environments without the full feature set.
/// Decodes using the Ogg rewrap path (fallback: return raw).
#[cfg(not(feature = "full"))]
pub fn raw_opus_to_pcm_wav(
    data: &[u8],
    _fx:  &crate::fx::FxParams,
) -> Result<Vec<u8>> {
    // Without Opus decoder, return raw bytes as-is
    Ok(data.to_vec())
}
