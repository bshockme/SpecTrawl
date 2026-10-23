//! P25 Phase 1 link control: who a voice channel is carrying.
//!
//! The 72-bit link control word (opcode, manufacturer, talkgroup, source)
//! is protected by a Reed-Solomon (24,12,13) code over GF(64), 12 data
//! hexbits then 12 parity, the first hexbit the highest-degree coefficient
//! and the roots α¹..α¹². On the terminator (TDULC) each pair of hexbits is
//! a Golay (24,12,8) word: 12 data bits, 11 parity bits from the Golay
//! (23,12) generator 0xC75, then an even-parity bit. Both orientations were
//! confirmed on a real traffic channel (`examples/p25_lc_probe`).

use std::sync::OnceLock;

use crate::decode::Call;
use crate::frames::Frame;
use crate::p25::data_dibits;

// ------------------------------------------------------------- Golay ----

const GOLAY_POLY: u32 = 0xC75;

fn golay_parity(data: u32) -> u32 {
    let mut r = data << 11;
    for b in (11..23).rev() {
        if r >> b & 1 == 1 {
            r ^= GOLAY_POLY << (b - 11);
        }
    }
    r & 0x7FF
}

/// A Golay (24,12) codeword for 12 data bits.
pub fn golay_encode(data: u32) -> u32 {
    let cw = (data & 0xFFF) << 11 | golay_parity(data & 0xFFF);
    cw << 1 | (cw.count_ones() & 1)
}

/// The (23,12) code is perfect: every 11-bit syndrome is one error
/// pattern of weight ≤ 3.
fn golay_table() -> &'static [u32; 2048] {
    static T: OnceLock<[u32; 2048]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [u32::MAX; 2048];
        let synd = |e: u32| (golay_parity(e >> 11) ^ e) & 0x7FF;
        t[0] = 0;
        for i in 0..23 {
            for j in i..23 {
                for k in j..23 {
                    let e = 1u32 << i | 1 << j | 1 << k;
                    let s = synd(e) as usize;
                    if t[s] == u32::MAX || t[s].count_ones() > e.count_ones() {
                        t[s] = e;
                    }
                }
            }
        }
        t
    })
}

/// Corrects a received Golay (23,12) word (data in bits 22..11, parity in
/// 10..0): (12 data bits, bits fixed). Always succeeds (the code is
/// perfect); more than 3 errors decode to the wrong word.
pub fn golay23_decode(cw: u32) -> (u32, u32) {
    let cw = cw & 0x7F_FFFF;
    let e = golay_table()[((golay_parity(cw >> 11) ^ cw) & 0x7FF) as usize];
    ((cw ^ e) >> 11, e.count_ones())
}

/// Bit reliabilities of a dibit from its soft value (outer levels ±1, inner
/// ±1/3): the first bit is the sign, the second outer (1) or inner (0).
pub fn dibit_reliability(x: f32) -> [f32; 2] {
    [x.abs(), (x.abs() - 2.0 / 3.0).abs()]
}

/// Chase decoding: `rx` is a received word whose bit `i` has reliability
/// `rel[i]`; `decode` maps a hard word to the nearest codeword. Tries every
/// pattern of flips among the `flips` least reliable bits and returns the
/// codeword nearest `rx` in reliability-weighted distance.
pub fn chase(rx: u32, rel: &[f32], flips: usize, decode: impl Fn(u32) -> u32) -> u32 {
    let mut order: Vec<usize> = (0..rel.len()).collect();
    order.sort_by(|&a, &b| rel[a].total_cmp(&rel[b]));
    let weak = &order[..flips.min(order.len())];
    let cost = |cw: u32| -> f32 { (0..rel.len()).filter(|&i| (cw ^ rx) >> i & 1 == 1).map(|i| rel[i]).sum() };
    let mut best = (f32::MAX, rx);
    for pattern in 0u32..1 << weak.len() {
        let flip = weak.iter().enumerate().filter(|(k, _)| pattern >> k & 1 == 1).fold(0u32, |a, (_, &i)| a | 1 << i);
        let cw = decode(rx ^ flip);
        let c = cost(cw);
        if c < best.0 {
            best = (c, cw);
        }
    }
    best.1
}

