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
        return status(Health::Warning, "Sweep mode".into(), Some("not re-homed on the new engine yet (step 4a)".into()));
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
    fn sweep_mode_says_it_is_pending() {
        let s = status_line(&StatusInput { mode: Mode::Sweep, ..base() });
        assert_eq!((s.text.as_str(), s.reason.as_deref()), ("Sweep mode", Some("not re-homed on the new engine yet (step 4a)")));
    }
}
