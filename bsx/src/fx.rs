//! Per-event runtime audio effects parsed from `.fx` sidecar files.
//!
//! ## File naming convention
//!
//!   `sfx/explosion.ogg`  → sidecar  `sfx/explosion.fx`
//!
//! For events with numbered variations (`explosion_01.ogg`, `explosion_02.ogg`)
//! the packer accepts either `explosion.fx` (whole event) or
//! `explosion_01.fx` (first variation wins).
//!
//! ## Файлы эффектов — конвенции именования
//!
//!   sfx/explosion.ogg        → sfx/explosion.fx       (специфичный для файла)
//!   sfx/explosion_01.ogg     → sfx/explosion.fx        (первая вариация определяет весь ивент)
//!   sfx/__dir__.fx            → все .ogg в папке sfx/, НЕ рекурсивно
//!   sfx/__master__.fx         → все .ogg в sfx/ и всех подпапках рекурсивно
//!
//! ## Порядок суммирования эффектов
//!
//!   Итоговые параметры = __master__.fx (рекурсивный) + __dir__.fx (директория) + файловый .fx
//!   При суммировании числовые диапазоны складываются (min+min, max+max).
//!   bitrate_kbps берётся из файлового .fx если есть, иначе из __dir__.fx, иначе из __master__.fx.
//!
//! ## Пример
//!
//!   audio/
//!     __master__.fx       → volume: -3::0          (всё аудио тише на 0..3 dB)
//!     music/
//!       __dir__.fx        → reverb: 0.1::0.3       (только музыка с реverbом)
//!       theme.ogg
//!       theme.fx          → pitch: -1::1            (итог: volume + reverb + pitch)
//!     sfx/
//!       explosion.ogg
//!       explosion.fx      → pitch: -2::2            (итог: volume + pitch)
//!
//! ## Syntax
//!
//! ```text
//! # comment
//! pitch:        -2::2       # semitones — pitch-only shift
//! speed:        0.9::1.1    # playback rate multiplier
//! spitch:       -2::2       # semitones — pitch + speed together
//! volume:       -6::0       # dB gain
//! pan:          -40::40     # -100 = full left · 100 = full right
//! lowpass:      3000::8000  # Hz
//! highpass:     80::300     # Hz
//! reverb:       0.0::0.25   # wet mix 0..1
//! start_offset: 0::150      # random trim ms
//! bitrate_kbps: 48          # pack-time override only
//! ```
//!
//! A value written as `a::b` is sampled uniformly from [a, b] each play.
//! A bare `a` is shorthand for `a::a` (constant).

use serde::{Deserialize, Serialize};

// ─── FxRange ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct FxRange { pub min: f32, pub max: f32 }

impl FxRange {
    pub fn exact(v: f32) -> Self           { FxRange { min: v, max: v } }
    pub fn range(a: f32, b: f32) -> Self   { FxRange { min: a, max: b } }

    pub fn sample(&self) -> f32 {
        if (self.max - self.min).abs() < 1e-9 { return self.min; }
        #[cfg(feature = "full")] {
            use rand::Rng;
            rand::rng().random_range(self.min..=self.max)
        }
        #[cfg(not(feature = "full"))]
        { self.min + (self.max - self.min) * 0.5 }
    }
}

impl std::fmt::Display for FxRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if (self.max - self.min).abs() < 1e-6 {
            write!(f, "{}", self.min)
        } else {
            write!(f, "{}::{}", self.min, self.max)
        }
    }
}

impl Default for FxRange {
    fn default() -> Self { FxRange::exact(0.0) }
}

// ─── FxParams ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FxParams {
    pub pitch:        Option<FxRange>,
    pub speed:        Option<FxRange>,
    pub spitch:       Option<FxRange>,
    pub volume_db:    Option<FxRange>,
    pub pan:          Option<FxRange>,
    pub lowpass_hz:   Option<FxRange>,
    pub highpass_hz:  Option<FxRange>,
    pub reverb_wet:   Option<FxRange>,
    pub start_offset: Option<FxRange>,
    /// Pack-time only — not used at runtime.
    pub bitrate_kbps: Option<u32>,
}

