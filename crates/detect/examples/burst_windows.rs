//! How bursty a channel is, and how long a classification window it needs.
//!
//! For each cf32 SigMF channel capture: the on/off cadence from the envelope
//! gate (burst lengths and the gaps between them), then, for several window
//! lengths, the share of windows (stepped by half a window) in which
//! `detect::classify` names a protocol, with the mean sync count and the
//! features it reports. This is the slotted/bursty-traffic question of the
//! backlog ("TDMA single-slot traffic fragments into many short records")
//! asked of the signal itself.
//!
//!     cargo run --release -p detect --example burst_windows -- [--secs N] <path without extension>...

use num_complex::Complex32;

fn load(stem: &str) -> (Vec<Complex32>, f64) {
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    (x, fs)
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut secs = f64::INFINITY;
    let mut timeline = false;
    let mut records = false;
    while let Some(a) = args.first().cloned() {
        match a.as_str() {
            "--secs" => {
                args.remove(0);
                secs = args.remove(0).parse().unwrap();
            }
            "--timeline" => {
                args.remove(0);
                timeline = true;
            }
            "--records" => {
                args.remove(0);
                records = true;
            }
            _ => break,
        }
    }
    for stem in args {
        let (mut x, fs) = load(&stem);
        x.truncate(((secs * fs) as usize).min(x.len()));
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        // Cadence from 10 ms power blocks against the quiet floor (the 20th
        // percentile block), so an idle channel's noise doesn't read as on.
        let blk = (0.01 * fs) as usize;
        let pw: Vec<f32> =
            x.chunks(blk).map(|c| 10.0 * (c.iter().map(|v| v.norm_sqr()).sum::<f32>() / c.len() as f32 + 1e-20).log10()).collect();
        let mut sorted = pw.clone();
        sorted.sort_by(f32::total_cmp);
        let floor = sorted[sorted.len() / 5];
        let on_blk: Vec<bool> = pw.iter().map(|p| *p > floor + 6.0).collect();
        let mut on: Vec<(usize, usize)> = Vec::new();
        let mut i = 0;
        while i < on_blk.len() {
            if on_blk[i] {
                let s = i;
                while i < on_blk.len() && on_blk[i] {
                    i += 1;
                }
                on.push((s * blk, i * blk));
            } else {
                i += 1;
            }
        }
        if timeline {
            let per = (1.0 / 0.01) as usize; // one char per second
            let line: String = on_blk
                .chunks(per)
                .map(|c| {
                    let k = c.iter().filter(|b| **b).count() * 4 / c.len().max(1);
                    [' ', '.', ':', '|', '#'][k.min(4)]
                })
                .collect();
            println!("  on/off per second: {line}");
        }
        let mut durs: Vec<f64> = on.iter().map(|(s, e)| (e - s) as f64 / fs * 1e3).collect();
        let mut gaps: Vec<f64> = on.windows(2).map(|p| (p[1].0 - p[0].1) as f64 / fs * 1e3).collect();
        let duty = on.iter().map(|(s, e)| e - s).sum::<usize>() as f64 / x.len().max(1) as f64;
        println!("{name}: {:.1} s, {} bursts, duty {:.0}%", x.len() as f64 / fs, on.len(), duty * 100.0);
        println!(
            "  burst ms  p10 {:.0} p50 {:.0} p90 {:.0}   gap ms  p10 {:.0} p50 {:.0} p90 {:.0}",
            pct(&mut durs, 0.1),
            pct(&mut durs, 0.5),
            pct(&mut durs, 0.9),
            pct(&mut gaps, 0.1),
            pct(&mut gaps, 0.5),
            pct(&mut gaps, 0.9)
        );
        if records {
            emulate_records(&x, fs, &on);
        }
        println!("  window   identified   syncs/win   decoded/tried  levels     rate     dev kHz   labels");
        for win_s in [0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0] {
            let win = (win_s * fs) as usize;
            if win > x.len() {
                continue;
            }
            let step = win / 2;
            let (mut n, mut ident, mut syncs) = (0usize, 0usize, 0usize);
            let (mut dec, mut tried) = (0usize, 0usize);
            let (mut lv, mut rate, mut dev) = (std::collections::BTreeMap::<u8, usize>::new(), 0usize, Vec::<f64>::new());
            let mut labels = std::collections::BTreeMap::<String, usize>::new();
            let mut start = 0;
            while start + win <= x.len() {
                let c = detect::classify(&x[start..start + win], fs, 12_500.0);
                n += 1;
                if c.protocol.is_some() {
                    ident += 1;
                    syncs += c.sync.as_ref().map(|s| s.count).unwrap_or(0);
                    if let Some(f) = &c.fields {
                        dec += f.decoded;
                        tried += f.tried;
                    }
                    *lv.entry(c.features.fsk_levels).or_default() += 1;
                    rate += c.features.symbol_rate.is_some() as usize;
                    dev.push(c.features.deviation_hz as f64 / 1e3);
                }
                *labels.entry(c.label.clone()).or_default() += 1;
                start += step;
            }
            let mut top: Vec<(String, usize)> = labels.into_iter().collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            let top: Vec<String> = top.iter().take(3).map(|(l, k)| format!("{l}×{k}")).collect();
            let lvs: Vec<String> = lv.iter().map(|(l, k)| format!("{l}:{k}")).collect();
            println!(
                "  {:>5.2} s  {:>4}/{:<4}  {:>8.1}   {:>6}/{:<6}  {:<10} {:>3}/{:<3}  {:>5.2}   {}",
                win_s,
                ident,
                n,
                syncs as f64 / ident.max(1) as f64,
                dec,
                tried,
                lvs.join(" "),
                rate,
                ident,
                pct(&mut dev, 0.5),
                top.join(", ")
            );
        }
    }
}