/// The Golay (23,12) codeword nearest a received word.
pub fn golay23_nearest(w: u32) -> u32 {
    let (d, _) = golay23_decode(w);
    golay_encode(d) >> 1
}

/// Corrects a received Golay (24,12) word: (12 data bits, bits fixed), or
/// None when it's beyond repair (4 errors, caught by the parity bit).
pub fn golay_decode(w: u32) -> Option<(u32, u32)> {
    let cw = w >> 1 & 0x7F_FFFF;
    let s = (golay_parity(cw >> 11) ^ cw) & 0x7FF;
    let e = golay_table()[s as usize];
    let fixed = cw ^ e;
    // The overall parity: with ≤ 3 errors in the 23 bits, a mismatch means
    // the parity bit itself was hit (fine) unless the correction count
    // makes it inconsistent (4 errors).
    let parity_ok = (fixed.count_ones() + (w & 1)).is_multiple_of(2);
    if !parity_ok && e.count_ones() == 3 {
        return None;
    }
    Some((fixed >> 11, e.count_ones() + !parity_ok as u32))
}

// ------------------------------------------------- Reed-Solomon GF(64) ----

struct Gf {
    exp: [u8; 126],
    log: [usize; 64],
}

fn gf() -> &'static Gf {
    static G: OnceLock<Gf> = OnceLock::new();
    G.get_or_init(|| {
        let mut exp = [0u8; 126];
        let mut log = [0usize; 64];
        let mut v = 1u8;
        for (i, e) in exp.iter_mut().take(63).enumerate() {
            *e = v;
            log[v as usize] = i;
            v <<= 1;
            if v & 0x40 != 0 {
                v ^= 0x43; // x^6 + x + 1
            }
        }
        for i in 63..126 {
            exp[i] = exp[i - 63];
        }
        Gf { exp, log }
    })
}

fn mul(a: u8, b: u8) -> u8 {
    let g = gf();
    if a == 0 || b == 0 { 0 } else { g.exp[g.log[a as usize] + g.log[b as usize]] }
}

fn inv(a: u8) -> u8 {
    let g = gf();
    g.exp[(63 - g.log[a as usize]) % 63]
}

fn alpha(e: usize) -> u8 {
    gf().exp[e % 63]
}

/// Evaluates a polynomial given highest degree first.
fn eval_hi(p: &[u8], x: u8) -> u8 {
    p.iter().fold(0, |acc, &c| mul(acc, x) ^ c)
}

/// Evaluates a polynomial given lowest degree first.
fn eval_lo(p: &[u8], x: u8) -> u8 {
    p.iter().rev().fold(0, |acc, &c| mul(acc, x) ^ c)
}

const RS_N: usize = 24;
const RS_K: usize = 12;
const RS_T2: usize = RS_N - RS_K;

/// A Reed-Solomon (24,12) codeword for 12 data hexbits.
pub fn rs_encode(data: &[u8; RS_K]) -> [u8; RS_N] {
    // g(x) = ∏ (x - α^j), j = 1..12, lowest degree first.
    let mut g = vec![1u8];
    for j in 1..=RS_T2 {
        let mut n = vec![0u8; g.len() + 1];
        for (i, &c) in g.iter().enumerate() {
            n[i] ^= mul(c, alpha(j));
            n[i + 1] ^= c;
        }
        g = n;
    }
    // Remainder of data·x^12 by g, by long division (highest degree first).
    let mut r = [0u8; RS_N];
    r[..RS_K].copy_from_slice(data);
    for i in 0..RS_K {
        let c = r[i];
        if c != 0 {
            for (k, &gk) in g.iter().rev().enumerate().skip(1) {
                r[i + k] ^= mul(c, gk);
            }
        }
    }
    let mut out = [0u8; RS_N];
    out[..RS_K].copy_from_slice(data);
    out[RS_K..].copy_from_slice(&r[RS_K..]);
    out
}