impl FxParams {
    pub fn is_runtime_empty(&self) -> bool {
        self.pitch.is_none()
            && self.speed.is_none()
            && self.spitch.is_none()
            && self.volume_db.is_none()
            && self.pan.is_none()
            && self.lowpass_hz.is_none()
            && self.highpass_hz.is_none()
            && self.reverb_wet.is_none()
            && self.start_offset.is_none()
    }

    /// Merge `self` (base) with `more_specific`.
    /// Numeric ranges are **added** (min+min, max+max).
    /// `bitrate_kbps` is taken from `more_specific` if set, otherwise falls back to `self`.
    pub fn merge(&self, more_specific: &FxParams) -> FxParams {
        FxParams {
            pitch:        add_range(self.pitch,        more_specific.pitch),
            speed:        add_range(self.speed,        more_specific.speed),
            spitch:       add_range(self.spitch,       more_specific.spitch),
            volume_db:    add_range(self.volume_db,    more_specific.volume_db),
            pan:          add_range(self.pan,          more_specific.pan),
            lowpass_hz:   add_range(self.lowpass_hz,   more_specific.lowpass_hz),
            highpass_hz:  add_range(self.highpass_hz,  more_specific.highpass_hz),
            reverb_wet:   add_range(self.reverb_wet,   more_specific.reverb_wet),
            start_offset: add_range(self.start_offset, more_specific.start_offset),
            bitrate_kbps: more_specific.bitrate_kbps.or(self.bitrate_kbps),
        }
    }

    /// Merge: `self` overrides `base` (self has higher priority, not additive).
    pub fn merge_over(&self, base: &FxParams) -> FxParams {
        FxParams {
            pitch:        self.pitch        .or(base.pitch),
            speed:        self.speed        .or(base.speed),
            spitch:       self.spitch       .or(base.spitch),
            volume_db:    self.volume_db    .or(base.volume_db),
            pan:          self.pan          .or(base.pan),
            lowpass_hz:   self.lowpass_hz   .or(base.lowpass_hz),
            highpass_hz:  self.highpass_hz  .or(base.highpass_hz),
            reverb_wet:   self.reverb_wet   .or(base.reverb_wet),
            start_offset: self.start_offset .or(base.start_offset),
            bitrate_kbps: self.bitrate_kbps .or(base.bitrate_kbps),
        }
    }
}

fn add_range(a: Option<FxRange>, b: Option<FxRange>) -> Option<FxRange> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b))    => Some(FxRange { min: a.min + b.min, max: a.max + b.max }),
    }
}

// ─── .fx file parser ─────────────────────────────────────────────────────────

/// Parse the text content of a `.fx` sidecar file into [`FxParams`].
/// Unknown keys and malformed lines are silently ignored.
pub fn parse_fx(content: &str) -> FxParams {
    let mut p = FxParams::default();
    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() { continue; }
        let Some((key, val)) = line.split_once(':') else { continue };
        let key = key.trim();
        let val = val.trim();
        match key {
            "pitch"        => p.pitch        = parse_range(val),
            "speed"        => p.speed        = parse_range(val),
            "spitch"       => p.spitch       = parse_range(val),
            "volume"       => p.volume_db    = parse_range(val),
            "pan"          => p.pan          = parse_range(val),
            "lowpass"      => p.lowpass_hz   = parse_range(val),
            "highpass"     => p.highpass_hz  = parse_range(val),
            "reverb"       => p.reverb_wet   = parse_range(val),
            "start_offset" => p.start_offset = parse_range(val),
            "bitrate_kbps" => p.bitrate_kbps = val.parse().ok(),
            _              => {}
        }
    }
    p
}

fn parse_range(s: &str) -> Option<FxRange> {
    if let Some((a, b)) = s.split_once("::") {
        let min = a.trim().parse::<f32>().ok()?;
        let max = b.trim().parse::<f32>().ok()?;
        Some(FxRange::range(min, max))
    } else {
        let v = s.trim().parse::<f32>().ok()?;
        Some(FxRange::exact(v))
    }
}

