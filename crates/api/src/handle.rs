//! The interface's grip on the engine: the latest snapshot and the command sender.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::command::{Command, CommandSender};
use crate::snapshot::Snapshot;

#[derive(Debug, Clone)]
pub struct Handle {
    snapshot: Arc<ArcSwap<Snapshot>>,
    commands: CommandSender,
}

impl Handle {
    pub fn new(snapshot: Arc<ArcSwap<Snapshot>>, commands: CommandSender) -> Handle {
        Handle { snapshot, commands }
    }

    /// The most recently published snapshot.
    pub fn latest(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    /// Queue a command; returns its sequence number.
    pub fn send(&self, c: Command) -> u64 {
        self.commands.send(c)
    }
}
