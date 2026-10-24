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

/// `2 m 40 s` (60 s and over), `30 s` (10 to 60, rounded), `0.5 s` (under 10, one decimal); negatives read as 0.
pub fn secs(s: f64) -> String {
    let s = if s.is_nan() { 0.0 } else { s.max(0.0) };
    if s >= 60.0 {
        let total = s.round() as u64;
        format!("{} m {} s", total / 60, total % 60)
    } else if s >= 10.0 {
        format!("{} s", s.round() as u64)
    } else if s == 0.0 {
        "0 s".to_string()
    } else {
        format!("{s:.1} s")
    }
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
        assert_eq!(count(1, "channel"), "1 channel");
        assert_eq!(count(3, "channel"), "3 channels");
    }
}
