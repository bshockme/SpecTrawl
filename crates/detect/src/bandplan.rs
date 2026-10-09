//! US frequency allocations, for context: which service a frequency belongs
//! to, and so what a signal there is likely to be. Coarse on purpose; the
//! spectral identifier and classifier decide what a signal actually is.

/// Broad kinds of service, for grouping and for the spectrum squelch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Service {
    BroadcastFm,
    BroadcastTv,
    CellularDown,
    CellularUp,
    Aviation,
    AviationNav,
    Marine,
    Weather,
    LandMobile,
    PublicSafetyTrunked,
    Paging,
    Amateur,
    Ism,
    Satellite,
    Military,
    Radar,
}

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Self::BroadcastFm => "FM broadcast",
            Self::BroadcastTv => "TV broadcast",
            Self::CellularDown => "Cellular downlink",
            Self::CellularUp => "Cellular uplink",
            Self::Aviation => "Aviation voice",
            Self::AviationNav => "Aviation nav",
            Self::Marine => "Marine",
            Self::Weather => "Weather",
            Self::LandMobile => "Land mobile",
            Self::PublicSafetyTrunked => "Trunked radio",
            Self::Paging => "Paging",
            Self::Amateur => "Amateur",
            Self::Ism => "ISM",
            Self::Satellite => "Satellite",
            Self::Military => "Military",
            Self::Radar => "Radar/nav",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Band {
    pub lo_hz: f64,
    pub hi_hz: f64,
    pub name: &'static str,
    pub service: Service,
}

const fn b(lo_mhz: f64, hi_mhz: f64, name: &'static str, service: Service) -> Band {
    Band { lo_hz: lo_mhz * 1e6, hi_hz: hi_mhz * 1e6, name, service }
}

use Service::*;

/// Narrower entries first: `lookup` returns the first match.
pub const BANDS: &[Band] = &[
    b(121.45, 121.55, "Aviation emergency", Aviation),
    b(162.40, 162.55, "NOAA Weather Radio", Weather),
    b(156.0, 162.025, "Marine VHF", Marine),
    b(137.0, 138.0, "Weather satellites", Satellite),
    b(433.05, 434.79, "ISM 433", Ism),
    b(462.55, 462.725, "FRS/GMRS", LandMobile),
    b(467.5625, 467.7125, "FRS", LandMobile),
    b(929.0, 932.0, "Paging", Paging),
    b(1089.0, 1091.0, "ADS-B / Mode S", AviationNav),
    b(1029.0, 1031.0, "SSR interrogation", AviationNav),
    b(769.0, 775.0, "700 MHz public safety", PublicSafetyTrunked),
    b(799.0, 805.0, "700 MHz public safety", PublicSafetyTrunked),
    b(758.0, 768.0, "LTE band 14 (FirstNet) down", CellularDown),
    b(788.0, 798.0, "LTE band 14 (FirstNet) up", CellularUp),
    b(54.0, 72.0, "TV VHF-low (ch 2-4)", BroadcastTv),
    b(76.0, 88.0, "TV VHF-low (ch 5-6)", BroadcastTv),
    b(87.8, 108.0, "FM broadcast", BroadcastFm),
    b(108.0, 118.0, "VOR/ILS", AviationNav),
    b(118.0, 137.0, "Aviation voice (AM)", Aviation),
    b(144.0, 148.0, "Amateur 2 m", Amateur),
    b(148.0, 174.0, "VHF land mobile", LandMobile),
    b(174.0, 216.0, "TV VHF-high (ch 7-13)", BroadcastTv),
    b(222.0, 225.0, "Amateur 1.25 m", Amateur),
    b(225.0, 400.0, "Military aviation (AM)", Military),
    b(420.0, 450.0, "Amateur 70 cm", Amateur),
    b(450.0, 470.0, "UHF land mobile", LandMobile),
    b(470.0, 608.0, "TV UHF (ch 14-36)", BroadcastTv),
    b(617.0, 652.0, "LTE band 71 (600) down", CellularDown),
    b(663.0, 698.0, "LTE band 71 (600) up", CellularUp),
    b(698.0, 716.0, "LTE band 12/17 up", CellularUp),
    b(728.0, 746.0, "LTE band 12/17/29 down", CellularDown),
    b(746.0, 756.0, "LTE band 13 down", CellularDown),
    b(777.0, 787.0, "LTE band 13 up", CellularUp),
    b(806.0, 824.0, "800 MHz SMR/public safety (mobile)", PublicSafetyTrunked),
    b(824.0, 849.0, "Cellular band 5 up", CellularUp),
    b(851.0, 869.0, "800 MHz SMR/public safety", PublicSafetyTrunked),
    b(869.0, 894.0, "Cellular band 5 down", CellularDown),
    b(896.0, 901.0, "900 MHz SMR (mobile)", LandMobile),
    b(902.0, 928.0, "ISM 915", Ism),
    b(935.0, 940.0, "900 MHz SMR", LandMobile),
    b(960.0, 1215.0, "Aero nav (DME/TACAN)", AviationNav),
    b(1215.0, 1240.0, "GNSS L2 / radar", Radar),
    b(1240.0, 1300.0, "Amateur 23 cm", Amateur),
    b(1525.0, 1559.0, "Mobile satellite (Inmarsat) down", Satellite),
    b(1559.0, 1610.0, "GNSS L1", Satellite),
    b(1610.0, 1626.5, "Mobile satellite (Iridium/Globalstar)", Satellite),
    b(1670.0, 1710.0, "Weather satellites (GOES/POES)", Satellite),
    b(1710.0, 1780.0, "AWS (band 4/66) up", CellularUp),
];

/// The allocation a frequency falls in, if listed.
pub fn lookup(hz: f64) -> Option<&'static Band> {
    BANDS.iter().find(|b| (b.lo_hz..b.hi_hz).contains(&hz))
}

/// US TV channel whose 6 MHz slot contains `hz`: (number, lower edge).
pub fn tv_channel(hz: f64) -> Option<(u16, f64)> {
    let mhz = hz / 1e6;
    let (first, base) = match mhz {
        m if (54.0..72.0).contains(&m) => (2, 54.0),
        m if (76.0..88.0).contains(&m) => (5, 76.0),
        m if (174.0..216.0).contains(&m) => (7, 174.0),
        m if (470.0..608.0).contains(&m) => (14, 470.0),
        _ => return None,
    };
    let k = ((mhz - base) / 6.0).floor();
    Some((first + k as u16, (base + k * 6.0) * 1e6))
}

/// US FM broadcast channels: 87.9 to 107.9 MHz in 200 kHz steps.
pub fn fm_channels(lo_hz: f64, hi_hz: f64) -> impl Iterator<Item = f64> {
    (0..101).map(|k| 87.9e6 + k as f64 * 200e3).filter(move |f| (lo_hz..hi_hz).contains(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups() {
        assert_eq!(lookup(121.5e6).unwrap().name, "Aviation emergency");
        assert_eq!(lookup(127.0e6).unwrap().service, Aviation);
        assert_eq!(lookup(98.1e6).unwrap().service, BroadcastFm);
        assert_eq!(tv_channel(57e6), Some((2, 54e6)));
        assert_eq!(tv_channel(569e6), Some((30, 566e6)));
        assert_eq!(tv_channel(213e6), Some((13, 210e6)));
        assert_eq!(fm_channels(102e6, 102.4e6).collect::<Vec<_>>(), vec![102.1e6, 102.3e6]);
    }
}
