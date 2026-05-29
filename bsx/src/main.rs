//! BSX CLI — Game Asset Bundle tool

use bsx::{depack_core, info_core, pack_core, human, BinmapMode};
use clap::{Parser, Subcommand};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::{path::PathBuf, sync::{Arc, Mutex}, time::Duration};
use bsx::format::{ChannelMode, Preset};

// ─── CLI definition ───────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "bsx",
    about = "BSX — Game Asset Bundle\nPacks directories of game assets into a single optimised .bsx file.",
    version = "2.2.0",
    after_help = "\
EXAMPLES:
    bsx pack assets/                        Pack with default settings (zstd-16)
    bsx pack assets/ -o game.bsx            Specify output path
    bsx pack assets/ --preset fast          Fast pack with lz4
    bsx pack assets/ --preset slow          Maximum compression (zstd-22)
    bsx pack assets/ --binmap               Single blob + zstd dictionary training
    bsx pack assets/ --force-rgb            Store all textures as RGB (no alpha)
    bsx pack assets/ --force-rgba           Store all textures as RGBA
    bsx depack game.bsx                     Extract all assets to game_out/
    bsx depack game.bsx -o extracted/       Extract to custom directory
    bsx info game.bsx                       Show bundle statistics
    bsx list game.bsx                       List all asset paths
    bsx list game.bsx --audio              List audio tracks only
    bsx list game.bsx --json               Machine-readable JSON output
    bsx extract game.bsx sprites/hero.png   Extract single asset
    bsx strings game.bsx -k mykey           Dump decrypted string pool"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pack a directory or single file into a .bsx bundle.
    ///
    /// Scans the source directory recursively and packs all assets:
    ///   - Images (png/jpg/webp/…) → QOI-encoded, then compressed
    ///   - Audio  (wav/mp3/ogg/…)  → Opus-encoded, FX sidecars applied
    ///   - Text   (json/glsl/lua/…) → zstd compressed
    ///   - Everything else         → compressed blob
    Pack {
        /// Source directory (or single file) to pack.
        path: PathBuf,

        /// Output .bsx file path. Default: <dirname>.bsx next to source.
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,

        /// Compression preset.
        ///   fast  = lz4       (fastest pack/unpack, ~2× larger than slow)
        ///   mid   = zstd-8    (good balance, ~2× faster than slow)
        ///   slow  = zstd-22   (maximum compression, slow pack)
        ///   Default (no flag) = zstd-16
        #[arg(long, value_name = "fast|mid|slow", value_parser = parse_preset)]
        preset: Option<Preset>,

        /// Path to a .pool file to embed an encrypted string pool.
        #[arg(long, value_name = "FILE")]
        pool: Option<PathBuf>,

        /// Encryption key for the string pool.
        #[arg(long, default_value = "", value_name = "KEY")]
        key: String,

        /// Pack all assets into a single flat blob and train a zstd dictionary
        /// on the full payload set for extra compression (~5-15% gain on
        /// homogeneous asset sets like tilesets or sprite sheets).
        #[arg(long)]
        binmap: bool,

        /// Store all textures as RGBA (4 bytes/pixel), disabling automatic
        /// alpha detection. Use when your renderer unconditionally expects RGBA.
        /// Mutually exclusive with --force-rgb.
        #[arg(long, conflicts_with = "force_rgb")]
        force_rgba: bool,

        /// Store all textures as RGB (3 bytes/pixel), discarding the alpha
        /// channel even when present. Saves ~20–25 % on texture payload size.
        /// Safe for fully-opaque asset sets (backgrounds, tilesets, UI without
        /// transparency). On decode, textures are expanded back to RGBA8
        /// (alpha = 255) transparently. Mutually exclusive with --force-rgba.
        #[arg(long, conflicts_with = "force_rgba")]
        force_rgb: bool,

        /// Enable lossy mesh optimisation for GLB/GLTF assets.
        ///
        /// Applies meshopt::simplify (~50% triangle reduction, ≤1% geometric error).
        /// Output is still a valid GLB; GLTF accessor.count is updated in JSON.
        /// Without this flag only lossless vertex-cache reorder runs.
        #[arg(long)]
        opt: bool,

        /// Embed a copyright notice in the bundle footer.
        #[arg(long, value_name = "TEXT")]
        legal: Option<String>,
    },

    /// Extract all assets from a .bsx bundle to disk.
    ///
    /// Textures are written as .png, audio as .ogg, everything else as-is.
    /// Default output directory: <bundlename>_out/ next to the .bsx file.
    Depack {
        /// Bundle file to extract.
        path: PathBuf,

        /// Output directory. Default: <bundlename>_out/
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,
    },

    /// Show bundle metadata and asset table.
    Info {
        /// Bundle file to inspect.
        path: PathBuf,
    },

    /// List asset paths in a bundle.
    ///
    /// Useful for scripting: pipe into grep, xargs, fzf, etc.
    List {
        /// Bundle file to list.
        path: PathBuf,

        /// Show only audio tracks.
        #[arg(long)]
        audio: bool,

        /// Output as a JSON array (machine-readable).
        #[arg(long)]
        json: bool,
    },

    /// Extract a single asset from a bundle.
    ///
    /// The asset is written decoded: textures as raw RGBA8, audio as .ogg Opus.
    Extract {
        /// Bundle file.
        path: PathBuf,

        /// Asset path inside the bundle (as shown by `bsx list`).
        asset: String,

        /// Output file path. Default: filename component of the asset path.
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },

    /// Dump the decrypted string pool from a bundle.
    Strings {
        /// Bundle file.
        path: PathBuf,

        /// Decryption key (must match the key used at pack time).
        #[arg(short, long, value_name = "KEY")]
        key: String,
    },
}

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Pack { path, output, preset, pool, key, binmap, force_rgba, force_rgb, opt, legal } => {
            if pool.is_some() && key.is_empty() {
                eprintln!("error: --pool requires --key (string pool must be encrypted)");
                std::process::exit(1);
            }
            let mode = if binmap { BinmapMode::Flat } else { BinmapMode::None };
            let p    = preset.unwrap_or(Preset::Default);
            let ch   = if force_rgba      { ChannelMode::ForceRgba }
                       else if force_rgb  { ChannelMode::ForceRgb  }
                       else               { ChannelMode::Auto       };
            cmd_pack(&path, output, pool.as_deref(), &key, p, legal.as_deref(), mode, ch, opt)
        }
        Cmd::Depack  { path, output }       => cmd_depack(&path, output.as_deref()),
        Cmd::Info    { path }               => cmd_info(&path),
        Cmd::List    { path, audio, json }  => cmd_list(&path, audio, json),
        Cmd::Extract { path, asset, output } => cmd_extract(&path, &asset, output.as_deref()),
        Cmd::Strings { path, key }          => cmd_strings(&path, &key),
    };
    if let Err(e) = result {
        eprintln!("{} {e}", "error:".red().bold());
        std::process::exit(1);
    }
}

