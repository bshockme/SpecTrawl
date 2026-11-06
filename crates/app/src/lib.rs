//! SpecTrawl's interface on the new engine: it renders the engine's published
//! [`api::Snapshot`]s and sends [`api::Command`]s back. This file is the only
//! place the engine is named, to start it.

pub mod app;
pub mod devices_pane;
pub mod log_pane;
pub mod receiver_pane;
pub mod records_pane;
pub mod tabs;
pub mod theme;
pub mod trace;
pub mod waterfall;

use engine::snapshot::PublisherConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, RingBudget, SourceConfig};

/// How long, after the window closes, to wait for the engine to stop its devices before exiting anyway.
const SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Starts the engine and its snapshot publisher, then runs the window until it closes. The window's app holds the only
/// handle, so once eframe has dropped it the publisher stops the devices and drops the engine; this waits (bounded)
/// for that before returning, so the process does not exit under a device that is still streaming.
pub fn run() -> eframe::Result {
    let budget = BudgetConfig { guaranteed_secs: 30.0, ceiling_bytes: RingBudget::physical_ram_bytes() / 2, sweep_secs: 1.0 };
    let draft = devices_pane::BudgetDraft { guaranteed_secs: budget.guaranteed_secs, ceiling_mb: budget.ceiling_bytes >> 20 };
    let store = Some(engine::store::StoreConfig::new(engine::store::default_path()));
    let slices = Some(engine::recorder::SliceConfig::new(engine::recorder::default_dir()));
    let audio = engine::audio::AudioConfig::Device;
    let classify = engine::classify::ClassifyConfig::default();
    let engine =
        Engine::new(EngineConfig { budget, bus: BusConfig::default(), source: SourceConfig::default(), store, slices, audio, classify });
    let backends = vec![Box::new(backend_airspy::AirspyBackend::new()) as _, Box::new(backend_sim::SimBackend) as _];
    let (handle, guard) = engine::snapshot::spawn_with_guard(engine, backends, PublisherConfig::default());
    let viewport = egui::ViewportBuilder::default().with_title("SpecTrawl").with_inner_size([1600.0, 960.0]);
    let options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Wgpu, ..Default::default() };
    // The app name keys eframe's storage: apart from the old app's "SpecTrawl".
    let result = eframe::run_native(
        "SpecTrawl Next",
        options,
        // `move`: the handle lives only in the app, which `run_native` drops before it returns.
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            let mut app = app::App::new(handle).with_budget(draft);
            if let Some(storage) = cc.storage {
                app.restore(storage);
            }
            Ok(Box::new(app))
        }),
    );
    if !guard.join(SHUTDOWN_WAIT) {
        log::warn!("the engine did not stop within {SHUTDOWN_WAIT:?} of the window closing; exiting anyway");
    }
    result
}