/// Load a `.fx` file from disk, returning default (all None) on error.
pub fn load_fx(path: &std::path::Path) -> FxParams {
    std::fs::read_to_string(path)
        .map(|s| parse_fx(&s))
        .unwrap_or_default()
}

// ─── Runtime DSP ──────────────────────────────────────────────────────────────

/// Apply FxParams to interleaved i16 PCM samples.
/// Returns (processed_samples, out_channels, sample_rate).
pub fn apply_fx_to_pcm(
    samples:     &[i16],
    channels:    u8,
    sample_rate: u32,
    fx:          &FxParams,
) -> (Vec<f32>, u8, u32) {
    if fx.is_runtime_empty() {
        let f: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
        return (f, channels, sample_rate);
    }

    let ch  = channels as usize;
    let sr  = sample_rate as f64;
    let mut out: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();

    if let Some(r) = fx.start_offset {
        let trim_ms  = r.sample().max(0.0) as f64;
        let trim_smp = ((trim_ms / 1000.0) * sr * ch as f64) as usize;
        let trim_smp = trim_smp - (trim_smp % ch);
        if trim_smp < out.len() { out = out[trim_smp..].to_vec(); }
    }

    let mut rate_mult = 1.0f64;
    if let Some(r) = fx.spitch {
        let st = r.sample() as f64;
        rate_mult *= 2.0f64.powf(st / 12.0);
    }
    if let Some(r) = fx.speed {
        rate_mult *= r.sample().max(0.1).min(4.0) as f64;
    }
    if (rate_mult - 1.0).abs() > 1e-4 {
        let in_frames  = out.len() / ch;
        let out_frames = (in_frames as f64 / rate_mult).round() as usize;
        let mut resampled = vec![0.0f32; out_frames * ch];
        for i in 0..out_frames {
            let src_pos = i as f64 * rate_mult;
            let lo      = src_pos.floor() as usize;
            let hi      = (lo + 1).min(in_frames.saturating_sub(1));
            let t       = (src_pos - src_pos.floor()) as f32;
            for c in 0..ch {
                let a = *out.get(lo * ch + c).unwrap_or(&0.0);
                let b = *out.get(hi * ch + c).unwrap_or(&0.0);
                resampled[i * ch + c] = a + (b - a) * t;
            }
        }
        out = resampled;
    }

    if let Some(r) = fx.volume_db {
        let db  = r.sample() as f64;
        let lin = 10.0f64.powf(db / 20.0) as f32;
        for s in &mut out { *s *= lin; }
    }

    if ch == 2 {
        if let Some(r) = fx.pan {
            let p      = (r.sample() as f64 / 100.0).clamp(-1.0, 1.0);
            let gain_l = ((1.0 - p) * 0.5).sqrt() as f32;
            let gain_r = ((1.0 + p) * 0.5).sqrt() as f32;
            let n = out.len() / 2;
            for i in 0..n {
                out[i * 2]     *= gain_l;
                out[i * 2 + 1] *= gain_r;
            }
        }
    }

    if let Some(r) = fx.lowpass_hz {
        let freq = r.sample().max(20.0).min(20_000.0) as f64;
        let (b0, b1, b2, a1, a2) = biquad_lowpass(freq, sr, 0.707);
        for c in 0..ch { biquad_apply(&mut out, c, ch, b0, b1, b2, a1, a2); }
    }

    if let Some(r) = fx.highpass_hz {
        let freq = r.sample().max(1.0).min(18_000.0) as f64;
        let (b0, b1, b2, a1, a2) = biquad_highpass(freq, sr, 0.707);
        for c in 0..ch { biquad_apply(&mut out, c, ch, b0, b1, b2, a1, a2); }
    }

    if let Some(r) = fx.reverb_wet {
        let wet = r.sample().clamp(0.0, 1.0);
        if wet > 0.001 { out = apply_reverb(&out, ch, sample_rate, wet); }
    }

    for s in &mut out { *s = s.clamp(-1.0, 1.0); }
    (out, channels, sample_rate)
}

