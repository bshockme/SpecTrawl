//! Wideband SigMF export out of a [`Source`]'s ring, through its segment table.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ring;
use crate::segments::{CoverError, Segment, SpansSegments};
use crate::sigmf::{self, Meta};
use crate::source::Source;

const CHUNK_SAMPLES: usize = 262_144;

#[derive(Debug)]
pub enum ExportError {
    Spans(SpansSegments),
    /// `Ring(ReadError::Gap)` when the range touches a gap's hole (found in
    /// the segment table or by the ring).
    Ring(ring::ReadError),
    Io(std::io::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Spans(s) => write!(f, "the range spans segments {} to {}", s.first.0, s.last.0),
            ExportError::Ring(ring::ReadError::Gap) => f.write_str("the range touches a gap: samples the device never delivered"),
            ExportError::Ring(ring::ReadError::Overwritten) => f.write_str("the range is no longer held"),
            ExportError::Ring(ring::ReadError::Lost) => f.write_str("the range was discarded by the OS under memory pressure"),
            ExportError::Ring(ring::ReadError::Future) => f.write_str("the range has not been written yet"),
            ExportError::Io(e) => write!(f, "writing the export failed: {e}"),
        }
    }
}

impl std::error::Error for ExportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExportError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ExportError {
    fn from(e: std::io::Error) -> Self {
        ExportError::Io(e)
    }
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub data_path: PathBuf,
    pub meta_path: PathBuf,
    pub samples: u64,
    pub segment: Segment,
}

impl Source {
    /// Writes [start, end) as ci16_le SigMF named <dir>/<stem>.sigmf-{data,meta}. Checks the segment and the ring range
    /// before creating any file; reads in 262144-sample chunks; on a mid-way ring error deletes the partial files.
    pub fn export_wideband(&self, start: u64, end: u64, dir: &Path, stem: &str) -> Result<ExportResult, ExportError> {
        if end <= start {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput).into());
        }
        // The ring range first: once `head` has passed `end`, no boundary can
        // still be pushed inside the range, so the segment resolved next is
        // the one every sample in it was recorded with.
        let ring = self.ring();
        if end > ring.head() {
            return Err(ExportError::Ring(ring::ReadError::Future));
        }
        if start < ring.oldest() {
            return Err(ExportError::Ring(ring::ReadError::Overwritten));
        }
        let segment = self.segment_covering(start, end).map_err(|e| match e {
            CoverError::Spans(s) => ExportError::Spans(s),
            CoverError::Gap(_) => ExportError::Ring(ring::ReadError::Gap),
        })?;
        std::fs::create_dir_all(dir)?;
        let data_path = dir.join(format!("{stem}.sigmf-data"));
        let meta_path = dir.join(format!("{stem}.sigmf-meta"));
        if let Err(e) = write_data(self, start, end, &data_path) {
            let _ = std::fs::remove_file(&data_path);
            return Err(e);
        }
        let snap = self.snapshot();
        let meta = Meta {
            datatype: "ci16_le",
            sample_rate: segment.sample_rate,
            frequency: segment.center_hz,
            datetime: self.time_of(start).map(sigmf::rfc3339),
            hw: snap.info.label.clone(),
            description: format!("samples {start}..{end}"),
            sample_count: end - start,
        };
        if let Err(e) = sigmf::write_meta(&meta_path, &meta) {
            let _ = std::fs::remove_file(&data_path);
            let _ = std::fs::remove_file(&meta_path);
            return Err(e.into());
        }
        Ok(ExportResult { data_path, meta_path, samples: end - start, segment })
    }
}

fn write_data(src: &Source, start: u64, end: u64, path: &Path) -> Result<(), ExportError> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut buf = vec![[0i16; 2]; CHUNK_SAMPLES];
    let mut bytes = Vec::with_capacity(CHUNK_SAMPLES * 4);
    let mut at = start;
    while at < end {
        let n = ((end - at) as usize).min(CHUNK_SAMPLES);
        src.ring().read_raw(at, &mut buf[..n]).map_err(ExportError::Ring)?;
        bytes.clear();
        for s in &buf[..n] {
            bytes.extend_from_slice(&s[0].to_le_bytes());
            bytes.extend_from_slice(&s[1].to_le_bytes());
        }
        file.write_all(&bytes)?;
        at += n as u64;
    }
    file.flush()?;
    Ok(())
}
