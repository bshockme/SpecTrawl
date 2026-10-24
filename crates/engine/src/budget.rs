//! Ring budget across sources (spec 4.1).
//!
//! Pure arithmetic: how many blocks of ceiling and how many guaranteed
//! samples each source's ring gets. The keeper calls [`RingBudget::allowances`]
//! on every tick and applies the result to the rings.
//!
//! Sweep sources hold a fixed `sweep_secs`. Every live source gets a
//! guaranteed window of `guaranteed_secs`. What is left of the shared ceiling
//! is split so each live source holds the same number of seconds of history
//! per unit weight. Weight 0 means guaranteed only.

use std::sync::Arc;

use parking_lot::Mutex;

/// Bytes per complex sample (two f16/i16 components).
const BYTES_PER_SAMPLE: f64 = 4.0;

pub use api::{Mode, SourceId};

/// User-tunable budget parameters.
#[derive(Debug, Clone)]
pub struct BudgetConfig {
    pub guaranteed_secs: f64,
    pub ceiling_bytes: u64,
    pub sweep_secs: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self { guaranteed_secs: 30.0, ceiling_bytes: RingBudget::physical_ram_bytes() / 2, sweep_secs: 1.0 }
    }
}

/// What one source's ring may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allowance {
    pub ceiling_blocks: u32,
    pub guaranteed_samples: u64,
}

#[derive(Debug, Clone)]
struct Member {
    id: SourceId,
    rate: f64,
    mode: Mode,
    weight: f32,
    block_bytes: usize,
}

impl Member {
    fn bytes_per_sec(&self) -> f64 {
        self.rate * BYTES_PER_SAMPLE
    }

    fn blocks_for_secs(&self, secs: f64) -> u64 {
        ceil_blocks(secs * self.bytes_per_sec(), self.block_bytes)
    }
}

fn ceil_blocks(bytes: f64, block_bytes: usize) -> u64 {
    if bytes <= 0.0 || block_bytes == 0 {
        return 0;
    }
    (bytes / block_bytes as f64).ceil() as u64
}

fn samples_for_secs(secs: f64, rate: f64) -> u64 {
    (secs * rate).max(0.0) as u64
}

fn clamp_blocks(blocks: u64) -> u32 {
    blocks.min(u32::MAX as u64) as u32
}

/// Shared budget; one per engine, handed out as `Arc`.
pub struct RingBudget {
    cfg: Mutex<BudgetConfig>,
    members: Mutex<Vec<Member>>,
}

impl RingBudget {
    pub fn new(cfg: BudgetConfig) -> Arc<Self> {
        Arc::new(Self { cfg: Mutex::new(cfg), members: Mutex::new(Vec::new()) })
    }

    /// Total physical RAM in bytes (8 GiB where it cannot be determined).
    pub fn physical_ram_bytes() -> u64 {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
            // SAFETY: a zeroed MEMORYSTATUSEX with dwLength set is the documented input.
            unsafe {
                let mut st: MEMORYSTATUSEX = std::mem::zeroed();
                st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
                if GlobalMemoryStatusEx(&mut st) != 0 && st.ullTotalPhys > 0 {
                    return st.ullTotalPhys;
                }
            }
            8 << 30
        }
        #[cfg(unix)]
        {
            // SAFETY: sysconf has no preconditions.
            let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
            if pages > 0 && size > 0 { (pages as u64).saturating_mul(size as u64) } else { 8 << 30 }
        }
        #[cfg(not(any(windows, unix)))]
        {
            8 << 30
        }
    }

    pub fn set_config(&self, cfg: BudgetConfig) {
        *self.cfg.lock() = cfg;
    }

    /// Register a source. Joining an id that is already a member replaces
    /// its entry (idempotent re-registration).
    pub fn join(&self, id: SourceId, rate: f64, mode: Mode, weight: f32, block_bytes: usize) {
        let m = Member { id, rate, mode, weight, block_bytes };
        let mut members = self.members.lock();
        match members.iter_mut().find(|x| x.id == id) {
            Some(slot) => *slot = m,
            None => members.push(m),
        }
    }

    /// Change a member's rate and mode. No-op for an unknown id.
    pub fn update(&self, id: SourceId, rate: f64, mode: Mode) {
        if let Some(m) = self.members.lock().iter_mut().find(|x| x.id == id) {
            m.rate = rate;
            m.mode = mode;
        }
    }

    /// Change a member's weight (0 = guaranteed only). No-op for an unknown id.
    pub fn set_weight(&self, id: SourceId, weight: f32) {
        if let Some(m) = self.members.lock().iter_mut().find(|x| x.id == id) {
            m.weight = weight;
        }
    }

    /// Remove a member. No-op for an unknown id.
    pub fn leave(&self, id: SourceId) {
        self.members.lock().retain(|m| m.id != id);
    }

    /// Current allowance for every member, in join order.
    pub fn allowances(&self) -> Vec<(SourceId, Allowance)> {
        let cfg = self.cfg.lock().clone();
        let members = self.members.lock();

        // First pass: what sweep ceilings and live guarantees already claim.
        let mut reserved: f64 = 0.0;
        let mut weighted_rate_bytes: f64 = 0.0;
        for m in members.iter() {
            match m.mode {
                Mode::Sweep => {
                    reserved += sweep_blocks(m, &cfg) as f64 * m.block_bytes as f64;
                }
                Mode::Live => {
                    reserved += m.blocks_for_secs(cfg.guaranteed_secs) as f64 * m.block_bytes as f64;
                    weighted_rate_bytes += f64::from(m.weight.max(0.0)) * m.bytes_per_sec();
                }
            }
        }
        let remainder = cfg.ceiling_bytes as f64 - reserved;
        let share = if remainder > 0.0 && weighted_rate_bytes > 0.0 { remainder / weighted_rate_bytes } else { 0.0 };

        // Second pass: per-member result.
        let mut out = Vec::with_capacity(members.len());
        for m in members.iter() {
            let a = match m.mode {
                Mode::Sweep => Allowance {
                    ceiling_blocks: clamp_blocks(sweep_blocks(m, &cfg)),
                    guaranteed_samples: samples_for_secs(cfg.sweep_secs, m.rate),
                },
                Mode::Live => {
                    let g = m.blocks_for_secs(cfg.guaranteed_secs);
                    let extra = if share > 0.0 && m.block_bytes > 0 {
                        (f64::from(m.weight.max(0.0)) * share * m.bytes_per_sec() / m.block_bytes as f64).floor() as u64
                    } else {
                        0
                    };
                    Allowance {
                        ceiling_blocks: clamp_blocks((g + extra + 1).max(2)),
                        guaranteed_samples: samples_for_secs(cfg.guaranteed_secs, m.rate),
                    }
                }
            };
            out.push((m.id, a));
        }
        out
    }
}