// ─── pack ─────────────────────────────────────────────────────────────────────

fn cmd_pack(
    path:     &std::path::Path,
    output:   Option<PathBuf>,
    pool:     Option<&std::path::Path>,
    key:      &str,
    preset:   Preset,
    legal:    Option<&str>,
    mode:     BinmapMode,
    ch:       ChannelMode,
    mesh_opt: bool,
) -> anyhow::Result<()> {
    banner("BSX Pack");

    // Single-file input: copy into a temp dir so pack_core sees a directory.
    let (dir, tmp_dir, stem_hint): (PathBuf, Option<PathBuf>, Option<String>) =
        if path.is_file() {
            let tmp = std::env::temp_dir().join(format!("bsx_{}", std::process::id()));
            std::fs::create_dir_all(&tmp)?;
            let fname = path.file_name().ok_or_else(|| anyhow::anyhow!("no filename"))?;
            std::fs::copy(path, tmp.join(fname))?;
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            (tmp.clone(), Some(tmp), Some(stem))
        } else {
            (path.to_path_buf(), None, None)
        };

    let out = match output {
        Some(p) => p,
        None => {
            let stem = stem_hint.unwrap_or_else(|| {
                dir.file_name().unwrap_or(std::ffi::OsStr::new("bundle"))
                    .to_string_lossy().into_owned()
            });
            PathBuf::from(format!("{stem}.bsx"))
        }
    };

    let preset_label = match preset {
        Preset::Fast    => "fast  (lz4)",
        Preset::Mid     => "mid   (zstd-8)",
        Preset::Slow    => "slow  (zstd-22)",
        Preset::Default => "default (zstd-16)",
    };
    let mode_label = match mode {
        BinmapMode::None => "standard",
        BinmapMode::Flat => "flat blob + dict",
    };
    let ch_label = match ch {
        ChannelMode::Auto      => "auto (rgb if opaque, rgba if transparent)",
        ChannelMode::ForceRgba => "rgba (forced)",
        ChannelMode::ForceRgb  => "rgb  (forced, alpha discarded)",
    };

    println!("  Source   : {}", dir.display());
    println!("  Output   : {}", out.display());
    println!("  Preset   : {preset_label}");
    println!("  Mode     : {mode_label}");
    println!("  Textures : {ch_label}");
    println!("  Meshes   : {}", if mesh_opt { "lossy (--opt: simplify 50%)" } else { "lossless (vertex cache reorder)" });
    if let Some(p) = pool  { println!("  Pool     : {}", p.display()); }
    if let Some(n) = legal { println!("  Notice   : {n}"); }
    println!();

    // Progress bar — length will be set from first callback.
    let pb  = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template("  [{bar:40.cyan/blue}] {pos}/{len}  {msg}")
            .unwrap()
            .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    pb.enable_steady_tick(Duration::from_millis(100));
    let pba = Arc::new(Mutex::new(pb));

    let cb: Arc<dyn Fn(usize, usize, &str) + Send + Sync> = {
        let pba = pba.clone();
        Arc::new(move |n, total, name| {
            if let Ok(pb) = pba.lock() {
                pb.set_length(total as u64);
                pb.set_position(n as u64);
                // Truncate long names so the bar stays on one line.
                let msg = if name.len() > 48 { &name[name.len()-48..] } else { name };
                pb.set_message(msg.to_string());
            }
        })
    };

    let res = pack_core(&dir, &out, preset, pool, key, legal, mode, ch, mesh_opt, Some(cb))?;
    pba.lock().unwrap().finish_and_clear();
    if let Some(tmp) = tmp_dir { let _ = std::fs::remove_dir_all(tmp); }

    let ratio = compression_ratio(res.orig_bytes, res.packed_bytes);
    println!("  {} {}", "✓".green().bold(), out.display());
    println!("  Assets       : {}", res.assets);
    println!("  Textures     : {}", res.textures);
    println!("  Audio tracks : {}", res.tracks);
    if res.strings > 0 { println!("  Strings      : {}", res.strings); }
    println!("  Original     : {}", human(res.orig_bytes));
    println!("  Packed       : {}  {}", human(res.packed_bytes), ratio);
    println!("  Time         : {:.2}s", res.elapsed_ms as f64 / 1000.0);
    Ok(())
}

