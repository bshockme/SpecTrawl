mod spectrum_view;
mod theme;

use std::time::{SystemTime, UNIX_EPOCH};

use engine::{Source, SourceConfig};
use sdr_core::{Backend, DeviceInfo};
use spectrum_view::SpectrumView;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1600.0, 960.0]).with_title("SpecTrawl"),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native("SpecTrawl", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

struct App {
    backends: Vec<Box<dyn Backend>>,
    devices: Vec<DeviceInfo>,
    source: Option<Source>,
    view: SpectrumView,
    paused: bool,
    tune_mhz: f64,
    error: Option<String>,
}

/// Command line: `--sim` to force the simulator, `--freq <MHz>` to set the
/// initial centre, `--serial <hex>` to pick a device.
struct Args {
    sim: bool,
    freq_mhz: Option<f64>,
    serial: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args { sim: false, freq_mhz: None, serial: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sim" => a.sim = true,
            "--freq" => a.freq_mhz = it.next().and_then(|v| v.parse().ok()),
            "--serial" => a.serial = it.next(),
            _ => log::warn!("ignoring argument {arg}"),
        }
    }
    a
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let args = parse_args();
        let backends: Vec<Box<dyn Backend>> = vec![Box::new(backend_airspy::AirspyBackend::new()), Box::new(backend_sim::SimBackend)];
        let mut app = Self {
            backends,
            devices: Vec::new(),
            source: None,
            view: SpectrumView::new(),
            paused: false,
            tune_mhz: args.freq_mhz.unwrap_or(if args.sim { 856.0 } else { 100.0 }),
            error: None,
        };
        app.rescan();
        // Until role assignment exists: an explicit serial, else the widest
        // hardware (R2 before Mini/others), else the simulator.
        let rank = |d: &DeviceInfo| match (d.backend.as_str(), d.label.as_str()) {
            ("sim", _) => 9,
            (_, "Airspy R2") => 0,
            _ => 1,
        };
        let pick = match (&args.serial, args.sim) {
            (Some(s), _) => app.devices.iter().find(|d| d.serial.eq_ignore_ascii_case(s)),
            (None, true) => app.devices.iter().find(|d| d.backend == "sim"),
            (None, false) => app.devices.iter().min_by_key(|d| rank(d)),
        }
        .cloned();
        if let Some(d) = pick {
            app.open_device(&d);
        }
        app
    }

    fn rescan(&mut self) {
        self.devices.clear();
        for b in &self.backends {
            match b.enumerate() {
                Ok(mut v) => self.devices.append(&mut v),
                Err(e) => log::info!("backend {}: {e}", b.name()),
            }
        }
    }

    fn open_device(&mut self, info: &DeviceInfo) {
        // Release the current device before opening another.
        self.source = None;
        self.view = SpectrumView::new();
        let Some(backend) = self.backends.iter().find(|b| b.name() == info.backend) else { return };
        let result = backend.open(&info.serial).and_then(|mut dev| {
            if let Err(e) = dev.set_center(self.tune_mhz * 1e6) {
                log::warn!("initial tune: {e}");
            }
            Source::start(dev, SourceConfig::default())
        });
        match result {
            Ok(s) => {
                self.tune_mhz = s.device().center_hz() / 1e6;
                self.source = Some(s);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{}: {e}", info.label)),
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.label(egui::RichText::new("SpecTrawl").strong().size(15.0));
            ui.add_space(8.0);
            let (txt, fg, bg) = if self.paused {
                ("PAUSED", theme::MUTED, theme::PANEL2)
            } else {
                ("● LIVE", egui::Color32::from_rgb(0xff, 0x8a, 0x8a), egui::Color32::from_rgb(0x2a, 0x12, 0x15))
            };
            egui::Frame::new().fill(bg).corner_radius(4).inner_margin(egui::Margin::symmetric(8, 2)).show(ui, |ui| {
                ui.label(egui::RichText::new(txt).monospace().color(fg).size(11.0));
            });
            ui.label(egui::RichText::new(utc_now()).monospace().color(theme::TEXT2));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(if self.paused { "▶ Resume" } else { "⏸ Pause" }).clicked() {
                    self.paused = !self.paused;
                }
            });
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("SOURCES").small().strong().color(theme::MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Rescan").clicked() {
                    // Release the open device so it can be probed too.
                    let reopen = self.source.as_ref().map(|s| s.info().clone());
                    self.source = None;
                    self.rescan();
                    if let Some(d) = reopen {
                        self.open_device(&d);
                    }
                }
            });
        });
        let mut to_open = None;
        for d in &self.devices {
            let open = self.source.as_ref().is_some_and(|s| s.info().serial == d.serial && s.info().backend == d.backend);
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&d.label).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if open {
                            theme::chip(ui, "FOCUS A", theme::CHIP_FOCUS_BG, theme::CHIP_FOCUS_FG);
                        } else if ui.small_button("Open").clicked() {
                            to_open = Some(d.clone());
                        }
                    });
                });
                ui.label(egui::RichText::new(format!("{} · {}", d.backend, d.serial)).small().color(theme::MUTED));
                if let (true, Some(src)) = (open, self.source.as_mut()) {
                    let dev = src.device();
                    ui.label(
                        egui::RichText::new(format!(
                            "{:.1} MS/s · {}-bit · {:?}",
                            dev.sample_rate() / 1e6,
                            dev.capabilities().adc_bits,
                            dev.capabilities().if_arch
                        ))
                        .small()
                        .color(theme::TEXT2),
                    );
                    ui.horizontal(|ui| {
                        ui.label("Centre");
                        let r = ui.add(egui::DragValue::new(&mut self.tune_mhz).speed(0.1).range(24.0..=1800.0).max_decimals(6).suffix(" MHz"));
                        if r.changed() {
                            if let Err(e) = src.set_center(self.tune_mhz * 1e6) {
                                self.error = Some(e.to_string());
                            }
                        }
                    });
                }
            });
        }
        if let Some(d) = to_open {
            self.open_device(&d);
        }
        if let Some(src) = &self.source {
            let dev = src.device();
            let st = src.stats();
            ui.add_space(8.0);
            ui.label(egui::RichText::new("ENGINE").small().strong().color(theme::MUTED));
            theme::card(ui, |ui| {
                let rate = dev.sample_rate();
                kv(ui, "FFT", &format!("{} pt · {:.0}/s", src.config().fft_size, st.ffts_per_sec));
                kv(ui, "Bin width", &format!("{:.0} Hz", rate / src.config().fft_size as f64));
                kv(ui, "Look-back", &format!("{:.1} / {:.0} s", st.ring_samples as f64 / rate, src.config().ring_seconds));
                kv(ui, "Gaps", &format!("{} ({} samples)", st.gaps, st.lost_samples));
            });
            ui.add_space(8.0);
            ui.label(egui::RichText::new("DISPLAY").small().strong().color(theme::MUTED));
            theme::card(ui, |ui| {
                ui.spacing_mut().slider_width = 110.0;
                ui.add(egui::Slider::new(&mut self.view.db_max, -80.0..=0.0).text("top dB"));
                ui.add(egui::Slider::new(&mut self.view.db_min, -160.0..=-40.0).text("floor dB"));
                ui.add(egui::Slider::new(&mut self.view.smoothing, 0.0..=0.95).text("smoothing"));
                if ui.button("Auto range").clicked() {
                    self.view.auto_range();
                }
            });
        }
        if let Some(e) = &self.error {
            ui.colored_label(egui::Color32::from_rgb(0xff, 0x70, 0x96), e);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("top").exact_size(40.0).frame(theme::bar_frame()).show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").exact_size(24.0).frame(theme::bar_frame()).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                if let Some(src) = &self.source {
                    let st = src.stats();
                    ui.label(egui::RichText::new(format!(
                        "1 source · ring {:.0} MB · {} samples · drops {}",
                        st.ring_capacity as f64 * 4.0 / 1e6,
                        st.total_samples,
                        st.gaps
                    )).monospace().small().color(theme::MUTED));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(format!("{:.0} fps", 1.0 / ctx.input(|i| i.stable_dt).max(1e-3))).monospace().small().color(theme::MUTED));
                });
            });
        });
        egui::Panel::left("side").exact_size(260.0).frame(theme::side_frame()).show(ui, |ui| self.side_panel(ui));
        egui::CentralPanel::default().frame(theme::central_frame()).show(ui, |ui| {
            if let Some(src) = &self.source {
                if !self.paused {
                    self.view.ingest(src);
                }
                self.view.show(ui, src);
            }
        });
        ctx.request_repaint();
    }
}

fn kv(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(k).color(theme::MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(v).monospace().color(theme::TEXT2));
        });
    });
}

fn utc_now() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let s = t.as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}.{} UTC", s / 3600, (s / 60) % 60, s % 60, t.subsec_millis() / 100)
}
