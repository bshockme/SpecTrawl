//! Identity of a trunked radio system, whatever its protocol: the key that
//! calls, grants, imported names and the UI's selection resolve against.
//! Only P25 is decoded to a system today; the other kinds are here so the
//! engine and UI never have to change shape when their decoders arrive.

/// Trunking protocol family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SystemKind {
    P25,
    DmrCapPlus,
    DmrConPlus,
    DmrTier3,
    Nxdn,
    Ltr,
    Edacs,
    Mpt1327,
}

impl SystemKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::P25 => "P25",
            Self::DmrCapPlus => "DMR Cap+",
            Self::DmrConPlus => "DMR Con+",
            Self::DmrTier3 => "DMR Tier III",
            Self::Nxdn => "NXDN",
            Self::Ltr => "LTR",
            Self::Edacs => "EDACS",
            Self::Mpt1327 => "MPT1327",
        }
    }
}

/// Stable identity of one system. Fields learned later (a P25 WACN arrives
/// after the first grants) are optional, so two keys for the same system
/// are compared with [`SystemKey::same`], not `==`.
#[derive(Debug, Clone, PartialEq)]
pub enum SystemKey {
    P25 { wacn: Option<u32>, system: Option<u16>, nac: u16 },
    /// Capacity Plus, Connect Plus or Tier III: the network id when the
    /// signalling carries one, else the colour code and the control (rest)
    /// channel it was seen on.
    Dmr { kind: SystemKind, network: Option<u32>, colour_code: u8, control_hz: f64 },
    Nxdn { ran: u8, control_hz: f64 },
    Other { kind: SystemKind, key: String },
}

impl SystemKey {
    pub fn kind(&self) -> SystemKind {
        match self {
            Self::P25 { .. } => SystemKind::P25,
            Self::Dmr { kind, .. } => *kind,
            Self::Nxdn { .. } => SystemKind::Nxdn,
            Self::Other { kind, .. } => *kind,
        }
    }

    /// The same system, allowing for fields one side hasn't learned yet.
    pub fn same(&self, other: &SystemKey) -> bool {
        match (self, other) {
            (Self::P25 { wacn: w1, system: s1, nac: n1 }, Self::P25 { wacn: w2, system: s2, nac: n2 }) => {
                let known = |a: &Option<u32>, b: &Option<u32>| a.is_none() || b.is_none() || a == b;
                let known16 = |a: &Option<u16>, b: &Option<u16>| a.is_none() || b.is_none() || a == b;
                n1 == n2 && known(w1, w2) && known16(s1, s2)
            }
            (Self::Dmr { kind: k1, network: n1, colour_code: c1, control_hz: f1 }, Self::Dmr { kind: k2, network: n2, colour_code: c2, control_hz: f2 }) => {
                k1 == k2
                    && match (n1, n2) {
                        (Some(a), Some(b)) => a == b,
                        _ => c1 == c2 && (f1 - f2).abs() < 1_000.0,
                    }
            }
            (Self::Nxdn { ran: r1, control_hz: f1 }, Self::Nxdn { ran: r2, control_hz: f2 }) => r1 == r2 && (f1 - f2).abs() < 1_000.0,
            (Self::Other { kind: k1, key: a }, Self::Other { kind: k2, key: b }) => k1 == k2 && a == b,
            _ => false,
        }
    }

    /// Short identity for labels: "NAC 0x2A5", "Cap+ CC 3 @ 855.5875".
    pub fn short(&self) -> String {
        match self {
            Self::P25 { nac, .. } => format!("NAC 0x{nac:03X}"),
            Self::Dmr { kind, network: Some(n), .. } => format!("{} net {n}", kind.name()),
            Self::Dmr { kind, colour_code, control_hz, .. } => format!("{} CC {colour_code} @ {:.4}", kind.name(), control_hz / 1e6),
            Self::Nxdn { ran, control_hz } => format!("NXDN RAN {ran} @ {:.4}", control_hz / 1e6),
            Self::Other { kind, key } => format!("{} {key}", kind.name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p25_same_tolerates_unlearned_fields() {
        let early = SystemKey::P25 { wacn: None, system: None, nac: 0x2A5 };
        let full = SystemKey::P25 { wacn: Some(0xBEE00), system: Some(0x2A5), nac: 0x2A5 };
        let other = SystemKey::P25 { wacn: Some(0xBEE00), system: Some(0x2A6), nac: 0x2A5 };
        assert!(early.same(&full));
        assert!(full.same(&early));
        assert!(!full.same(&other));
        assert!(!early.same(&SystemKey::P25 { wacn: None, system: None, nac: 0x2A6 }));
    }

    #[test]
    fn dmr_same_prefers_network_id() {
        let a = SystemKey::Dmr { kind: SystemKind::DmrCapPlus, network: Some(7), colour_code: 1, control_hz: 855.5875e6 };
        let b = SystemKey::Dmr { kind: SystemKind::DmrCapPlus, network: Some(7), colour_code: 3, control_hz: 856.0e6 };
        let c = SystemKey::Dmr { kind: SystemKind::DmrCapPlus, network: None, colour_code: 1, control_hz: 855.5880e6 };
        let d = SystemKey::Dmr { kind: SystemKind::DmrCapPlus, network: None, colour_code: 1, control_hz: 855.5875e6 };
        assert!(a.same(&b));
        assert!(c.same(&d));
        assert!(!a.same(&SystemKey::Dmr { kind: SystemKind::DmrConPlus, network: Some(7), colour_code: 1, control_hz: 855.5875e6 }));
    }
}
