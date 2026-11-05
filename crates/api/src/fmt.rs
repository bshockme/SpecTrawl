//! Number formatting shared by the interfaces.

/// `855.250 MHz`.
pub fn mhz(hz: f64) -> String {
    format!("{:.3} MHz", hz / 1e6)
}

/// `10 MS/s`, `2.5 MS/s`, `768 kS/s`.
pub fn rate(sps: f64) -> String {
    if sps >= 1e6 {
        format!("{} MS/s", trim(sps / 1e6))
    } else if sps >= 1e3 {
        format!("{} kS/s", trim(sps / 1e3))
    } else {
        format!("{} S/s", trim(sps))
    }
}

fn trim(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `2 m 40 s` (60 s and over), `30 s` (10 to 60, rounded), `0.5 s` (under 10, one decimal), `0 s` (under 0.05);
/// negatives read as 0. The value is rounded before the form is chosen, so `59.6` reads `1 m 0 s` and `9.96` reads `10 s`.
pub fn secs(s: f64) -> String {
    let s = if s.is_nan() { 0.0 } else { s.max(0.0) };
    let tenths = (s * 10.0).round();
    if tenths == 0.0 {
        return "0 s".to_string();
    }
    if tenths < 100.0 {
        return format!("{:.1} s", tenths / 10.0);
    }
    let total = s.round() as u64;
    if total >= 60 { format!("{} m {} s", total / 60, total % 60) } else { format!("{total} s") }
}

/// `21:47:04` UTC.
pub fn clock(t: std::time::SystemTime) -> String {
    let s = t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("{:02}:{:02}:{:02}", s / 3600 % 24, s / 60 % 60, s % 60)
}

/// `1 channel`, `3 channels`.
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_read_like_the_spec() {
        assert_eq!(mhz(855.25e6), "855.250 MHz");
        assert_eq!(mhz(124.85e6), "124.850 MHz");
        assert_eq!(rate(10e6), "10 MS/s");
        assert_eq!(rate(2.5e6), "2.5 MS/s");
        assert_eq!(rate(768e3), "768 kS/s");
        assert_eq!(secs(160.0), "2 m 40 s");
        assert_eq!(secs(30.0), "30 s");
        assert_eq!(secs(0.5), "0.5 s");
        assert_eq!(secs(-1.0), "0 s");
        // Rounded first, then formatted: no "60 s" and no "10.0 s".
        assert_eq!(secs(59.6), "1 m 0 s");
        assert_eq!(secs(59.4), "59 s");
        assert_eq!(secs(9.96), "10 s");
        assert_eq!(secs(9.94), "9.9 s");
        assert_eq!(secs(0.04), "0 s");
        assert_eq!(secs(0.05), "0.1 s");
        assert_eq!(count(1, "channel"), "1 channel");
        assert_eq!(count(3, "channel"), "3 channels");
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_791_547_624_123);
        assert_eq!(clock(t), "12:07:04");
    }
}
