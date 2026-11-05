//! The per-receiver status sentence. Pure: no clock, no I/O.

use api::{Health, Mode, Status, fmt};

/// Everything the status sentence is built from, gathered by the publisher.
pub struct StatusInput {
    pub mode: Mode,
    pub center_hz: f64,
    pub sample_rate: f64,
    pub held_secs: f64,
    pub guaranteed_secs: f64,
    pub channels: usize,
    pub worst_lag_secs: f64,
    pub shed: usize,
    pub device_lost: bool,
    /// Seconds since the head last advanced.
    pub silent_secs: f64,
    pub stage_behind_secs: f64,
    /// In Sweep mode, its progress (`None` until the sweep has started).
    pub sweep: Option<api::SweepView>,
}

/// The one-sentence status of a receiver; the first matching rule wins.
pub fn status_line(i: &StatusInput) -> Status {
    let status = |health, text: String, reason: Option<String>| Status { health, text, reason };
    if i.device_lost {
        return status(Health::Lost, "Device lost".into(), Some(format!("no samples for {}", fmt::secs(i.silent_secs))));
    }
    if i.silent_secs >= 1.0 {
        return status(
            Health::Warning,
            format!("Waiting for samples at {}", fmt::mhz(i.center_hz)),
            Some(format!("none for {}", fmt::secs(i.silent_secs))),
        );
    }
    if i.mode == Mode::Sweep {
        let Some(s) = &i.sweep else { return status(Health::Ok, "Starting the sweep".into(), None) };
        let board = if s.on_board { " on the board" } else { "" };
        let span = format!("Sweeping {:.1}–{:.1} MHz{board}", s.start_hz / 1e6, s.stop_hz / 1e6);
        let measured = if s.measured_hops < s.hops { format!(" ({} of {} hops)", s.measured_hops, s.hops) } else { String::new() };
        if s.passes == 0 {
            return status(Health::Ok, format!("{span}{measured}: first pass, hop {} of {}", s.current_hop + 1, s.hops), None);
        }
        let text = format!(
            "{span}{measured}: pass {}, {:.0} hops/s ({:.0} MHz/s)",
            fmt::secs(s.last_pass_s),
            s.hops_per_sec,
            s.hops_per_sec * s.hop_hz / 1e6
        );
        if s.unseen_retunes > 0 && s.on_board {
            let reason = format!("the board stopped sending hops {} times and was restarted", s.unseen_retunes);
            return status(Health::Warning, text, Some(reason));
        }
        if s.unseen_retunes > 0 {
            let reason = format!("{} retunes never showed in the stream; those hops were measured from the head", s.unseen_retunes);
            return status(Health::Warning, text, Some(reason));
        }
        return status(Health::Ok, text, None);
    }
    let mut live = format!(
        "Live at {}, {}: holding {} (guaranteed {}), {}",
        fmt::mhz(i.center_hz),
        fmt::rate(i.sample_rate),
        fmt::secs(i.held_secs),
        fmt::secs(i.guaranteed_secs),
        fmt::count(i.channels, "channel"),
    );
    if i.worst_lag_secs >= 0.1 {
        live.push_str(&format!(", worst lag {}", fmt::secs(i.worst_lag_secs)));
    }
    if i.shed > 0 {
        let reason = format!("{} shed: the pool is saturated", fmt::count(i.shed, "channel"));
        return status(Health::Warning, live, Some(reason));
    }
    if i.stage_behind_secs >= 0.5 {
        let reason = format!("the FFT is {} behind the device", fmt::secs(i.stage_behind_secs));
        return status(Health::Warning, live, Some(reason));
    }
    status(Health::Ok, live, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> StatusInput {
        StatusInput {
            mode: Mode::Live,
            center_hz: 855.25e6,
            sample_rate: 10e6,
            held_secs: 160.0,
            guaranteed_secs: 30.0,
            channels: 3,
            worst_lag_secs: 0.0,
            shed: 0,
            device_lost: false,
            silent_secs: 0.0,
            stage_behind_secs: 0.0,
            sweep: None,
        }
    }

    #[test]
    fn a_healthy_receiver_says_what_it_holds() {
        let s = status_line(&base());
        assert_eq!(s.health, Health::Ok);
        assert_eq!(s.text, "Live at 855.250 MHz, 10 MS/s: holding 2 m 40 s (guaranteed 30 s), 3 channels");
        assert_eq!(s.reason, None);
    }

    #[test]
    fn lag_shed_and_behind_add_their_reasons() {
        let s = status_line(&StatusInput { worst_lag_secs: 0.3, ..base() });
        assert!(s.text.ends_with(", worst lag 0.3 s"));
        let s = status_line(&StatusInput { shed: 2, ..base() });
        assert_eq!((s.health, s.reason.as_deref()), (Health::Warning, Some("2 channels shed: the pool is saturated")));
        let s = status_line(&StatusInput { stage_behind_secs: 0.7, ..base() });
        assert_eq!(s.reason.as_deref(), Some("the FFT is 0.7 s behind the device"));
    }

    #[test]
    fn silence_and_loss_come_first() {
        let s = status_line(&StatusInput { silent_secs: 1.3, ..base() });
        assert_eq!(
            (s.health, s.text.as_str(), s.reason.as_deref()),
            (Health::Warning, "Waiting for samples at 855.250 MHz", Some("none for 1.3 s"))
        );
        let s = status_line(&StatusInput { device_lost: true, silent_secs: 2.0, shed: 1, ..base() });
        assert_eq!((s.health, s.text.as_str(), s.reason.as_deref()), (Health::Lost, "Device lost", Some("no samples for 2.0 s")));
    }

    #[test]
    fn sweep_mode_says_what_it_sweeps_and_how_fast() {
        let s = status_line(&StatusInput { mode: Mode::Sweep, ..base() });
        assert_eq!((s.text.as_str(), s.health), ("Starting the sweep", Health::Ok));
        let view = api::SweepView {
            start_hz: 24e6,
            stop_hz: 1797e6,
            hop_hz: 9e6,
            hops: 197,
            measured_hops: 197,
            current_hop: 40,
            passes: 0,
            last_pass_s: 0.0,
            hops_per_sec: 0.0,
            redone_hops: 0,
            settle_retries: 0,
            slow_hops: 0,
            lock_waits: 0,
            unseen_retunes: 0,
            on_board: false,
        };
        let s = status_line(&StatusInput { mode: Mode::Sweep, sweep: Some(view.clone()), ..base() });
        assert_eq!(s.text, "Sweeping 24.0–1797.0 MHz: first pass, hop 41 of 197");
        let done = api::SweepView { passes: 3, last_pass_s: 1.3, hops_per_sec: 152.0, ..view.clone() };
        let s = status_line(&StatusInput { mode: Mode::Sweep, sweep: Some(done.clone()), ..base() });
        assert_eq!((s.text.as_str(), s.health), ("Sweeping 24.0–1797.0 MHz: pass 1.3 s, 152 hops/s (1368 MHz/s)", Health::Ok));
        let part = api::SweepView { measured_hops: 12, ..done.clone() };
        assert!(status_line(&StatusInput { mode: Mode::Sweep, sweep: Some(part), ..base() }).text.contains("(12 of 197 hops)"));
        let unseen = api::SweepView { unseen_retunes: 2, ..done.clone() };
        let s = status_line(&StatusInput { mode: Mode::Sweep, sweep: Some(unseen), ..base() });
        assert_eq!(s.health, Health::Warning);
        assert!(s.reason.unwrap().starts_with("2 retunes never showed"));
        let board = api::SweepView { on_board: true, last_pass_s: 0.3, hops_per_sec: 650.0, ..done };
        let s = status_line(&StatusInput { mode: Mode::Sweep, sweep: Some(board), ..base() });
        assert_eq!(s.text, "Sweeping 24.0–1797.0 MHz on the board: pass 0.3 s, 650 hops/s (5850 MHz/s)");
    }
}
