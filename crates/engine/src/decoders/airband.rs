//! Air band voice, native (spec §4.6's hot path; step 7 since OnAir was dropped): AM on the VHF and UHF air bands' 25 kHz
//! raster. Built on the records, not a second detector: a record on a raster channel that the bank's look found to be
//! AM (`detect`'s modulation rung, which measures a carrier's sidebands coherently when the detector saw only the
//! carrier) gets an "Air band voice" claim naming its channel, filed under [`AIRBAND`], so its emitter headlines as the
//! channel. [`channels`] is the CHANNELS list (spec §5), built from claims only, as [`super::systems`] is.
//!
//! The raster is the US one: 25 kHz everywhere (8.33 kHz channels, Europe's, are not named yet).

use std::collections::BTreeMap;
use std::sync::Arc;

use api::{ChannelRow, ChannelsView, Claim, Evidence, Identity, RecordsView, SignalRecord};

use crate::records::RecordBook;

/// What the air band's claims are filed under in the book.
pub const AIRBAND: &str = "airband";
/// The claims' decoder mode.
pub const MODE: &str = "Air band AM";
/// Under a decoded identity (0.95), over a shape alone (0.6): the channel and the AM are both measured, but nothing
/// was decoded.
const CONFIDENCE: f32 = 0.8;

/// A band of the plan: its name, edges and raster.
struct Band {
    plan: &'static str,
    lo_hz: f64,
    hi_hz: f64,
    raster_hz: f64,
}

/// Civil VHF voice and data (118–137 MHz), military UHF (225–400 MHz).
const BANDS: [Band; 2] = [
    Band { plan: "Air band", lo_hz: 118e6, hi_hz: 137e6, raster_hz: 25e3 },
    Band { plan: "Military air", lo_hz: 225e6, hi_hz: 400e6, raster_hz: 25e3 },
];
/// How far a carrier may sit from its channel: offset-carrier sites (several transmitters on one channel, for coverage)
/// put theirs up to 7.5 kHz off.
const TOLERANCE_HZ: f64 = 8e3;
/// Widest a record on one channel may be.
const MAX_WIDTH_HZ: f64 = 25e3;

/// A raster channel: its plan, centre and name ("128.775").
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub plan: &'static str,
    pub hz: f64,
    pub name: String,
}

/// The raster channel a record sits on, if it is in an air band, near a channel and narrow enough to be on one.
pub fn channel_of(r: &SignalRecord) -> Option<Channel> {
    if r.bandwidth_hz() > MAX_WIDTH_HZ {
        return None;
    }
    channel_at(r.center_hz())
}

/// The raster channel within [`TOLERANCE_HZ`] of `hz`.
pub fn channel_at(hz: f64) -> Option<Channel> {
    let b = BANDS.iter().find(|b| (b.lo_hz..b.hi_hz).contains(&hz))?;
    let ch = (hz / b.raster_hz).round() * b.raster_hz;
    ((hz - ch).abs() <= TOLERANCE_HZ).then(|| Channel { plan: b.plan, hz: ch, name: format!("{:.3}", ch / 1e6) })
}

/// What the bank's own look (its `Features` claim) called the record, if it was AM.
fn bank_says_am(r: &SignalRecord) -> bool {
    r.claims.iter().any(|c| matches!(c.by, Evidence::Features { .. }) && c.label == "AM")
}

/// A decoder or sync claim other than this module's: the record is something identified (ACARS is AM too, but data).
fn identified(r: &SignalRecord) -> bool {
    r.claims.iter().any(|c| match &c.by {
        Evidence::Decoder { mode, .. } => mode != MODE,
        Evidence::SyncBank { .. } => true,
        _ => false,
    })
}

fn is_ours(c: &Claim) -> bool {
    matches!(&c.by, Evidence::Decoder { mode, .. } if mode == MODE)
}

/// The claim the air band makes about a record: voice on its channel, or nothing.
pub fn claim_for(r: &SignalRecord) -> Option<Claim> {
    if !bank_says_am(r) || identified(r) {
        return None;
    }
    let ch = channel_of(r)?;
    let off = r.center_hz() - ch.hz;
    let offset = if off.abs() >= 1e3 { format!(", carrier {:+.1} kHz", off / 1e3) } else { String::new() };
    let voice = if ch.plan == "Air band" { "Air band voice" } else { "Military air voice" };
    Some(Claim::new(
        voice,
        CONFIDENCE,
        Evidence::Decoder {
            mode: MODE.into(),
            locked: true,
            summary: format!("{} {} MHz: AM voice{offset}", ch.plan, ch.name),
            identity: Some(Identity::Channel { plan: ch.plan.into(), name: ch.name }),
        },
    ))
}

