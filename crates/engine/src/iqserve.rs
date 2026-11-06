//! One bus channel served as an rtl_tcp stream on loopback: SpecTrawl's side of the decoder-worker protocol
//! (`docs/decoder-worker.md`, spec §4.6). A worker (OnAir's `onair-server` with `net_radio=rtltcp,127.0.0.1,<port>`, or
//! anything that speaks rtl_tcp) connects, reads a 12-byte greeting, sends 5-byte commands and receives 8-bit offset IQ.
//!
//! - The greeting is `SPTW`, tuner type 0, no gains: OnAir then takes the server as "another radio", which may run at
//!   any rate (an `RTL0` greeting with tuner 1–6 holds it to 2.56 MS/s).
//! - The channel is a bus subscription at the server's priority, centred where the client tunes (inside the source's
//!   usable span), at a rate the channelizer gives exactly (`fs·K/N`). A rate command for another rate is counted and
//!   the served rate kept: the worker is configured with the served rate, so it never asks for another.
//! - A slow AGC holds the samples' RMS at [`TARGET_RMS`] counts (18 dB under clipping), like a dongle's IF gain; the
//!   client's gain commands are counted and ignored.
//! - A bounded queue ([`QUEUE_SECS`]) sits between the bus and the socket: a slow client loses whole blocks, counted,
//!   and never holds a bus worker.
//!
//! One client at a time; a new one is served after the last has gone. The server's threads are joined when it drops.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use sdr_core::Complex32;

use crate::channels::{ChannelBus, ChannelId, ChannelSpec, Consumer, Delivery, Flow, Priority, Start};
use crate::source::Source;

/// The greeting's four characters.
pub const MAGIC: &[u8; 4] = b"SPTW";
/// The samples' RMS the AGC holds, in counts of the 8-bit scale (±127.5).
pub const TARGET_RMS: f32 = 16.0;
/// Most IQ waiting for a slow client, seconds.
pub const QUEUE_SECS: f64 = 0.5;
/// The AGC's time constant, seconds.
const AGC_SECS: f64 = 0.5;
/// The accept loop's poll.
const POLL: Duration = Duration::from_millis(50);

/// rtl_tcp's commands (`rtl_tcp.c`).
const CMD_FREQ: u8 = 0x01;
const CMD_RATE: u8 = 0x02;
const CMD_GAIN: u8 = 0x04;

