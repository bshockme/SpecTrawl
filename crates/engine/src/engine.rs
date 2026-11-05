//! The engine: one keeper, one channel bus, and the sources attached to it.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use sdr_core::Device;

use crate::budget::{BudgetConfig, Mode, RingBudget, SourceId};
use crate::channels::{BusConfig, ChannelBus};
use crate::records::{Loaded, RecordBook};
use crate::ring::keeper::Keeper;
use crate::source::{Source, SourceConfig};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// No default: how much memory the rings may hold is the caller's decision.
    pub budget: BudgetConfig,
    pub bus: BusConfig,
    /// Every source added starts with this configuration.
    pub source: SourceConfig,
}

pub struct Engine {
    cfg: EngineConfig,
    /// Every source's rows go here (the stages' row sink).
    records: Arc<RecordBook>,
    bus: Arc<ChannelBus>,
    sources: Mutex<HashMap<SourceId, Arc<Source>>>,
    /// The engine's handle; each `Source` holds the keeper too, so it lives
    /// as long as the last of them whatever the field order.
    keeper: Arc<Keeper>,
}

impl Engine {
    /// Starts the keeper (from `cfg.budget`) and the bus (from `cfg.bus`), with a record book on every source's rows
    /// (unless `cfg.bus` already names a row sink).
    pub fn new(cfg: EngineConfig) -> Engine {
        let records = Arc::new(RecordBook::new(Loaded::default(), None));
        let keeper = Keeper::start(RingBudget::new(cfg.budget.clone()));
        let mut bus_cfg = cfg.bus.clone();
        bus_cfg.row_sink.get_or_insert_with(|| records.clone());
        let bus = ChannelBus::new(bus_cfg);
        Engine { cfg, records, keeper, bus, sources: Mutex::new(HashMap::new()) }
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

    pub fn bus(&self) -> &Arc<ChannelBus> {
        &self.bus
    }

    pub fn keeper(&self) -> &Arc<Keeper> {
        &self.keeper
    }
}

impl Drop for Engine {
    /// Detaches every source (its consumers end with `SourceDetached`),
    /// stops the bus and drops the engine's handles on the sources.
    fn drop(&mut self) {
        let ids: Vec<SourceId> = self.sources.lock().keys().copied().collect();
        for id in ids {
            self.bus.detach(id);
        }
        self.bus.stop();
        self.sources.lock().clear();
    }
}
