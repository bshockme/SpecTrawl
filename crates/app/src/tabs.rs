//! What the dock holds.

use api::SourceId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tab {
    /// One open device: its header, status and (from Task 6) waterfall.
    Receiver(SourceId),
    Devices,
    Log,
}

impl Tab {
    /// The tab's name for the `View ▾` menu and the tab strip (receivers are titled by the app).
    pub fn name(&self) -> &'static str {
        match self {
            Tab::Receiver(_) => "Receiver",
            Tab::Devices => "Devices",
            Tab::Log => "Log",
        }
    }
}