#[derive(Debug, Clone)]
pub struct IqServeConfig {
    /// Where the channel starts, before a client tunes it.
    pub center_hz: f64,
    /// The served rate; it must be one the channelizer gives exactly (`fs·K/N` for a power of two `K ≤ N/4`).
    pub rate: f64,
    pub priority: Priority,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IqServeStats {
    pub clients: u64,
    pub connected: bool,
    pub center_hz: f64,
    pub rate: f64,
    pub bytes_sent: u64,
    /// Blocks a slow client lost.
    pub dropped_blocks: u64,
    pub tunes: u64,
    /// Tunes outside the source's usable span, refused.
    pub refused_tunes: u64,
    /// Rate commands for another rate than the served one.
    pub refused_rates: u64,
    pub gain_commands: u64,
    /// Gaps in the channel (the bus's), and the subscription ending.
    pub gaps: u64,
}

struct Shared {
    stop: AtomicBool,
    stats: Mutex<IqServeStats>,
    /// Where the client wants the channel (f64 bits), and whether that changed.
    want_hz: AtomicU64,
    retune: AtomicBool,
}

pub struct IqServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl IqServer {
    /// Listens on `127.0.0.1` (a free port) and serves `source` through `bus`. Fails if the rate is not one the
    /// channelizer gives exactly at the source's rate, or the starting centre is outside its usable span.
    pub fn start(bus: Arc<ChannelBus>, source: Arc<Source>, cfg: IqServeConfig) -> Result<IqServer, String> {
        let probe =
            ChannelSpec { source: source.id(), center_hz: cfg.center_hz, bandwidth_hz: cfg.rate / 1.6, min_rate: cfg.rate, parent: None };
        let info = bus.subscribe(probe, Start::Live, cfg.priority, Box::new(|_: Delivery<'_>| Flow::Stop)).map_err(|e| format!("{e:?}"))?;
        bus.unsubscribe(info.id);
        if (info.sample_rate - cfg.rate).abs() > 1e-6 * cfg.rate {
            return Err(format!("the channelizer gives {:.0} S/s for {:.0} S/s asked: serve a rate fs·K/N", info.sample_rate, cfg.rate));
        }
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            stats: Mutex::new(IqServeStats { center_hz: cfg.center_hz, rate: cfg.rate, ..Default::default() }),
            want_hz: AtomicU64::new(cfg.center_hz.to_bits()),
            retune: AtomicBool::new(false),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name(format!("iqserve-{}", addr.port()))
            .spawn(move || accept(listener, s, bus, source, cfg))
            .map_err(|e| e.to_string())?;
        Ok(IqServer { addr, shared, thread: Some(thread) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stats(&self) -> IqServeStats {
        self.shared.stats.lock().clone()
    }
}

impl Drop for IqServer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn accept(listener: TcpListener, shared: Arc<Shared>, bus: Arc<ChannelBus>, source: Arc<Source>, cfg: IqServeConfig) {
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((sock, peer)) => {
                log::info!("iqserve {}: {peer} connected", listener.local_addr().map_or(0, |a| a.port()));
                if let Err(e) = serve(sock, &shared, &bus, &source, &cfg) {
                    log::info!("iqserve: {peer} gone: {e}");
                }
                shared.stats.lock().connected = false;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
            Err(e) => {
                log::warn!("iqserve: accept: {e}");
                std::thread::sleep(POLL);
            }
        }
    }
}

/// One client, until it goes or the server stops.
fn serve(sock: TcpStream, shared: &Arc<Shared>, bus: &ChannelBus, source: &Arc<Source>, cfg: &IqServeConfig) -> io::Result<()> {
    sock.set_nonblocking(false)?;
    sock.set_nodelay(true)?;
    sock.set_write_timeout(Some(Duration::from_millis(500)))?;
    let mut out = sock.try_clone()?;
    let mut greeting = [0u8; 12];
    greeting[..4].copy_from_slice(MAGIC);
    out.write_all(&greeting)?;
    {
        let mut s = shared.stats.lock();
        s.clients += 1;
        s.connected = true;
    }
    // Commands, on their own thread: the writer must not wait on them.
    let alive = Arc::new(AtomicBool::new(true));
    let reader = {
        let (shared, alive, rate) = (shared.clone(), alive.clone(), cfg.rate);
        let mut sock = sock.try_clone()?;
        sock.set_read_timeout(Some(POLL))?;
        std::thread::Builder::new().name("iqserve-cmd".into()).spawn(move || commands(&mut sock, &shared, &alive, rate))?
    };
    let result = stream(&mut out, shared, bus, source, cfg, &alive);
    alive.store(false, Ordering::Relaxed);
    let _ = sock.shutdown(std::net::Shutdown::Both);
    let _ = reader.join();
    result
}

/// Reads 5-byte commands until the client goes.
fn commands(sock: &mut TcpStream, shared: &Shared, alive: &AtomicBool, rate: f64) {
    let mut cmd = [0u8; 5];
    let mut have = 0;
    while alive.load(Ordering::Relaxed) && !shared.stop.load(Ordering::Relaxed) {
        match sock.read(&mut cmd[have..]) {
            Ok(0) => break,
            Ok(n) => {
                have += n;
                if have < 5 {
                    continue;
                }
                have = 0;
                let arg = u32::from_be_bytes([cmd[1], cmd[2], cmd[3], cmd[4]]);
                let mut s = shared.stats.lock();
                match cmd[0] {
                    CMD_FREQ => {
                        shared.want_hz.store(f64::from(arg).to_bits(), Ordering::Relaxed);
                        shared.retune.store(true, Ordering::Relaxed);
                    }
                    CMD_RATE if (f64::from(arg) - rate).abs() > 0.5 => {
                        s.refused_rates += 1;
                        log::warn!("iqserve: the client asked for {arg} S/s; serving {rate:.0}");
                    }
                    CMD_GAIN => s.gain_commands += 1,
                    _ => {}
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    alive.store(false, Ordering::Relaxed);
}

/// Subscribes the channel and writes its blocks to the client, retuning when the client asks.
fn stream(
    out: &mut TcpStream,
    shared: &Arc<Shared>,
    bus: &ChannelBus,
    source: &Arc<Source>,
    cfg: &IqServeConfig,
    alive: &AtomicBool,
) -> io::Result<()> {
    let blocks = ((QUEUE_SECS * cfg.rate) as usize / 4096).max(4);
    let mut current: Option<(ChannelId, Receiver<Vec<u8>>)> = None;
    let mut center = f64::from_bits(shared.want_hz.load(Ordering::Relaxed));
    let result = loop {
        if !alive.load(Ordering::Relaxed) || shared.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        if current.is_none() || shared.retune.swap(false, Ordering::Relaxed) {
            let want = f64::from_bits(shared.want_hz.load(Ordering::Relaxed));
            match subscribe(bus, source, cfg, want, blocks, shared) {
                Ok(sub) => {
                    if let Some((id, _)) = current.replace(sub) {
                        bus.unsubscribe(id);
                    }
                    center = want;
                    let mut s = shared.stats.lock();
                    s.center_hz = center;
                    s.tunes += 1;
                }
                Err(why) => {
                    shared.stats.lock().refused_tunes += 1;
                    log::warn!("iqserve: cannot tune to {:.4} MHz: {why}; staying at {:.4} MHz", want / 1e6, center / 1e6);
                    if current.is_none() {
                        break Err(io::Error::other(why));
                    }
                }
            }
        }
        let Some((_, rx)) = &current else { continue };
        match rx.recv_timeout(POLL) {
            Ok(bytes) => {
                if let Err(e) = out.write_all(&bytes) {
                    break Err(e);
                }
                shared.stats.lock().bytes_sent += bytes.len() as u64;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Err(io::Error::other("the channel ended")),
        }
    };
    if let Some((id, _)) = current {
        bus.unsubscribe(id);
    }
    result
}

fn subscribe(
    bus: &ChannelBus,
    source: &Arc<Source>,
    cfg: &IqServeConfig,
    center_hz: f64,
    blocks: usize,
    shared: &Arc<Shared>,
) -> Result<(ChannelId, Receiver<Vec<u8>>), String> {
    let (tx, rx) = sync_channel(blocks);
    let feed = Feed { tx, shared: shared.clone(), ema: None, alpha: 0.0, rate: cfg.rate };
    let spec = ChannelSpec { source: source.id(), center_hz, bandwidth_hz: cfg.rate / 1.6, min_rate: cfg.rate, parent: None };
    let info = bus.subscribe(spec, Start::Live, cfg.priority, Box::new(feed)).map_err(|e| format!("{e:?}"))?;
    if (info.sample_rate - cfg.rate).abs() > 1e-6 * cfg.rate {
        bus.unsubscribe(info.id);
        return Err(format!("the channel came at {:.0} S/s", info.sample_rate));
    }
    Ok((info.id, rx))
}

/// The bus consumer: samples to 8-bit offset IQ under the AGC, into the queue.
struct Feed {
    tx: SyncSender<Vec<u8>>,
    shared: Arc<Shared>,
    /// The mean |x|² the AGC tracks.
    ema: Option<f32>,
    alpha: f32,
    rate: f64,
}

impl Consumer for Feed {
    fn deliver(&mut self, d: Delivery<'_>) -> Flow {
        match d {
            Delivery::Samples { samples, .. } => {
                if samples.is_empty() {
                    return Flow::Continue;
                }
                let power = samples.iter().map(|v| v.norm_sqr()).sum::<f32>() / samples.len() as f32;
                if self.alpha == 0.0 {
                    self.alpha = (samples.len() as f64 / (AGC_SECS * self.rate)).min(1.0) as f32;
                }
                let ema = self.ema.map_or(power, |e| e + self.alpha * (power - e)).max(1e-20);
                self.ema = Some(ema);
                // RMS per component is sqrt(power / 2).
                let k = TARGET_RMS / (ema / 2.0).sqrt();
                let bytes = to_u8(samples, k);
                match self.tx.try_send(bytes) {
                    Ok(()) => Flow::Continue,
                    Err(TrySendError::Full(_)) => {
                        self.shared.stats.lock().dropped_blocks += 1;
                        Flow::Continue
                    }
                    Err(TrySendError::Disconnected(_)) => Flow::Stop,
                }
            }
            Delivery::Gap { .. } => {
                self.shared.stats.lock().gaps += 1;
                Flow::Continue
            }
            _ => Flow::Stop,
        }
    }
}

/// `x·k` as rtl_tcp's interleaved offset bytes (127.5 is zero), clipped.
pub fn to_u8(x: &[Complex32], k: f32) -> Vec<u8> {
    let q = |v: f32| (v * k + 127.5).round().clamp(0.0, 255.0) as u8;
    x.iter().flat_map(|v| [q(v.re), q(v.im)]).collect()
}

/// rtl_tcp's bytes back to samples (for clients and tests): `(b − 127.5) / 127.5`.
pub fn from_u8(b: &[u8]) -> Vec<Complex32> {
    b.as_chunks::<2>().0.iter().map(|p| Complex32::new((f32::from(p[0]) - 127.5) / 127.5, (f32::from(p[1]) - 127.5) / 127.5)).collect()
}
