//! Assembling each tick's receivers and bus view from the engine.

use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use api::{BusView, ConsumerState, ConsumerView, LogKind, Receiver, RingView, StageView};

use super::Log;
use super::devices::Registry;
use super::status::{StatusInput, status_line};
use crate::budget::SourceId;
use crate::engine::Engine;

/// What the publisher remembers about each receiver between ticks.
#[derive(Default)]
pub(crate) struct Watch {
    /// The head last seen and when it was first seen there.
    heads: HashMap<SourceId, (u64, Instant)>,
    /// Receivers whose loss has been logged.
    lost: BTreeSet<SourceId>,
}

impl Watch {
    /// Forget receivers that are no longer open.
    fn retain(&mut self, open: &[SourceId]) {
        self.heads.retain(|id, _| open.contains(id));
        self.lost.retain(|id| open.contains(id));
    }

    /// Seconds since `head` last advanced.
    fn silent_secs(&mut self, id: SourceId, head: u64, now: Instant) -> f64 {
        let e = self.heads.entry(id).or_insert((head, now));
        if e.0 != head {
            *e = (head, now);
        }
        now.duration_since(e.1).as_secs_f64()
    }
}

/// Every open receiver in letter order, and the bus figures, from one `bus.stats()`.
pub(crate) fn receivers(engine: &Engine, registry: &Registry, rows: usize, watch: &mut Watch, log: &mut Log) -> (Vec<Receiver>, BusView) {
    let bus = engine.bus().stats();
    let now = Instant::now();
    watch.retain(&registry.ids());
    let mut out = Vec::new();
    for (id, o) in registry.open_by_letter() {
        let Some(source) = engine.source(id) else { continue };
        let dev = source.snapshot();
        let stats = source.stats();
        let stage = engine.bus().stage(id);
        let stage_stats = stage.as_ref().map(|s| s.stats()).unwrap_or_default();
        let consumers: Vec<ConsumerView> = bus
            .consumers
            .iter()
            .filter(|c| c.channel.source == id)
            .map(|c| ConsumerView {
                id: c.channel.id,
                center_hz: c.channel.center_hz,
                bandwidth_hz: c.channel.bandwidth_hz,
                priority: c.priority,
                lag_secs: c.lag_secs,
                state: c.state,
            })
            .collect();
        let worst_lag_secs = consumers.iter().filter(|c| c.state == ConsumerState::Running).map(|c| c.lag_secs).fold(0.0, f64::max);
        let shed = consumers.iter().filter(|c| c.state == ConsumerState::Shed).count();
        let channels = consumers.iter().filter(|c| c.state != ConsumerState::Ended).count();
        let head = source.head();
        let silent_secs = watch.silent_secs(id, head, now);
        if stats.device_lost && watch.lost.insert(id) {
            log.push(Some(o.letter), LogKind::Device, format!("{}: device lost", o.letter));
        }
        let mode = source.mode();
        let sweep = stage.as_ref().and_then(|s| s.sweep_status()).filter(|_| mode == api::Mode::Sweep).map(|s| api::SweepView {
            start_hz: s.start_hz,
            stop_hz: s.stop_hz(),
            hop_hz: s.hop_hz,
            hops: s.hops,
            measured_hops: s.measured_hops,
            current_hop: s.current_hop,
            passes: s.passes,
            last_pass_s: s.last_pass_s,
            hops_per_sec: s.hops_per_sec,
            redone_hops: s.redone_hops,
            settle_retries: s.settle_retries,
            slow_hops: s.slow_hops,
            lock_waits: s.lock_waits,
            unseen_retunes: s.unseen_retunes,
            on_board: s.on_board,
        });
        let status = status_line(&StatusInput {
            mode,
            center_hz: dev.center_hz,
            sample_rate: dev.sample_rate,
            held_secs: stats.held_secs,
            guaranteed_secs: stats.guaranteed_secs,
            channels,
            worst_lag_secs,
            shed,
            device_lost: stats.device_lost,
            silent_secs,
            stage_behind_secs: stage_stats.behind_secs,
            sweep: sweep.clone(),
        });
        out.push(Receiver {
            id,
            letter: o.letter,
            device: o.info.clone(),
            caps: o.caps.clone(),
            mode,
            center_hz: dev.center_hz,
            sample_rate: dev.sample_rate,
            segment: source.segment_at(head).id,
            gain: o.gain,
            status,
            ring: RingView {
                held_secs: stats.held_secs,
                guaranteed_secs: stats.guaranteed_secs,
                ceiling_secs: stats.ceiling_secs,
                device_gaps: stats.device_gaps,
                starved_samples: stats.starved_samples,
                overloads: stats.overloads,
                device_lost: stats.device_lost,
            },
            stage: StageView {
                blocks: stage_stats.blocks,
                skipped_blocks: stage_stats.skipped_blocks,
                block_micros: stage_stats.block_micros,
                behind_secs: stage_stats.behind_secs,
            },
            head,
            head_time: source.time_of(head),
            rows: stage.map(|s| s.rows().newest(rows)).unwrap_or_default(),
            consumers,
            sweep,
        });
    }
    let view = BusView { workers: bus.workers, saturated: bus.saturated, shed_total: bus.shed_total, panicked_total: bus.panicked_total };
    (out, view)
}