/// Reed-Solomon over GF(64) with roots α¹..α^`nroots`, errors and
/// erasures: `cw` is a whole (possibly shortened) codeword, first symbol
/// the highest degree; `erasures` are indices into `cw` known to be
/// unreliable (punctured symbols). Corrects in place while 2·errors +
/// erasures ≤ `nroots`. Returns the symbols changed, or None.
pub fn rs_decode_erasures(cw: &mut [u8], nroots: usize, erasures: &[usize]) -> Option<usize> {
    let n = cw.len();
    let deg = |i: usize| n - 1 - i;
    let s: Vec<u8> = (1..=nroots).map(|j| eval_hi(cw, alpha(j))).collect();
    if s.iter().all(|&v| v == 0) {
        return Some(0);
    }
    let ne = erasures.len();
    if ne > nroots {
        return None;
    }
    // Erasure locator Γ(x) = ∏ (1 + X_k x), lowest degree first.
    let mut lam = vec![0u8; nroots + 1];
    lam[0] = 1;
    for &e in erasures {
        let x = alpha(deg(e));
        for i in (1..=nroots).rev() {
            lam[i] ^= mul(x, lam[i - 1]);
        }
    }
    // Berlekamp-Massey, started from the erasure locator.
    let mut b = lam.clone();
    let mut el = ne;
    for r in ne + 1..=nroots {
        let mut d = 0u8;
        for i in 0..r {
            d ^= mul(lam[i], s[r - 1 - i]);
        }
        if d == 0 {
            b.rotate_right(1);
            b[0] = 0;
            continue;
        }
        let mut t = lam.clone();
        for i in 1..=nroots {
            t[i] ^= mul(d, b[i - 1]);
        }
        if 2 * el < r + ne {
            el = r + ne - el;
            let di = inv(d);
            b = lam.iter().map(|&c| mul(c, di)).collect();
        } else {
            b.rotate_right(1);
            b[0] = 0;
        }
        lam = t;
    }
    let deg_lam = lam.iter().rposition(|&c| c != 0)?;
    if deg_lam == 0 || 2 * (deg_lam - ne.min(deg_lam)) + ne > nroots {
        return None;
    }
    lam.truncate(deg_lam + 1);
    // Ω = S·Λ mod x^nroots; Λ' keeps the odd terms.
    let mut om = vec![0u8; nroots];
    for (i, &si) in s.iter().enumerate() {
        for (j, &lj) in lam.iter().enumerate() {
            if i + j < nroots {
                om[i + j] ^= mul(si, lj);
            }
        }
    }
    let dlam: Vec<u8> = lam.iter().enumerate().skip(1).map(|(i, &c)| if i % 2 == 1 { c } else { 0 }).collect();
    let mut found = 0;
    for (i, c) in cw.iter_mut().enumerate() {
        let xinv = alpha(63 - deg(i) % 63);
        if eval_lo(&lam, xinv) == 0 {
            let den = eval_lo(&dlam, xinv);
            if den == 0 {
                return None;
            }
            *c ^= mul(eval_lo(&om, xinv), inv(den));
            found += 1;
        }
    }
    // Every root must land inside the (shortened) codeword.
    (found == deg_lam).then_some(found)
}

/// Corrects up to 6 hexbit errors in place (first hexbit = highest
/// degree). Returns the number corrected, or None if beyond repair.
pub fn rs_decode(h: &mut [u8; RS_N]) -> Option<usize> {
    let s: Vec<u8> = (1..=RS_T2).map(|j| eval_hi(h, alpha(j))).collect();
    if s.iter().all(|&v| v == 0) {
        return Some(0);
    }
    // Berlekamp-Massey: error locator Λ (lowest degree first).
    let mut lam = vec![1u8];
    let mut b = vec![1u8];
    let mut l = 0;
    let mut m = 1;
    let mut bd = 1u8;
    for n in 0..RS_T2 {
        let mut d = s[n];
        for i in 1..=l {
            d ^= mul(*lam.get(i).unwrap_or(&0), s[n - i]);
        }
        if d == 0 {
            m += 1;
            continue;
        }
        let coef = mul(d, inv(bd));
        let mut t = lam.clone();
        if t.len() < b.len() + m {
            t.resize(b.len() + m, 0);
        }
        for (i, &bi) in b.iter().enumerate() {
            t[i + m] ^= mul(coef, bi);
        }
        if 2 * l <= n {
            l = n + 1 - l;
            b = lam;
            bd = d;
            m = 1;
        } else {
            m += 1;
        }
        lam = t;
    }
    while lam.last() == Some(&0) {
        lam.pop();
    }
    if l == 0 || l > RS_T2 / 2 || lam.len() != l + 1 {
        return None;
    }
    // Ω = S·Λ mod x^12.
    let mut om = vec![0u8; RS_T2];
    for (i, &si) in s.iter().enumerate() {
        for (j, &lj) in lam.iter().enumerate() {
            if i + j < RS_T2 {
                om[i + j] ^= mul(si, lj);
            }
        }
    }
    // Λ' (formal derivative): odd terms only.
    let dlam: Vec<u8> = lam.iter().enumerate().skip(1).map(|(i, &c)| if i % 2 == 1 { c } else { 0 }).collect();
    // Chien search over the 24 positions; degree of position i is 23 - i.
    let mut found = 0;
    for (i, hi) in h.iter_mut().enumerate() {
        let deg = RS_N - 1 - i;
        let xinv = alpha(63 - deg % 63);
        if eval_lo(&lam, xinv) == 0 {
            // Forney with first root α¹: e = Ω(X⁻¹) / Λ'(X⁻¹).
            let den = eval_lo(&dlam, xinv);
            if den == 0 {
                return None;
            }
            *hi ^= mul(eval_lo(&om, xinv), inv(den));
            found += 1;
        }
    }
    (found == l).then_some(l)
}

