//! The engine: one keeper, one channel bus, and the sources attached to it.

use std::collections::HashMap;
use std::sync::Arc;

use api::{AudioMode, AudioView, PlayId, PlayTarget, PlayTo, SignalRecord};
use parking_lot::Mutex;
use sdr_core::Device;

use crate::audio::AudioConfig;
use crate::budget::{BudgetConfig, Mode, RingBudget, SourceId};
use crate::channels::{BusConfig, ChannelBus};
use crate::classify::{Classifier, ClassifyConfig};
use crate::decoders::DecoderBank;
use crate::recorder::{Recorder, SliceConfig};
use crate::records::{Loaded, RecordBook, RecordSink};
use crate::replay::{Player, Resolved};
use crate::ring::keeper::Keeper;
use crate::source::{Source, SourceConfig};
use crate::store::{Store, StoreConfig};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// No default: how much memory the rings may hold is the caller's decision.
    pub budget: BudgetConfig,
    pub bus: BusConfig,
    /// Every source added starts with this configuration.
    pub source: SourceConfig,
    /// Where records, emitters, tags and notes are kept across restarts; `None`: in memory only.
    pub store: Option<StoreConfig>,
    /// The slice tier (every record's IQ on disk); `None`: off.
    pub slices: Option<SliceConfig>,
    /// What plays the replays' audio.
    pub audio: AudioConfig,
    /// The detector bank's pool.
    pub classify: ClassifyConfig,
}

pub struct Engine {
    cfg: EngineConfig,
    /// Every source's rows go here (the stages' row sink).
    records: Arc<RecordBook>,
    store: Option<Arc<Store>>,
    /// Why the configured store could not be opened.
    store_error: Option<String>,
    bus: Arc<ChannelBus>,
    player: Player,
    classifier: Classifier,
    decoders: DecoderBank,
    sources: Arc<Mutex<HashMap<SourceId, Arc<Source>>>>,
    /// The engine's handle; each `Source` holds the keeper too, so it lives
    /// as long as the last of them whatever the field order.
    keeper: Arc<Keeper>,
}

impl Engine {
    /// Starts the keeper (from `cfg.budget`) and the bus (from `cfg.bus`), with a record book on every source's rows
    /// (unless `cfg.bus` already names a row sink).
    pub fn new(cfg: EngineConfig) -> Engine {
        let (store, loaded, store_error) = match cfg.store.clone().map(Store::open) {
            None => (None, Loaded::default(), None),
            Some(Ok((s, loaded))) => (Some(s), loaded, None),
            Some(Err(e)) => {
                log::error!("record store: {e}");
                (None, Loaded::default(), Some(e))
            }
        };
        let sink = store.clone().map(|s| s as Arc<dyn RecordSink>);
        let records = Arc::new(RecordBook::new(loaded, sink));
        let keeper = Keeper::start(RingBudget::new(cfg.budget.clone()));
        let mut bus_cfg = cfg.bus.clone();
        bus_cfg.row_sink.get_or_insert_with(|| records.clone());
        let bus = ChannelBus::new(bus_cfg);
        if let Some(s) = cfg.slices.clone() {
            let r = Recorder::new(s);
            r.attach(&bus);
            records.set_recorder(r);
        }
        let player = Player::new(cfg.audio);
        let sources = Arc::new(Mutex::new(HashMap::new()));
        let classifier = Classifier::start(cfg.classify.clone(), records.clone(), bus.clone(), sources.clone());
        // Decoders attach to what the bank found: on with it.
        let decoders = DecoderBank::start(cfg.classify.enabled, records.clone(), bus.clone(), sources.clone());
        Engine { cfg, records, store, store_error, keeper, bus, player, classifier, decoders, sources }
    }

    /// Starts the device streaming into a new source and attaches the source to the bus.
    pub fn add_source(&self, device: Box<dyn Device>, mode: Mode) -> sdr_core::Result<Arc<Source>> {
        let source = Arc::new(Source::start(device, self.cfg.source.clone(), mode, &self.keeper)?);
        self.records.add_source(source.clone());
        self.bus.attach(source.clone());
        self.sources.lock().insert(source.id(), source.clone());
        Ok(source)
    }

