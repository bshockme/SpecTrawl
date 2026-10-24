//! The spectrum trace: a smoothed line with a decaying peak hold, and the plot's scale helpers. Pure.

/// The trace's two lines, one value per history column, in dB.
#[derive(Debug, Clone)]
pub struct Trace {
    /// Exponentially smoothed: `alpha · previous + (1 − alpha) · new`.
    pub smooth: Vec<f32>,
    /// Peak hold, falling `decay_db` per row until a new value beats it.
    pub hold: Vec<f32>,
    pub alpha: f32,
    pub decay_db: f32,
}

impl Default for Trace {
    fn default() -> Self {
        Trace::new()
    }
}

impl Trace {
    pub fn new() -> Trace {
        Trace { smooth: Vec::new(), hold: Vec::new(), alpha: 0.6, decay_db: 0.05 }
    }

    /// Takes in one row's dB; a row of another width starts the trace over. NaN (no data) leaves a column as it was.
    pub fn update(&mut self, db: &[f32]) {
        if self.smooth.len() != db.len() {
            self.smooth = db.to_vec();
            self.hold = db.to_vec();
            return;
        }
        for ((s, h), &v) in self.smooth.iter_mut().zip(self.hold.iter_mut()).zip(db) {
            if v.is_nan() {
                continue;
            }
            *s = if s.is_nan() { v } else { self.alpha * *s + (1.0 - self.alpha) * v };
            *h = if h.is_nan() { v } else { (*h - self.decay_db).max(v) };
        }
    }

    pub fn reset(&mut self) {
        self.smooth.clear();
        self.hold.clear();
    }
}

/// A display range that fits `db`: (10th percentile − 5, 99.5th percentile + 10), ignoring NaN; the default range when empty.
pub fn auto_range(db: &[f32]) -> (f32, f32) {
    let mut v: Vec<f32> = db.iter().copied().filter(|x| !x.is_nan()).collect();
    if v.is_empty() {
        return (-110.0, -45.0);
    }
    v.sort_by(f32::total_cmp);
    let at = |p: f32| v[((v.len() - 1) as f32 * p).round() as usize];
    (at(0.10) - 5.0, at(0.995) + 10.0)
}

/// 1, 2 or 5 × 10^k, the one nearest (geometrically) to `span / n`.
pub fn nice_step(span: f64, n: f64) -> f64 {
    let raw = span / n.max(1.0);
    if !(raw.is_finite() && raw > 0.0) {
        return 1.0;
    }
    let mag = 10f64.powi(raw.log10().floor() as i32);
    let m = raw / mag;
    let k = if m < 2f64.sqrt() {
        1.0
    } else if m < 10f64.sqrt() {
        2.0
    } else if m < 50f64.sqrt() {
        5.0
    } else {
        10.0
    };
    k * mag
}

/// Decimals for MHz labels on ticks `step` Hz apart.
pub fn decimals(step: f64) -> usize {
    let d = (-(step / 1e6).log10() - 1e-9).ceil();
    if d.is_finite() && d > 0.0 { d as usize } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_smooths_and_holds() {
        let mut t = Trace::new();
        t.update(&[-100.0, -50.0]);
        assert_eq!(t.hold, vec![-100.0, -50.0]);
        t.update(&[-100.0, -100.0]);
        assert!((t.smooth[1] - (0.6 * -50.0 + 0.4 * -100.0)).abs() < 1e-4);
        assert!((t.hold[1] - -50.05).abs() < 1e-4);
    }

    #[test]
    fn auto_range_and_nice_steps() {
        let db: Vec<f32> = (0..1000).map(|i| -100.0 + i as f32 * 0.05).collect(); // −100 .. −50.05
        let (lo, hi) = auto_range(&db);
        assert!((lo - (-95.0 - 5.0)).abs() < 0.2 && (hi - (-50.3 + 10.0)).abs() < 0.3, "{lo} {hi}");
        assert_eq!(auto_range(&[]), (-110.0, -45.0));
        assert_eq!(nice_step(10e6, 8.0), 1e6);
        assert_eq!(nice_step(1e6, 8.0), 100e3);
        assert_eq!(decimals(100e3), 1); // 852.1
        assert_eq!(decimals(1e6), 0);
    }
}
