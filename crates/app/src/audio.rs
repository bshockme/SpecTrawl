//! Audio output: plays a source's listener queue on the default device.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use engine::AudioQueue;
use parking_lot::Mutex;

pub struct AudioOut {
    _stream: cpal::Stream,
    queue: Arc<Mutex<Option<Arc<AudioQueue>>>>,
    volume: Arc<AtomicU32>,
}

impl AudioOut {
    /// Opens the default output device. Audio is 48 kHz mono from the
    /// engine; it is resampled if the device runs at another rate and
    /// copied to every channel.
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let dev = host.default_output_device().ok_or("no audio output device")?;
        let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
        let channels = cfg.channels() as usize;
        let rate = cfg.sample_rate().0 as f64;
        let queue: Arc<Mutex<Option<Arc<AudioQueue>>>> = Arc::new(Mutex::new(None));
        let volume = Arc::new(AtomicU32::new(0.5f32.to_bits()));
        let (q, vol) = (queue.clone(), volume.clone());
        // Linear resampler state: engine samples per device frame.
        let step = 48_000.0 / rate;
        let (mut pos, mut prev, mut cur) = (0.0f64, 0.0f32, 0.0f32);
        let mut mono = Vec::new();
        let err = |e| log::warn!("audio: {e}");
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => dev.build_output_stream(
                &cfg.into(),
                move |data: &mut [f32], _| {
                    let frames = data.len() / channels;
                    let need = ((frames as f64 * step) + pos).ceil() as usize + 1;
                    mono.resize(need, 0.0);
                    match q.lock().as_ref() {
                        Some(a) => a.pop_into(&mut mono[..need]),
                        None => mono.iter_mut().for_each(|v| *v = 0.0),
                    }
                    let v = f32::from_bits(vol.load(Ordering::Relaxed));
                    let mut i = 0;
                    for frame in data.chunks_mut(channels) {
                        while pos >= 1.0 {
                            prev = cur;
                            cur = mono.get(i).copied().unwrap_or(0.0);
                            i += 1;
                            pos -= 1.0;
                        }
                        let s = (prev + (cur - prev) * pos as f32) * v;
                        frame.iter_mut().for_each(|o| *o = s);
                        pos += step;
                    }
                },
                err,
                None,
            ),
            f => return Err(format!("unsupported audio sample format {f:?}")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Self { _stream: stream, queue, volume })
    }

    /// Plays this queue (or silence).
    pub fn set_source(&self, q: Option<Arc<AudioQueue>>) {
        *self.queue.lock() = q;
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }
}