    /// Detaches the source from the bus (its consumers end with
    /// `SourceDetached`), then drops the engine's handle on it. False if no
    /// such source was added.
    pub fn remove_source(&self, id: SourceId) -> bool {
        if self.sources.lock().remove(&id).is_none() {
            return false;
        }
        self.bus.detach(id);
        // The stage has stopped: no row can race the book's closing of the source's records.
        self.records.remove_source(id);
        true
    }

    /// The configuration the engine was built with; a later budget change (`keeper().budget().set_config`) is not shown here.
    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    pub fn source(&self, id: SourceId) -> Option<Arc<Source>> {
        self.sources.lock().get(&id).cloned()
    }

    pub fn sources(&self) -> Vec<Arc<Source>> {
        self.sources.lock().values().cloned().collect()
    }

    /// Records and emitters.
    pub fn records(&self) -> &Arc<RecordBook> {
        &self.records
    }

    /// The detector bank's budget.
    pub fn classify_view(&self) -> api::ClassifyView {
        self.classifier.view()
    }

    /// The running decoders.
    pub fn decoder_views(&self) -> Vec<api::DecoderView> {
        self.decoders.views()
    }

    /// The record store, when one is configured and opened.
    pub fn store(&self) -> Option<&Arc<Store>> {
        self.store.as_ref()
    }

    /// Why the configured store could not be opened.
    pub fn store_error(&self) -> Option<&str> {
        self.store_error.as_deref()
    }

    pub fn bus(&self) -> &Arc<ChannelBus> {
        &self.bus
    }

    pub fn keeper(&self) -> &Arc<Keeper> {
        &self.keeper
    }

    pub fn player(&self) -> &Player {
        &self.player
    }

    /// Starts a replay (spec §4.7); `Err` says why it can't be heard.
    /// A relative WAV path is under [`audio::default_dir`](crate::audio::default_dir).
    pub fn play(&self, target: PlayTarget, mode: Option<AudioMode>, gain: f32, to: PlayTo) -> Result<PlayId, String> {
        let to = match to {
            PlayTo::Wav(p) if p.is_relative() => PlayTo::Wav(crate::audio::default_dir().join(p)),
            to => to,
        };
        let what = match &target {
            PlayTarget::Record(id) => {
                let record = self
                    .records
                    .get(*id)
                    .or_else(|| {
                        self.store.as_ref().and_then(|s| s.query_result()).and_then(|q| q.records.iter().find(|r| r.id == *id).cloned())
                    })
                    .ok_or_else(|| format!("record {} is not in memory or in the last search", id.0))?;
                let source = self.ring_of(&record);
                Resolved::Record { record, source }
            }
            PlayTarget::Region { source, center_hz, bandwidth_hz, from_sample, to_sample } => Resolved::Region {
                source: self.source(*source).ok_or("no such receiver")?,
                center_hz: *center_hz,
                bandwidth_hz: *bandwidth_hz,
                from_sample: *from_sample,
                to_sample: *to_sample,
            },
        };
        self.player.play(&self.bus, what, target, mode, gain, to)
    }

    pub fn stop_play(&self, id: PlayId) -> bool {
        self.player.stop(&self.bus, id)
    }

    pub fn audio_view(&self) -> AudioView {
        self.player.view(&self.records)
    }

    /// The source whose ring holds a record's samples: the same device, in this run (source ids are reused across
    /// runs, so the source's own timeline must put the record's first sample at the record's start time).
    fn ring_of(&self, r: &SignalRecord) -> Option<Arc<Source>> {
        let s = self.source(r.source)?;
        if *s.snapshot().info.serial != *r.serial {
            return None;
        }
        let t = s.time_of(r.first_sample)?;
        let off = t.duration_since(r.t_start).or_else(|e| Ok::<_, ()>(e.duration())).ok()?;
        (off < std::time::Duration::from_secs(1)).then_some(s)
    }
}

impl Drop for Engine {
    /// Detaches every source (its consumers end with `SourceDetached`),
    /// stops the bus and drops the engine's handles on the sources.
    fn drop(&mut self) {
        // No new windows from here; the ones on the bus end with their sources.
        self.classifier.stop();
        self.decoders.stop();
        let ids: Vec<SourceId> = self.sources.lock().keys().copied().collect();
        for id in ids {
            self.bus.detach(id);
            // Its stage has stopped: close its open records so the store keeps them.
            self.records.remove_source(id);
        }
        self.bus.stop();
        self.sources.lock().clear();
        if let Some(s) = &self.store {
            s.close();
        }
    }
}
