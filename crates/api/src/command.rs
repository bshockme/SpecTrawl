//! Commands from an interface to the engine, with sequence-numbered acknowledgements.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, TryRecvError};

use sdr_core::GainSetting;

use crate::{Mode, SourceId};

#[derive(Debug, Clone)]
pub enum Command {
    Enumerate,
    Open { backend: String, serial: String },
    Close(SourceId),
    Tune { source: SourceId, center_hz: f64 },
    SetGain { source: SourceId, gain: GainSetting },
    SetMode { source: SourceId, mode: Mode },
    SetBudget { guaranteed_secs: f64, ceiling_bytes: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Done,
    Opened(SourceId),
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
}

impl CommandReceiver {
    /// Everything queued (possibly nothing); `None` once every sender is gone and the queue is empty.
    pub fn drain(&self) -> Option<Vec<(u64, Command)>> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(item) => out.push(item),
                Err(TryRecvError::Empty) => return Some(out),
                Err(TryRecvError::Disconnected) => return if out.is_empty() { None } else { Some(out) },
            }
        }
    }
}

pub fn command_channel() -> (CommandSender, CommandReceiver) {
    let (tx, rx) = mpsc::channel();
    (CommandSender { tx, seq: Arc::new(AtomicU64::new(0)) }, CommandReceiver { rx })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_numbers_count_from_one_and_drain_reports_disconnection() {
        let (tx, rx) = command_channel();
        assert_eq!(tx.send(Command::Enumerate), 1);
        assert_eq!(tx.send(Command::Enumerate), 2);
        assert_eq!(rx.drain().unwrap().len(), 2);
        assert_eq!(rx.drain().unwrap().len(), 0);
        drop(tx);
        assert!(rx.drain().is_none());
    }
}