fn sweep_blocks(m: &Member, cfg: &BudgetConfig) -> u64 {
    (m.blocks_for_secs(cfg.sweep_secs) + 1).max(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    const MB: usize = 1 << 20;

    #[test]
    fn live_sources_get_equal_seconds_not_equal_bytes() {
        let b = RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 1000 * MB as u64, sweep_secs: 1.0 });
        b.join(SourceId(1), 10e6, Mode::Live, 1.0, MB); // 40 MB/s
        b.join(SourceId(2), 2.5e6, Mode::Live, 1.0, MB); // 10 MB/s
        let a: std::collections::HashMap<_, _> = b.allowances().into_iter().collect();
        let secs = |id, rate: f64| a[&id].ceiling_blocks as f64 * MB as f64 / (rate * 4.0);
        assert!((secs(SourceId(1), 10e6) - secs(SourceId(2), 2.5e6)).abs() < 0.2, "{a:?}"); // within one block of each other
        let expect = 1000.0 * 1048576.0 / 50e6; // 1000 MiB shared at 50 MB/s combined: 20.97 s each
        assert!((secs(SourceId(1), 10e6) - expect).abs() < 0.5);
        assert_eq!(a[&SourceId(1)].guaranteed_samples, 10_000_000);
    }

    #[test]
    fn sweep_sources_hold_one_second_and_do_not_share() {
        let b = RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 1000 * MB as u64, sweep_secs: 1.0 });
        b.join(SourceId(1), 10e6, Mode::Live, 1.0, MB);
        b.join(SourceId(2), 10e6, Mode::Sweep, 1.0, MB);
        let a: std::collections::HashMap<_, _> = b.allowances().into_iter().collect();
        assert_eq!(a[&SourceId(2)].ceiling_blocks, 40); // ceil(40e6 B / 1 MiB) = 39, plus the writer's block
        assert!(a[&SourceId(1)].ceiling_blocks >= 950, "{a:?}"); // nearly everything
        b.update(SourceId(2), 10e6, Mode::Live);
        let a: std::collections::HashMap<_, _> = b.allowances().into_iter().collect();
        assert!((a[&SourceId(1)].ceiling_blocks as i64 - a[&SourceId(2)].ceiling_blocks as i64).abs() <= 1);
    }

    #[test]
    fn rate_update_changes_allowance() {
        let b = RingBudget::new(BudgetConfig { guaranteed_secs: 2.0, ceiling_bytes: 100 * MB as u64, sweep_secs: 1.0 });
        b.join(SourceId(1), 10e6, Mode::Live, 1.0, MB);
        let before = b.allowances()[0].1;
        b.update(SourceId(1), 2.5e6, Mode::Live);
        let after = b.allowances()[0].1;
        assert_eq!(before.guaranteed_samples, 20_000_000);
        assert_eq!(after.guaranteed_samples, 5_000_000);
        assert_eq!(before.ceiling_blocks, after.ceiling_blocks); // same bytes, more seconds
    }

    #[test]
    fn weight_zero_means_guaranteed_only_and_everyone_gets_at_least_two_blocks() {
        let b = RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 10 * MB as u64, sweep_secs: 1.0 });
        b.join(SourceId(1), 10e6, Mode::Live, 0.0, MB);
        b.join(SourceId(2), 10e6, Mode::Live, 1.0, MB);
        let a: std::collections::HashMap<_, _> = b.allowances().into_iter().collect();
        assert_eq!(a[&SourceId(1)].ceiling_blocks, 40); // 39 guaranteed + 1, nothing extra
        assert!(a[&SourceId(2)].ceiling_blocks >= 40);
        assert!(RingBudget::physical_ram_bytes() >= 1 << 30);
    }
}
