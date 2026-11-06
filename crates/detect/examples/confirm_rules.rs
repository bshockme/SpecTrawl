//! False and true sync confirmations under each `sync::Confirm` rule: synthetic noise, random FSK and FM voice-like
//! windows (`TRIALS`, default 100), then the real captures given as SigMF stems. Run with `--release`.
use detect::sync::{Confirm, best_with};
use num_complex::Complex32;
use std::f64::consts::TAU;

struct Rng(u64);
impl Rng {
    fn u(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn g(&mut self) -> f64 {
        (-2.0 * self.u().max(1e-300).ln()).sqrt() * (TAU * self.u()).cos()
    }
}
const FS: f64 = 78_125.0;
fn noise(r: &mut Rng, n: usize, amp: f64) -> Vec<Complex32> {
    (0..n).map(|_| Complex32::new((r.g() * amp) as f32, (r.g() * amp) as f32)).collect()
}
fn fsk(r: &mut Rng, n: usize, baud: f64, levels: &[f64], snr_db: f64) -> Vec<Complex32> {
    let mut ph = 0.0;
    let mut dev = 0.0;
    let mut left = 0.0;
    let na = 10f64.powf(-snr_db / 20.0) / 2f64.sqrt();
    (0..n)
        .map(|_| {
            if left <= 0.0 {
                left += FS / baud;
                dev = levels[(r.u() * levels.len() as f64) as usize % levels.len()];
            }
            left -= 1.0;
            ph += TAU * dev / FS;
            Complex32::new((ph.cos() + r.g() * na) as f32, (ph.sin() + r.g() * na) as f32)
        })
        .collect()
}
fn fm_voice(r: &mut Rng, n: usize, snr_db: f64) -> Vec<Complex32> {
    let na = 10f64.powf(-snr_db / 20.0) / 2f64.sqrt();
    let tones: Vec<(f64, f64)> = (0..5).map(|_| (300.0 + r.u() * 2700.0, r.u())).collect();
    let mut ph = 0.0;
    (0..n)
        .map(|i| {
            let t = i as f64 / FS;
            let m: f64 =
                tones.iter().map(|(f, p)| (TAU * (f * t + p)).sin() * (0.5 + 0.5 * (TAU * 3.0 * t + p * 7.0).sin())).sum::<f64>() / 2.5;
            ph += TAU * 3000.0 * m / FS;
            Complex32::new((ph.cos() + r.g() * na) as f32, (ph.sin() + r.g() * na) as f32)
        })
        .collect()
}
fn confirmed(x: &[Complex32], fs: f64, rule: Confirm) -> Option<String> {
    for cutoff in [4_000.0, 6_500.0] {
        if let Some(b) = best_with(&detect::sync_hits(x, fs, cutoff), rule) {
            return Some(format!("{} {} x{}", b.protocol, b.detail, b.count));
        }
    }
    None
}
fn load(stem: &str) -> (Vec<Complex32>, f64) {
    let meta = std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap();
    let rate: f64 = meta
        .split("\"core:sample_rate\"")
        .nth(1)
        .unwrap()
        .trim_start_matches([':', ' '])
        .split([',', '}', '\n'])
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let b = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x = if meta.contains("ci16") {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| Complex32::new(i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0, i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0))
            .collect()
    } else {
        b.as_chunks::<8>()
            .0
            .iter()
            .map(|c| Complex32::new(f32::from_le_bytes([c[0], c[1], c[2], c[3]]), f32::from_le_bytes([c[4], c[5], c[6], c[7]])))
            .collect()
    };
    (x, rate)
}
fn main() {
    let rules = [Confirm::AnyPeriods, Confirm::AnyPeriodsTight, Confirm::WithinFrame];
    let n = (1.5 * FS) as usize;
    let trials = std::env::var("TRIALS").ok().and_then(|s| s.parse().ok()).unwrap_or(100usize);
    let kinds: Vec<(&str, Box<dyn Fn(&mut Rng) -> Vec<Complex32>>)> = vec![
        ("noise", Box::new(move |r: &mut Rng| noise(r, n, 1.0))),
        ("4FSK 4800 random 20dB", Box::new(move |r: &mut Rng| fsk(r, n, 4800.0, &[-1800.0, -600.0, 600.0, 1800.0], 20.0))),
        ("4FSK 4800 random 8dB", Box::new(move |r: &mut Rng| fsk(r, n, 4800.0, &[-1800.0, -600.0, 600.0, 1800.0], 8.0))),
        ("4FSK 2400 random 20dB", Box::new(move |r: &mut Rng| fsk(r, n, 2400.0, &[-800.0, -266.0, 266.0, 800.0], 20.0))),
        ("2FSK 1200 random 20dB", Box::new(move |r: &mut Rng| fsk(r, n, 1200.0, &[-4500.0, 4500.0], 20.0))),
        ("4FSK 9600 random 20dB", Box::new(move |r: &mut Rng| fsk(r, n, 9600.0, &[-2900.0, -966.0, 966.0, 2900.0], 20.0))),
        ("FM voice-like 20dB", Box::new(move |r: &mut Rng| fm_voice(r, n, 20.0))),
    ];
    println!("FALSE confirmations per {trials} windows of 1.5 s at 78.125 kHz:");
    for (name, make) in &kinds {
        let mut r = Rng(0x9e3779b97f4a7c15 ^ name.len() as u64 * 7919);
        let mut fp = [0usize; 3];
        let mut what = std::collections::BTreeMap::new();
        for _ in 0..trials {
            let x = make(&mut r);
            for (k, rule) in rules.iter().enumerate() {
                if let Some(s) = confirmed(&x, FS, *rule) {
                    fp[k] += 1;
                    if k == 0 {
                        *what.entry(s.split(" x").next().unwrap().to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
        println!("  {name:24} AnyPeriods {:3}  Tight {:3}  WithinFrame {:3}   {:?}", fp[0], fp[1], fp[2], what);
    }
    println!("TRUE confirmations on the real captures (1.5 s windows):");
    for stem in std::env::args().skip(1) {
        let (x, fs) = load(&stem);
        let w = (1.5 * fs) as usize;
        let mut ok = [0usize; 3];
        let mut tot = 0;
        let mut counts = [0usize; 3];
        for win in x.chunks(w).filter(|c| c.len() == w) {
            tot += 1;
            for (k, rule) in rules.iter().enumerate() {
                if let Some(s) = confirmed(win, fs, *rule) {
                    ok[k] += 1;
                    counts[k] += s.rsplit('x').next().unwrap().parse::<usize>().unwrap();
                }
            }
        }
        println!(
            "  {:34} of {tot:2}: AnyPeriods {:2} (syncs {:4})  Tight {:2} ({:4})  WithinFrame {:2} ({:4})",
            stem.rsplit('/').next().unwrap(),
            ok[0],
            counts[0],
            ok[1],
            counts[1],
            ok[2],
            counts[2]
        );
    }
}
