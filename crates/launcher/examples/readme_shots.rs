//! The README's screenshots: the app as the launcher builds it, on real radios, drawn offscreen by egui's own wgpu
//! renderer (`egui_kittest`), so no window or mouse is needed. Each radio is opened and tuned, the app runs in real
//! time for `secs`, then each named tab is brought to the front and saved as a PNG.
//!
//!     cargo run --release -p spectrawl-next --example readme_shots -- <out_dir> <secs> <serial>@<MHz>... \
//!         [--db <serial>=<lo>,<hi>]... [--records <filter words>]
//!
//! Shots (in `out_dir`): `receiver_<letter>.png` for each receiver; `records.png` (the list filtered by `--records`,
//! its newest decoded record selected), `emitters.png` (the busiest named one selected), `systems.png`, `channels.png` (in front of the receiver tuned to the air
//! band, if one is) and `catalogue.png`, drawn taller. `--db` sets a receiver's waterfall range, as the app's saved settings would.

use std::time::{Duration, Instant};

use api::{Command, Snapshot};
use engine::snapshot::PublisherConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, RingBudget, SourceConfig};
use spectrawl_app::app::App;
use spectrawl_app::selection::Selection;
use spectrawl_app::tabs::Tab;

/// The window the launcher opens, in points, and the scale it is drawn at; the list shots are taller.
const SIZE: [f32; 2] = [1600.0, 960.0];
const TALL: [f32; 2] = [1600.0, 1300.0];
const SCALE: f32 = 1.25;

/// The app's saved settings, held in memory: what `--db` sets.
struct Settings(std::collections::HashMap<String, String>);

impl eframe::Storage for Settings {
    fn get_string(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn set_string(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }
    fn remove_string(&mut self, key: &str) {
        self.0.remove(key);
    }
    fn flush(&mut self) {}
}

fn main() {
    env_logger::init();
    let mut a = std::env::args().skip(1);
    let (mut plain, mut ranges, mut filter) = (Vec::new(), serde_json::Map::new(), String::new());
    while let Some(x) = a.next() {
        match x.as_str() {
            "--db" => {
                let v = a.next().expect("--db <serial>=<lo>,<hi>");
                let (serial, range) = v.split_once('=').expect("<serial>=<lo>,<hi>");
                let (lo, hi) = range.split_once(',').expect("<lo>,<hi>");
                ranges.insert(serial.into(), serde_json::json!([lo.parse::<f32>().unwrap(), hi.parse::<f32>().unwrap()]));
            }
            "--records" => filter = a.next().expect("--records <words>"),
            _ => plain.push(x),
        }
    }
    if plain.len() < 3 {
        eprintln!("usage: readme_shots <out_dir> <secs> <serial>@<MHz>... [--db <serial>=<lo>,<hi>]... [--records <words>]");
        std::process::exit(2);
    }
    let out = std::path::PathBuf::from(&plain[0]);
    std::fs::create_dir_all(&out).expect("the output folder");
    let secs: u64 = plain[1].parse().expect("secs");
    let radios: Vec<(String, f64)> = plain[2..]
        .iter()
        .map(|s| {
            let (serial, mhz) = s.split_once('@').expect("<serial>@<MHz>");
            (serial.to_string(), mhz.parse::<f64>().expect("MHz") * 1e6)
        })
        .collect();

    // As the launcher builds it, without the store, the slices or the speaker.
    let budget = BudgetConfig { guaranteed_secs: 30.0, ceiling_bytes: RingBudget::physical_ram_bytes() / 2, sweep_secs: 1.0 };
    let draft =
        spectrawl_app::devices_pane::BudgetDraft { guaranteed_secs: budget.guaranteed_secs, ceiling_mb: budget.ceiling_bytes >> 20 };
    let engine = Engine::new(EngineConfig {
        budget,
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: engine::classify::ClassifyConfig::default(),
    });
    let backends = vec![Box::new(backend_airspy::AirspyBackend::new()) as _, Box::new(backend_sim::SimBackend) as _];
    let (handle, guard) = engine::snapshot::spawn_with_guard(engine, backends, PublisherConfig::default());
    let mut app = App::new(handle).with_budget(draft);
    let settings = Settings([("spectrawl-next".to_string(), serde_json::json!({ "db_ranges": ranges }).to_string())].into());
    app.restore(&settings);

    let mut h = egui_kittest::Harness::builder()
        .with_size(SIZE)
        .with_pixels_per_point(SCALE)
        .wgpu()
        .build_ui_state(|ui, app: &mut App| app.frame(ui), app);

    // Runs the app's frames in real time (the engine is live).
    let run = |h: &mut egui_kittest::Harness<'_, App>, d: Duration| {
        let t0 = Instant::now();
        while t0.elapsed() < d {
            h.step();
            std::thread::sleep(Duration::from_millis(16));
        }
    };
    let snap = |h: &egui_kittest::Harness<'_, App>| -> std::sync::Arc<Snapshot> { h.state().handle().latest() };