// ------------------------------------------------------- link control ----

/// A decoded link control word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkControl {
    /// Link control opcode: 0x00 group voice, 0x03 unit-to-unit voice, ...
    pub lco: u8,
    /// The protected flag (encrypted LC: fields unreadable).
    pub protected: bool,
    pub mfid: u8,
    /// The 7 bytes after the opcode and manufacturer.
    pub args: [u8; 7],
    /// Hexbits Reed-Solomon corrected.
    pub corrected: usize,
}

impl LinkControl {
    fn from_bytes(b: &[u8; 9], corrected: usize) -> Self {
        LinkControl { lco: b[0] & 0x3F, protected: b[0] & 0x80 != 0, mfid: b[1], args: b[2..9].try_into().unwrap(), corrected }
    }

    /// The call, for standard (MFID 0 or 1) group and unit voice.
    pub fn call(&self) -> Option<Call> {
        if self.protected || self.mfid > 1 {
            return None;
        }
        let a = &self.args;
        let u24 = |i: usize| (a[i] as u32) << 16 | (a[i + 1] as u32) << 8 | a[i + 2] as u32;
        // Service options: bit 7 emergency, bit 6 encrypted.
        let (emergency, encrypted) = (a[0] & 0x80 != 0, a[0] & 0x40 != 0);
        match self.lco {
            0x00 => Some(Call {
                group: true,
                target: (a[2] as u32) << 8 | a[3] as u32,
                source: Some(u24(4)).filter(|&s| s != 0),
                encrypted,
                emergency,
            }),
            0x03 => Some(Call { group: false, target: u24(1), source: Some(u24(4)).filter(|&s| s != 0), encrypted, emergency }),
            _ => None,
        }
    }
}

/// Short name of a standard link control opcode (TIA-102.AABF).
pub fn lco_name(lco: u8) -> &'static str {
    match lco {
        0x00 => "group voice",
        0x02 => "group voice update",
        0x03 => "unit voice",
        0x04 => "unit answer request",
        0x05 => "telephone voice",
        0x0F => "call termination",
        0x20 => "system service",
        0x21 => "secondary control channel",
        0x22 => "adjacent site",
        0x23 => "RFSS status",
        0x24 => "network status",
        _ => "other",
    }
}

/// 12 data hexbits → the 9 link control bytes.
fn hexbits_to_bytes(h: &[u8]) -> [u8; 9] {
    let mut out = [0u8; 9];
    let bits: Vec<u8> = h.iter().take(RS_K).flat_map(|&x| (0..6).rev().map(move |i| x >> i & 1)).collect();
    for (o, c) in out.iter_mut().zip(bits.chunks(8)) {
        *o = c.iter().fold(0, |a, b| a << 1 | b);
    }
    out
}

// ----------------------------------------------------------- LDU1 ----

