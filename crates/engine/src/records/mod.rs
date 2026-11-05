//! Records: the burst detector, emitter grouping and the record book (spec §4.4, step 4).

pub mod detector;

pub use detector::{Detected, Detector, DetectorConfig, Frame, Output};