    // Open each radio once the engine has listed it, and tune it when it is open.
    let t0 = Instant::now();
    while snap(&h).devices.len() < 1 + radios.len() && t0.elapsed() < Duration::from_secs(10) {
        run(&mut h, Duration::from_millis(200));
    }
    for (serial, _) in &radios {
        let d = snap(&h).devices.iter().find(|d| &d.info.serial == serial).cloned().unwrap_or_else(|| panic!("no device {serial}"));
        h.state_mut().push_command(Command::Open { backend: d.info.backend.clone(), serial: serial.clone() });
        run(&mut h, Duration::from_secs(2));
        let id = snap(&h).devices.iter().find(|d| &d.info.serial == serial).and_then(|d| d.open_as).expect("opened");
        h.state_mut().push_command(Command::Tune { source: id, center_hz: radios.iter().find(|r| &r.0 == serial).unwrap().1 });
    }
    println!("running {secs} s");
    run(&mut h, Duration::from_secs(secs));

    let save = |h: &mut egui_kittest::Harness<'_, App>, name: &str| {
        run(h, Duration::from_millis(500));
        let img = h.render().expect("render");
        let path = out.join(format!("{name}.png"));
        img.save(&path).expect("save");
        println!("wrote {}", path.display());
    };
    let s = snap(&h);
    let receivers: Vec<_> = s.receivers.iter().map(|r| (r.id, r.letter)).collect();
    for (id, letter) in &receivers {
        h.state_mut().show_tab(Tab::Receiver(*id));
        save(&mut h, &format!("receiver_{}", letter.to_ascii_lowercase()));
    }
    let air = s.receivers.iter().find(|r| (118e6..137e6).contains(&r.center_hz)).map(|r| r.id);
    h.set_size(TALL.into());
    // The records: filtered, the newest one a decoder named selected so its claims show.
    h.state_mut().records_pane_mut().filter = filter;
    let rows = h.state().records_pane().rows(&s);
    let decoded = rows.iter().find(|r| r.claims.iter().any(|c| matches!(c.by, api::Evidence::Decoder { .. }))).map(|r| r.id);
    h.state_mut().select(decoded);
    if let Some((id, _)) = receivers.first() {
        h.state_mut().show_tab(Tab::Receiver(*id));
    }
    h.state_mut().show_tab(Tab::Records);
    save(&mut h, "records");
    // The emitters, the most active one selected.
    let busiest = s.records.emitters.iter().filter(|e| e.headline.is_some()).max_by_key(|e| e.records).map(|e| e.id);
    h.state_mut().set_selection(busiest.map_or(Selection::None, Selection::Emitter));
    h.state_mut().show_tab(Tab::Emitters);
    save(&mut h, "emitters");
    h.state_mut().select(None);
    h.state_mut().show_tab(Tab::Systems);
    save(&mut h, "systems");
    if let Some(id) = air {
        h.state_mut().show_tab(Tab::Receiver(id));
    }
    h.state_mut().show_tab(Tab::Channels);
    save(&mut h, "channels");
    h.state_mut().show_tab(Tab::Catalogue);
    save(&mut h, "catalogue");
    drop(h);
    guard.join(Duration::from_secs(3));
}
