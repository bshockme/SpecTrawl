//! What the user selected: one value every pane reads and sets, so the waterfall, the lists and the inspector always
//! agree on it (the guide's L7: views that share a selection share it).

use api::{EmitterId, RecordId, SignalRecord, SourceId};

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Selection {
    #[default]
    None,
    Receiver(SourceId),
    Record(RecordId),
    Emitter(EmitterId),
    /// A trunked system, by its control channel's emitter.
    System(EmitterId),
    /// A band-plan channel, as the CHANNELS view names it.
    Channel {
        plan: String,
        name: String,
    },
}

impl Selection {
    /// The selected record, when a record is selected.
    pub fn record(&self) -> Option<RecordId> {
        match self {
            Selection::Record(id) => Some(*id),
            _ => None,
        }
    }

    /// Whether `r` is the selection or part of it: the record itself, or a record of the selected emitter or system.
    pub fn covers(&self, r: &SignalRecord) -> bool {
        match self {
            Selection::Record(id) => r.id == *id,
            Selection::Emitter(e) | Selection::System(e) => r.emitter == Some(*e),
            _ => false,
        }
    }

    /// Selects `s`, or clears the selection when `s` is already selected (a second click deselects).
    pub fn toggle(&mut self, s: Selection) {
        *self = if *self == s { Selection::None } else { s };
    }
}
