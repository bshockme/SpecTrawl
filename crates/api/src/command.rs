//! Commands from an interface to the engine, with sequence-numbered acknowledgements.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use sdr_core::GainSetting;

use crate::audio::{AudioMode, PlayId, PlayTarget, PlayTo};
use crate::classify::WorkerId;
use crate::records::{RecordId, RecordQuery};
use crate::{Mode, SourceId};

#[derive(Debug, Clone)]
pub enum Command {
    Enumerate,
    Open {
        backend: String,
        serial: String,
    },
    Close(SourceId),
    Tune {
        source: SourceId,
        center_hz: f64,
    },
    SetGain {
        source: SourceId,
        gain: GainSetting,
    },
    SetMode {
        source: SourceId,
        mode: Mode,
    },
    SetBudget {
        guaranteed_secs: f64,
        ceiling_bytes: u64,
    },
    /// What the receiver sweeps in Sweep mode: `start_hz..stop_hz`, measuring only `segments` when there are any.
    /// Takes effect at the end of the pass in progress.
    SetSweep {
        source: SourceId,
        start_hz: f64,
        stop_hz: f64,
        segments: Vec<(f64, f64)>,
    },
    Tag {
        record: RecordId,
        tag: String,
    },
    Untag {
        record: RecordId,
        tag: String,
    },
    Note {
        record: RecordId,
        text: String,
    },
    /// Answered as [`Snapshot::query`](crate::Snapshot::query) with this command's sequence number.
    QueryRecords(RecordQuery),
    /// Replay a record or a region (spec §4.7). `mode: None` lets the engine choose from the frequency; `gain` scales
    /// the sound (1 = as demodulated). Acknowledged with [`Outcome::Playing`], or an error saying why it can't be heard.
    Play {
        target: PlayTarget,
        mode: Option<AudioMode>,
        gain: f32,
        to: PlayTo,
    },
    StopPlay(PlayId),
    /// Keep a record's IQ: its slice is copied out of the capped slice directory (into `saved/`, beside it), now if it
    /// is written, else as soon as it is. The record is tagged `saved`. Acknowledged with [`Outcome::SavedIq`], or an
    /// error saying why there is no IQ to keep.
    SaveIq {
        record: RecordId,
    },
    /// Run a decoder worker (`docs/decoder-worker.md`) in `mode` (an id from [`Snapshot::worker_modes`]) on a channel
    /// of `source` centred at `center_hz`. Acknowledged with [`Outcome::WorkerStarted`].
    ///
    /// [`Snapshot::worker_modes`]: crate::Snapshot::worker_modes
    StartWorker {
        source: SourceId,
        mode: String,
        center_hz: f64,
    },
    StopWorker(WorkerId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done,
    Opened(SourceId),
    /// The query is running; its answer appears in a later snapshot.
    Queried,
    /// The replay is under way; it shows in [`Snapshot::audio`](crate::Snapshot::audio).
    Playing(PlayId),
    /// The worker is starting; it shows in [`Snapshot::workers`](crate::Snapshot::workers).
    WorkerStarted(WorkerId),
    /// The record's IQ is kept at `path` (a SigMF stem); `pending` while its slice is still being written.
    SavedIq {
        record: RecordId,
        path: std::path::PathBuf,
        pending: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ack {
    pub seq: u64,
    pub result: Result<Outcome, String>,
}

#[derive(Debug, Clone)]
pub struct CommandSender {
    tx: mpsc::Sender<(u64, Command)>,
    seq: Arc<AtomicU64>,
}

impl CommandSender {
    /// Queue `c`; returns its sequence number, counting from 1.
    pub fn send(&self, c: Command) -> u64 {
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        // A gone receiver means the engine has stopped; the command is moot.
        let _ = self.tx.send((n, c));
        n
    }
}

#[derive(Debug)]
pub struct CommandReceiver {
    rx: mpsc::Receiver<(u64, Command)>,
    /// Commands that arrived during [`CommandReceiver::wait`], handed out by the next drain.
    pending: Vec<(u64, Command)>,
}

impl CommandReceiver {
    /// Everything queued (possibly nothing); `None` once every sender is gone and the queue is empty.
    pub fn drain(&mut self) -> Option<Vec<(u64, Command)>> {
        let mut out = std::mem::take(&mut self.pending);
        loop {
            match self.rx.try_recv() {
                Ok(item) => out.push(item),
                Err(TryRecvError::Empty) => return Some(out),
                Err(TryRecvError::Disconnected) => return if out.is_empty() { None } else { Some(out) },
            }
        }
    }

    /// Blocks until a command arrives (kept for the next [`CommandReceiver::drain`]), `timeout` passes, or the last
    /// sender is dropped; returns `false` only in the last case, as soon as it happens.
    pub fn wait(&mut self, timeout: Duration) -> bool {
        match self.rx.recv_timeout(timeout) {
            Ok(item) => {
                self.pending.push(item);
                true
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => false,
        }
    }
}

pub fn command_channel() -> (CommandSender, CommandReceiver) {
    let (tx, rx) = mpsc::channel();
    (CommandSender { tx, seq: Arc::new(AtomicU64::new(0)) }, CommandReceiver { rx, pending: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_numbers_count_from_one_and_drain_reports_disconnection() {
        let (tx, mut rx) = command_channel();
        assert_eq!(tx.send(Command::Enumerate), 1);
        assert_eq!(tx.send(Command::Enumerate), 2);
        assert_eq!(rx.drain().unwrap().len(), 2);
        assert_eq!(rx.drain().unwrap().len(), 0);
        drop(tx);
        assert!(rx.drain().is_none());
    }

    #[test]
    fn wait_keeps_what_arrives_and_reports_disconnection() {
        let (tx, mut rx) = command_channel();
        assert!(rx.wait(Duration::ZERO), "a timeout is not a disconnection");
        tx.send(Command::Enumerate);
        assert!(rx.wait(Duration::from_secs(1)));
        tx.send(Command::Enumerate);
        drop(tx);
        // Nothing queued is lost: the waited-for command comes first, then the rest, then the end.
        assert_eq!(rx.drain().unwrap().iter().map(|(n, _)| *n).collect::<Vec<_>>(), [1, 2]);
        assert!(!rx.wait(Duration::from_secs(5)), "a disconnection returns at once");
        assert!(rx.drain().is_none());
    }
}