// ─── depack ───────────────────────────────────────────────────────────────────

fn cmd_depack(bsx_path: &std::path::Path, out_opt: Option<&std::path::Path>) -> anyhow::Result<()> {
    banner("BSX Depack");
    let out_dir = match out_opt {
        Some(d) => d.to_path_buf(),
        None    => {
            let stem = bsx_path.file_stem().unwrap_or_default().to_string_lossy();
            bsx_path.parent().unwrap_or(std::path::Path::new("."))
                .join(format!("{stem}_out"))
        }
    };
    println!("  Source : {}", bsx_path.display());
    println!("  Output : {}", out_dir.display());
    println!();
    let pb = spinner();
    pb.set_message("depacking…");
    let count = depack_core(bsx_path, &out_dir, false)?;
    pb.finish_and_clear();
    println!("  {} {} entries extracted", "✓".green().bold(), count);
    println!("  → {}", out_dir.display());
    Ok(())
}

// ─── info ─────────────────────────────────────────────────────────────────────

fn cmd_info(bsx_path: &std::path::Path) -> anyhow::Result<()> {
    banner("BSX Info");
    let info = info_core(bsx_path)?;
    let toc  = &info.toc;

    let mode = if let Some(bm) = &toc.binmap {
        if bm.dict.is_some() { "flat blob + dict".to_string() }
        else                 { "flat blob".to_string() }
    } else {
        "standard".to_string()
    };

    println!("  File    : {}", bsx_path.display());
    println!("  Size    : {}  (index: {} raw / {} compressed)",
        human(info.file_size), human(info.toc_raw_sz as u64), human(info.toc_comp_sz as u64));
    println!("  Mode    : {mode}");

    let (n_assets, n_tracks) = if let Some(bm) = &toc.binmap {
        (bm.assets.len(), bm.tracks.len())
    } else {
        (toc.assets.len(), toc.tracks.len())
    };
    println!("  Assets  : {n_assets}");
    println!("  Tracks  : {n_tracks}");

    if let Some(s) = toc.strings.as_ref().or(toc.binmap.as_ref().and_then(|b| b.strings.as_ref())) {
        println!("  Strings : {} ({} B)", s.count, s.size);
    }
    if !toc.index.is_empty()    { println!("  Load order : {} groups (index.bmap)", toc.index.len()); }
    if !toc.bmap.is_empty() {
        println!("  Aliases : {}", toc.bmap.keys().cloned().collect::<Vec<_>>().join(", "));
    }
    println!();

    // Asset table (standard mode only — binmap has no per-asset packing info).
    if !toc.assets.is_empty() {
        let w = (48usize, 14usize, 6usize, 9usize, 9usize);
        println!("  {}", "─".repeat(w.0+w.1+w.2+w.3+w.4+8).cyan());
        println!("  {:<w0$} {:<w1$} {:<w2$} {:>w3$} {:>w4$}",
            "Name", "Type", "Pack", "Packed", "Orig",
            w0=w.0, w1=w.1, w2=w.2, w3=w.3, w4=w.4);
        println!("  {}", "─".repeat(w.0+w.1+w.2+w.3+w.4+8));
        for e in &toc.assets {
            println!("  {:<w0$} {:<w1$} {:<w2$} {:>w3$} {:>w4$}",
                bsx::util::trunc(&e.name, w.0),
                bsx::asset::kind_to_str(&e.kind),
                bsx::asset::packing_to_str(&e.packing),
                human(e.packed_size), human(e.orig_size),
                w0=w.0, w1=w.1, w2=w.2, w3=w.3, w4=w.4);
        }
        let total: u64 = toc.assets.iter().map(|e| e.orig_size).sum();
        println!("  Total: {}", human(total));
        println!();
    }

    // Binmap asset table.
    if let Some(bm) = &toc.binmap {
        println!("  {}", "─".repeat(64).cyan());
        println!("  {:<50} {:>12}", "Name", "Size");
        println!("  {}", "─".repeat(64));
        for e in &bm.assets {
            println!("  {:<50} {:>12}", bsx::util::trunc(&e.name, 50), human(e.size));
        }
        for t in &bm.tracks {
            println!("  {:<50} {:>12}  [audio]", bsx::util::trunc(&t.path, 50), human(t.size));
        }
        println!();
    }
    Ok(())
}

