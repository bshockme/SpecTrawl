//! SpecTrawl's interface on the new engine: it renders the engine's published
//! [`api::Snapshot`]s and sends [`api::Command`]s back. This file is the only
//! place the engine is named, to start it.

pub mod app;
pub mod devices_pane;
pub mod log_pane;
pub mod receiver_pane;
pub mod tabs;
pub mod theme;

use engine::snapshot::PublisherConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, RingBudget, SourceConfig};

/// Starts the engine and its snapshot publisher, then runs the window until it closes.
pub fn run() -> eframe::Result {
    let budget = BudgetConfig { guaranteed_secs: 30.0, ceiling_bytes: RingBudget::physical_ram_bytes() / 2, sweep_secs: 1.0 };
    let draft = devices_pane::BudgetDraft { guaranteed_secs: budget.guaranteed_secs, ceiling_mb: budget.ceiling_bytes >> 20 };
    let engine = Engine::new(EngineConfig { budget, bus: BusConfig::default(), source: SourceConfig::default() });
    let backends = vec![Box::new(backend_airspy::AirspyBackend::new()) as _, Box::new(backend_sim::SimBackend) as _];
    let handle = engine::snapshot::spawn(engine, backends, PublisherConfig::default());
    let viewport = egui::ViewportBuilder::default().with_title("SpecTrawl").with_inner_size([1600.0, 960.0]);
    let options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Wgpu, ..Default::default() };
    eframe::run_native(
        "SpecTrawl",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            let mut app = app::App::new(handle).with_budget(draft);
            if let Some(storage) = cc.storage {
                app.restore(storage);
            }
            Ok(Box::new(app))
        }),
    )
}