fn biquad_lowpass(freq: f64, sr: f64, q: f64) -> (f32, f32, f32, f32, f32) {
    let w0    = 2.0 * std::f64::consts::PI * freq / sr;
    let alpha = w0.sin() / (2.0 * q);
    let cos_w = w0.cos();
    let b1    = 1.0 - cos_w;
    let b0    = b1 / 2.0;
    let b2    = b0;
    let a0    = 1.0 + alpha;
    let a1    = -2.0 * cos_w;
    let a2    = 1.0 - alpha;
    ((b0/a0) as f32, (b1/a0) as f32, (b2/a0) as f32, (a1/a0) as f32, (a2/a0) as f32)
}

fn biquad_highpass(freq: f64, sr: f64, q: f64) -> (f32, f32, f32, f32, f32) {
    let w0    = 2.0 * std::f64::consts::PI * freq / sr;
    let alpha = w0.sin() / (2.0 * q);
    let cos_w = w0.cos();
    let b0    = (1.0 + cos_w) / 2.0;
    let b1    = -(1.0 + cos_w);
    let b2    = b0;
    let a0    = 1.0 + alpha;
    let a1    = -2.0 * cos_w;
    let a2    = 1.0 - alpha;
    ((b0/a0) as f32, (b1/a0) as f32, (b2/a0) as f32, (a1/a0) as f32, (a2/a0) as f32)
}

fn biquad_apply(data: &mut [f32], c: usize, ch: usize,
                b0: f32, b1: f32, b2: f32, a1: f32, a2: f32) {
    let n = data.len() / ch;
    let (mut x1, mut x2, mut y1, mut y2) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for i in 0..n {
        let x0 = data[i * ch + c];
        let y0 = b0 * x0 + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
        data[i * ch + c] = y0;
        x2 = x1; x1 = x0;
        y2 = y1; y1 = y0;
    }
}

fn apply_reverb(input: &[f32], ch: usize, sr: u32, wet: f32) -> Vec<f32> {
    let dry     = 1.0 - wet * 0.5;
    let sr_f    = sr as f64;
    let comb_delays_ms = [29.7, 37.1, 41.1, 43.7];
    let comb_gain      = 0.84f32;
    let mut reverb = vec![0.0f32; input.len()];
    for delay_ms in comb_delays_ms {
        let delay_smp = ((delay_ms / 1000.0) * sr_f * ch as f64) as usize;
        let delay_smp = (delay_smp / ch) * ch;
        if delay_smp == 0 { continue; }
        let mut buf = vec![0.0f32; delay_smp];
        let mut pos = 0usize;
        for (i, &s) in input.iter().enumerate() {
            let delayed    = buf[pos];
            let new_sample = s + comb_gain * delayed;
            buf[pos]       = new_sample;
            reverb[i]     += delayed;
            pos = (pos + 1) % delay_smp;
        }
    }
    allpass_inplace(&mut reverb, ch, sr_f, 5.0,  0.7);
    allpass_inplace(&mut reverb, ch, sr_f, 12.7, 0.7);
    let peak = reverb.iter().copied().fold(0.0f32, f32::max).max(1e-6);
    let scale = 0.25 / peak;
    for (i, s) in input.iter().enumerate() {
        reverb[i] = s * dry + reverb[i] * scale * wet;
    }
    reverb
}

fn allpass_inplace(data: &mut [f32], ch: usize, sr: f64, delay_ms: f64, gain: f32) {
    let delay_smp = ((delay_ms / 1000.0) * sr * ch as f64) as usize;
    let delay_smp = (delay_smp / ch.max(1)) * ch.max(1);
    if delay_smp == 0 || delay_smp >= data.len() { return; }
    let mut buf = vec![0.0f32; delay_smp];
    let mut pos = 0usize;
    for s in data.iter_mut() {
        let delayed = buf[pos];
        let vn      = *s + (-gain) * delayed;
        let out     = gain * vn + delayed;
        buf[pos]    = vn;
        *s          = out;
        pos = (pos + 1) % delay_smp;
    }
}