/// Hamming (10,6,3) on LDU1 link control hexbits: parity bit j (sent
/// after the 6 data bits, first to last) is the XOR of the data bits in
/// `HAMMING_MASKS[j]` (data MSB first). Learned from 230 real LDU1s
/// (`examples/p25_ldu_probe`) and confirmed by the Reed-Solomon checks.
const HAMMING_MASKS: [u16; 4] = [0b111001, 0b110101, 0b101110, 0b011110];

fn hamming10_parity(data: u16) -> u16 {
    HAMMING_MASKS.iter().fold(0, |a, m| a << 1 | ((data & m).count_ones() & 1) as u16)
}

/// Corrects a single bit error in a Hamming (10,6) word: the 6 data bits,
/// or None when the syndrome matches no single-bit error.
pub fn hamming10_decode(w: u16) -> Option<u8> {
    let s = hamming10_parity(w >> 4) ^ (w & 0xF);
    if s == 0 {
        return Some((w >> 4) as u8);
    }
    (0..10).map(|b| w ^ 1 << b).find(|&c| hamming10_parity(c >> 4) == c & 0xF).map(|c| (c >> 4) as u8)
}

/// The link control of an LDU1 frame (sliced with at least ~880 dibits
/// after the sync). Its 24 hexbits come in six 40-bit pieces, one after
/// each of voice frames 2-7.
pub fn ldu1(f: &Frame) -> Option<LinkControl> {
    let d = data_dibits(f, 784)?;
    let bits: Vec<u16> = d.iter().flat_map(|&v| [(v >> 1) as u16, (v & 1) as u16]).collect();
    let mut h = [0u8; RS_N];
    for k in 0..6 {
        let seg = &bits[288 + k * 184..288 + k * 184 + 40];
        for (j, c) in seg.chunks(10).enumerate() {
            // A Hamming failure leaves the hexbit for Reed-Solomon to fix.
            h[4 * k + j] = hamming10_decode(c.iter().fold(0, |a, b| a << 1 | b)).unwrap_or(0);
        }
    }
    let corrected = rs_decode(&mut h)?;
    Some(LinkControl::from_bytes(&hexbits_to_bytes(&h), corrected))
}

/// The encryption sync of an LDU2: the message indicator (the cipher's IV),
/// algorithm and key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncryptionSync {
    pub mi: [u8; 9],
    pub algid: u8,
    pub kid: u16,
    /// Hexbits Reed-Solomon corrected.
    pub corrected: usize,
}

impl EncryptionSync {
    /// ALGID 0x80 is "unencrypted".
    pub fn encrypted(&self) -> bool {
        self.algid != 0x80
    }

    pub fn algorithm(&self) -> &'static str {
        algorithm_name(self.algid)
    }
}

/// A P25 encryption algorithm ID's name.
pub fn algorithm_name(algid: u8) -> &'static str {
    match algid {
        0x80 => "clear",
        0x81 => "DES-OFB",
        0x83 => "3DES",
        0x84 => "AES-256",
        0x85 => "AES-128",
        0x9F => "DES-XL",
        0xA0 => "DVI-XL",
        0xA1 => "DVP-XL",
        0xAA => "ADP",
        _ => "other",
    }
}

/// The encryption sync of an LDU2 frame (sliced as for `ldu1`): 16
/// hexbits of MI, ALGID and KID plus 8 Reed-Solomon (24,16,9) parity, in
/// the same six Hamming-coded pieces as LDU1's link control.
pub fn ldu2(f: &Frame) -> Option<EncryptionSync> {
    let d = data_dibits(f, 784)?;
    let bits: Vec<u16> = d.iter().flat_map(|&v| [(v >> 1) as u16, (v & 1) as u16]).collect();
    let mut h = [0u8; RS_N];
    for k in 0..6 {
        let seg = &bits[288 + k * 184..288 + k * 184 + 40];
        for (j, c) in seg.chunks(10).enumerate() {
            h[4 * k + j] = hamming10_decode(c.iter().fold(0, |a, b| a << 1 | b)).unwrap_or(0);
        }
    }
    let corrected = rs_decode_erasures(&mut h, 8, &[])?;
    // 16 hexbits = 96 bits: MI (72), ALGID (8), KID (16).
    let v = h[..16].iter().fold(0u128, |a, &x| a << 6 | x as u128);
    let mut mi = [0u8; 9];
    for (i, b) in mi.iter_mut().enumerate() {
        *b = (v >> (88 - 8 * i)) as u8;
    }
    Some(EncryptionSync { mi, algid: (v >> 16) as u8, kid: v as u16, corrected })
}