// ─── list ─────────────────────────────────────────────────────────────────────

fn cmd_list(bsx_path: &std::path::Path, audio: bool, json: bool) -> anyhow::Result<()> {
    let data   = std::fs::read(bsx_path)?;
    let bundle = bsx::BsxBundle::from_bytes(data)?;
    let items: Vec<String> = if audio { bundle.tracks() } else { bundle.list_all() };
    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        for item in &items { println!("{item}"); }
    }
    Ok(())
}

// ─── extract ─────────────────────────────────────────────────────────────────

fn cmd_extract(
    bsx_path:  &std::path::Path,
    asset:     &str,
    out_opt:   Option<&std::path::Path>,
) -> anyhow::Result<()> {
    banner("BSX Extract");
    let data   = std::fs::read(bsx_path)?;
    let bundle = bsx::BsxBundle::from_bytes(data)?;
    let bytes  = bundle.get(asset)?;
    let dest   = match out_opt {
        Some(p) => p.to_path_buf(),
        None    => {
            // Use the filename component of the asset path, adjust extension.
            let base = std::path::Path::new(asset);
            let kind = bundle.asset_type(asset);
            let fname = base.file_name().unwrap_or_default().to_string_lossy();
            match kind {
                "IMG"   => PathBuf::from(format!("{}.png",
                    base.file_stem().unwrap_or_default().to_string_lossy())),
                "AUDIO" => PathBuf::from(format!("{}.ogg",
                    base.file_stem().unwrap_or_default().to_string_lossy())),
                _       => PathBuf::from(fname.as_ref()),
            }
        }
    };
    if let Some(p) = dest.parent() {
        if !p.as_os_str().is_empty() { std::fs::create_dir_all(p)?; }
    }
    // For textures bsx.get() returns raw RGBA8; encode to PNG for usability.
    let out_bytes = if bundle.asset_type(asset) == "IMG" {
        if let Some((w, h)) = bundle.dimensions(asset) {
            use std::io::Cursor;
            let img = image::RgbaImage::from_raw(w, h, bytes)
                .ok_or_else(|| anyhow::anyhow!("invalid RGBA8 dimensions"))?;
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)?;
            buf
        } else { bytes }
    } else { bytes };

    std::fs::write(&dest, &out_bytes)?;
    println!("  {} {} → {}", "✓".green().bold(), asset, dest.display());
    println!("  {} B written", out_bytes.len());
    Ok(())
}