/// Files (or withdraws) the air band's claim on every record in `view` whose claim changed: a later look that no
/// longer says AM takes it back.
pub(crate) fn file_claims(view: &RecordsView, book: &RecordBook) {
    for r in view.records.iter() {
        let want = claim_for(r);
        let have: Vec<&Claim> = r.claims.iter().filter(|c| is_ours(c)).collect();
        let same = match (&want, have.as_slice()) {
            (None, []) => true,
            (Some(w), [h]) => w == *h,
            _ => false,
        };
        if !same {
            book.set_claims(r.id, AIRBAND, want.into_iter().collect());
        }
    }
}

/// The CHANNELS list: every raster channel with a record on it that is voice (this module's claim) or decoded data
/// (another decoder's claim, ACARS). Records with neither on a channel that has one count as its carrier (an AM call's
/// pauses are its bare carrier); a channel with only those isn't listed (a birdie sits on a raster point as readily).
pub fn channels(view: &RecordsView) -> ChannelsView {
    struct Acc<'a> {
        ch: Channel,
        records: Vec<&'a Arc<SignalRecord>>,
    }
    let mut by: BTreeMap<(usize, i64), Acc<'_>> = BTreeMap::new();
    for r in view.records.iter() {
        let Some(ch) = channel_of(r) else { continue };
        let plan = BANDS.iter().position(|b| b.plan == ch.plan).unwrap_or(0);
        let key = (plan, (ch.hz / 1e3).round() as i64);
        by.entry(key).or_insert_with(|| Acc { ch, records: Vec::new() }).records.push(r);
    }
    let decoder = |r: &SignalRecord| {
        r.claims
            .iter()
            .filter_map(|c| match &c.by {
                Evidence::Decoder { mode, summary, .. } if mode != MODE => Some((mode.clone(), summary.clone(), c.confidence)),
                _ => None,
            })
            .max_by(|a, b| a.2.total_cmp(&b.2))
    };
    let mut channels: Vec<ChannelRow> = by
        .into_values()
        .filter_map(|mut a| {
            a.records.sort_by_key(|r| (r.t_start, r.id));
            let (mut voice, mut data, mut carrier) = (0, 0, 0);
            let mut modes: Vec<String> = Vec::new();
            let mut summary = None;
            for r in a.records.iter().rev() {
                if let Some((mode, s, _)) = decoder(r) {
                    data += 1;
                    if !modes.contains(&mode) {
                        modes.push(mode);
                    }
                    summary.get_or_insert(s);
                } else if r.claims.iter().any(is_ours) {
                    voice += 1;
                } else {
                    carrier += 1;
                }
            }
            if voice + data == 0 {
                return None;
            }
            let newest = a.records.last()?;
            Some(ChannelRow {
                plan: a.ch.plan.to_string(),
                name: a.ch.name,
                hz: a.ch.hz,
                source: newest.source,
                open: a.records.iter().any(|r| r.open),
                voice,
                data,
                carrier,
                modes,
                summary: summary.unwrap_or_else(|| "AM voice".into()),
                airtime_s: a.records.iter().map(|r| r.duration().as_secs_f64()).sum(),
                peak_dbfs: a.records.iter().map(|r| r.peak_dbfs).fold(f32::NEG_INFINITY, f32::max),
                first_heard: a.records.iter().map(|r| r.t_start).min()?,
                last_heard: a.records.iter().map(|r| r.t_end).max()?,
                records: a.records.iter().map(|r| r.id).collect(),
            })
        })
        .collect();
    channels.sort_by(|a, b| a.hz.total_cmp(&b.hz));
    ChannelsView { channels }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_are_named_on_the_raster_and_offsets_are_tolerated() {
        let c = channel_at(128.7753e6).unwrap();
        assert_eq!((c.plan, c.name.as_str()), ("Air band", "128.775"));
        assert_eq!(channel_at(131.5425e6).unwrap().name, "131.550", "7.5 kHz offset carrier");
        assert_eq!(channel_at(124.8625e6), None, "between two channels");
        assert_eq!(channel_at(117.95e6), None, "VOR/ILS, not voice");
        assert_eq!(channel_at(282.8e6).unwrap().plan, "Military air");
        assert_eq!(channel_at(855.5e6), None);
    }
}