/// What the engine would do: bursts closer than the live detector's 0.3 s
/// hangover form one record; records under 40 ms are dropped (min_rows).
/// Each record is classified three ways: on its own extent (what
/// `classify_record` sees), over 0.75 s ending at its end (the old group
/// window), and over the newest four records within 3 s (the new one).
fn emulate_records(x: &[Complex32], fs: f64, bursts: &[(usize, usize)]) {
    let hang = (0.3 * fs) as usize;
    let mut recs: Vec<(usize, usize)> = Vec::new();
    for &(s, e) in bursts {
        match recs.last_mut() {
            Some(last) if s.saturating_sub(last.1) <= hang => last.1 = e,
            _ => recs.push((s, e)),
        }
    }
    recs.retain(|(s, e)| e - s >= (0.04 * fs) as usize);
    let mut durs: Vec<f64> = recs.iter().map(|(s, e)| (e - s) as f64 / fs).collect();
    println!("  engine records: {} (median {:.2} s, p90 {:.2} s)", recs.len(), pct(&mut durs, 0.5), pct(&mut durs, 0.9));
    let ident = |a: usize, b: usize| -> (bool, usize, usize) {
        let c = detect::classify(&x[a..b], fs, 12_500.0);
        let (d, t) = c.fields.as_ref().map(|f| (f.decoded, f.tried)).unwrap_or((0, 0));
        (c.protocol.is_some(), d, t)
    };
    let mut tally = [(0usize, 0usize, 0usize); 3];
    for (k, &(s, e)) in recs.iter().enumerate() {
        let own = (s, e);
        let old = (e.saturating_sub((0.75 * fs) as usize), e);
        let oldest = k.saturating_sub(3);
        let floor = e.saturating_sub((3.0 * fs) as usize);
        let new = (recs[oldest].0.max(floor).min(old.0), e);
        for (i, (a, b)) in [own, old, new].into_iter().enumerate() {
            let (ok, d, t) = ident(a, b);
            tally[i].0 += ok as usize;
            tally[i].1 += d;
            tally[i].2 += t;
        }
    }
    for (i, name) in ["record itself", "0.75 s group window", "4-record window (<= 3 s)"].iter().enumerate() {
        println!("    {name:<26} identified {:>4}/{:<4}  decoded {}/{}", tally[i].0, recs.len(), tally[i].1, tally[i].2);
    }
}