/// The link control of a TDULC frame (sliced with at least ~190 dibits
/// after the sync).
pub fn tdulc(f: &Frame) -> Option<LinkControl> {
    let d = data_dibits(f, 144)?;
    let bits: Vec<u32> = d.iter().flat_map(|&v| [(v >> 1) as u32, (v & 1) as u32]).collect();
    let mut h = [0u8; RS_N];
    for (k, c) in bits.chunks(24).enumerate() {
        let (data, _) = golay_decode(c.iter().fold(0, |a, b| a << 1 | b))?;
        h[2 * k] = (data >> 6) as u8;
        h[2 * k + 1] = (data & 0x3F) as u8;
    }
    let corrected = rs_decode(&mut h)?;
    Some(LinkControl::from_bytes(&hexbits_to_bytes(&h), corrected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golay_corrects_three() {
        for data in [0u32, 0xFFF, 0xA5C, 0x123] {
            let cw = golay_encode(data);
            assert_eq!(golay_decode(cw), Some((data, 0)));
            assert_eq!(golay_decode(cw ^ 1 << 5 ^ 1 << 17 ^ 1 << 23).map(|r| r.0), Some(data));
            assert_eq!(golay_decode(cw ^ 1).map(|r| r.0), Some(data));
        }
    }

    #[test]
    fn rs_corrects_six_hexbits() {
        let data: [u8; 12] = [0x18, 0x20, 0x00, 0x32, 0x29, 0x10, 0x08, 0x16, 0x05, 0x13, 0x21, 0x30];
        let cw = rs_encode(&data);
        // The parity matches a real TDULC.
        assert_eq!(&cw[12..], &[0x39, 0x23, 0x21, 0x02, 0x22, 0x37, 0x2A, 0x01, 0x11, 0x3C, 0x07, 0x0E]);
        let mut r = cw;
        for (i, e) in [(0, 0x3F), (3, 0x01), (9, 0x22), (14, 0x10), (20, 0x05), (23, 0x2A)] {
            r[i] ^= e;
        }
        assert_eq!(rs_decode(&mut r), Some(6));
        assert_eq!(r, cw);
        let mut clean = cw;
        assert_eq!(rs_decode(&mut clean), Some(0));
    }

    #[test]
    fn rs_errors_and_erasures() {
        // RS(24,12) as a shortened (63,51) code: decode the same codeword
        // through the general decoder, with 4 erasures and 4 errors.
        let data: [u8; 12] = [0x18, 0x20, 0x00, 0x32, 0x29, 0x10, 0x08, 0x16, 0x05, 0x13, 0x21, 0x30];
        let cw = rs_encode(&data);
        let mut r = cw.to_vec();
        for i in [20, 21, 22, 23] {
            r[i] = 0; // punctured: unknown
        }
        for (i, e) in [(1, 0x11), (5, 0x3F), (12, 0x02), (17, 0x20)] {
            r[i] ^= e;
        }
        assert!(rs_decode_erasures(&mut r, 12, &[20, 21, 22, 23]).is_some());
        assert_eq!(r, cw.to_vec());
    }

    #[test]
    fn hamming10_corrects_one() {
        for d in 0..64u16 {
            let w = d << 4 | hamming10_parity(d);
            assert_eq!(hamming10_decode(w), Some(d as u8));
            for b in 0..10 {
                assert_eq!(hamming10_decode(w ^ 1 << b), Some(d as u8), "data {d} bit {b}");
            }
        }
    }

    #[test]
    fn group_voice_fields() {
        // LCO 0, MFID 0, service options 0x40 (encrypted), TG 0x1234, source 0x00ABCD.
        let lc = LinkControl::from_bytes(&[0x00, 0x00, 0x40, 0x00, 0x12, 0x34, 0x00, 0xAB, 0xCD], 0);
        let c = lc.call().unwrap();
        assert_eq!(c, Call { group: true, target: 0x1234, source: Some(0xABCD), emergency: false, encrypted: true });
        assert_eq!(c.summary(), "TG 4660 ← 43981 (enc)");
    }
}
