/// Human-readable byte size.
pub fn human(b: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = K * 1024;
    const G: u64 = M * 1024;
    match b {
        b if b >= G => format!("{:.1} GB", b as f64 / G as f64),
        b if b >= M => format!("{:.1} MB", b as f64 / M as f64),
        b if b >= K => format!("{:.1} KB", b as f64 / K as f64),
        b           => format!("{b} B"),
    }
}

/// Truncate a string to `max` chars, prepending `…` if cut.
pub fn trunc(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let tail: String = chars[chars.len().saturating_sub(max - 1)..].iter().collect();
        format!("…{tail}")
    }
}

/// Replace file extension on a path string.
pub fn with_ext(name: &str, ext: &str) -> String {
    use std::path::Path;
    format!("{}.{ext}", Path::new(name).with_extension("").display())
}

/// Entropy estimate (Shannon, bits per symbol).
pub fn entropy(data: &[u8]) -> f64 {
    let mut freq = [0u64; 256];
    for &b in data { freq[b as usize] += 1; }
    let n = data.len() as f64;
    freq.iter()
        .filter(|&&c| c > 0)
        .map(|&c| { let p = c as f64 / n; -p * p.log2() })
        .sum()
}

/// Format first N bytes as uppercase hex pairs.
pub fn hex_preview(b: &[u8], n: usize) -> String {
    b.iter().take(n).map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(" ")
}
