//! Records: the burst detector, emitter grouping and the record book (spec §4.4, step 4).

pub mod book;
pub mod detector;
pub mod emitters;

pub use book::{Loaded, RecordBook, RecordSink, WINDOW};
pub use detector::{Detected, Detector, DetectorConfig, Frame, Output};
pub use emitters::{EMITTER_WINDOW, EmitterBook, EmitterState, Observed};