// ─── strings ──────────────────────────────────────────────────────────────────

fn cmd_strings(bsx_path: &std::path::Path, key: &str) -> anyhow::Result<()> {
    banner("BSX Strings");
    let buf  = std::fs::read(bsx_path)?;
    let toc  = bsx::load_toc(&buf)?;
    let desc = toc.strings.as_ref()
        .or(toc.binmap.as_ref().and_then(|b| b.strings.as_ref()))
        .ok_or_else(|| anyhow::anyhow!("bundle has no string pool"))?;
    let s = bsx::format::HEADER_SIZE + desc.offset as usize;
    let e = s + desc.size as usize;
    if e > buf.len() { return Err(anyhow::anyhow!("string pool OOB")); }
    let strings = bsx::strings::decode_pool(&buf[s..e], key)?;
    println!("  {} strings\n", strings.len());
    for (i, s) in strings.iter().enumerate() {
        println!("  [{i:>4}]  {}", s.replace('\n', "↵"));
    }
    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn banner(t: &str) { println!("{}", format!("── {t} ──").bold().cyan()); }

fn spinner() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("  {spinner:.cyan} {msg}").unwrap()
            .tick_strings(&["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

fn compression_ratio(orig: u64, packed: u64) -> String {
    if orig == 0 { return String::new(); }
    let r = (1.0 - packed as f64 / orig as f64) * 100.0;
    if r >= 0.0 { format!("({:.1}% smaller)", r) }
    else        { format!("({:.1}% larger)", r.abs()) }
}

fn parse_preset(s: &str) -> Result<Preset, String> {
    match s {
        "fast" => Ok(Preset::Fast),
        "mid"  => Ok(Preset::Mid),
        "slow" => Ok(Preset::Slow),
        other  => Err(format!("unknown preset '{other}': use fast, mid, or slow")),
    }
}