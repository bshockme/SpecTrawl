mod spectrum_view;
mod theme;

use std::time::{SystemTime, UNIX_EPOCH};

use engine::{Source, SourceConfig};
use sdr_core::Backend;
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
    source: Option<Source>,
    view: SpectrumView,
    paused: bool,
    error: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let mut error = None;
        let backend = backend_sim::SimBackend;
        let source = backend
            .enumerate()
            .and_then(|devs| backend.open(&devs[0].serial))
            .and_then(|dev| Source::start(dev, SourceConfig::default()))
            .map_err(|e| error = Some(e.to_string()))
            .ok();
        Self { source, view: SpectrumView::new(), paused: false, error }
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
        ui.label(egui::RichText::new("SOURCES").small().strong().color(theme::MUTED));
        if let Some(src) = &self.source {
            let dev = src.device();
            let st = src.stats();
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&dev.info().label).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        theme::chip(ui, "FOCUS A", theme::CHIP_FOCUS_BG, theme::CHIP_FOCUS_FG);
                    });
                });
                ui.label(
                    egui::RichText::new(format!(
                        "{:.3} MHz · {:.1} MS/s",
                        dev.center_hz() / 1e6,
                        dev.sample_rate() / 1e6
                    ))
                    .monospace()
                    .color(theme::TEXT2),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "{}-bit · {:?} · serial {}",
                        dev.capabilities().adc_bits,
                        dev.capabilities().if_arch,
                        dev.info().serial
                    ))
                    .small()
                    .color(theme::MUTED),
                );
            });
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
