//! Dark theme matching the UI mockup.

use egui::{Color32, CornerRadius, Frame, Margin, Stroke};

pub const BG: Color32 = Color32::from_rgb(0x0a, 0x0d, 0x11);
pub const BAR: Color32 = Color32::from_rgb(0x0d, 0x11, 0x16);
pub const PANEL: Color32 = Color32::from_rgb(0x10, 0x14, 0x1a);
pub const PANEL2: Color32 = Color32::from_rgb(0x14, 0x19, 0x20);
pub const PLOT_BG: Color32 = Color32::from_rgb(0x0c, 0x10, 0x15);
pub const BORDER: Color32 = Color32::from_rgb(0x1d, 0x24, 0x2d);
pub const BORDER2: Color32 = Color32::from_rgb(0x25, 0x2e, 0x38);
pub const GRID: Color32 = Color32::from_rgb(0x19, 0x20, 0x2a);
pub const TEXT: Color32 = Color32::from_rgb(0xdf, 0xe6, 0xee);
pub const TEXT2: Color32 = Color32::from_rgb(0xb9, 0xc4, 0xcf);
pub const MUTED: Color32 = Color32::from_rgb(0x8b, 0x97, 0xa5);
pub const ACCENT: Color32 = Color32::from_rgb(0x38, 0xd3, 0xcf);
/// Closed (finished) signal record boxes.
pub const RECORD: Color32 = Color32::from_rgb(0xc8, 0xd2, 0xdc);
/// Selected / hovered record.
pub const SELECT: Color32 = Color32::from_rgb(0xf2, 0xa3, 0x3a);
/// Known wideband occupants (FM broadcast, TV, LTE): the spectrum squelch.
pub const KNOWN: Color32 = Color32::from_rgb(0x8f, 0x86, 0xc9);
/// Digital channels carrying voice (talk activity).
pub const VOICE: Color32 = Color32::from_rgb(0x7e, 0xe0, 0x81);
/// Emergency calls.
/// Emitters first seen recently.
pub const NEW: Color32 = Color32::from_rgb(0x8c, 0xf0, 0x8a);
pub const ALERT: Color32 = Color32::from_rgb(0xf0, 0x5a, 0x4f);
pub const CHIP_FOCUS_BG: Color32 = Color32::from_rgb(0xe6, 0xed, 0xf3);
pub const CHIP_FOCUS_FG: Color32 = BG;

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = PLOT_BG;
    v.faint_bg_color = PANEL2;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = Color32::from_rgb(0x16, 0x4a, 0x4a);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    for w in [&mut v.widgets.inactive, &mut v.widgets.noninteractive] {
        w.bg_fill = PANEL2;
        w.weak_bg_fill = PANEL2;
        w.bg_stroke = Stroke::new(1.0, BORDER2);
        w.corner_radius = CornerRadius::same(4);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    // Slider rails and other filled shapes use bg_fill; keep them visible on cards.
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x2a, 0x34, 0x40);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT2);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x1a, 0x21, 0x2a);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x34, 0x40, 0x4d));
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x0f, 0x2b, 0x2b);
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    // Always dark, regardless of the OS light/dark setting.
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 5.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
    });
}

/// The icon rail: a bar with no side margin so 36 px fits an icon.
pub fn rail_frame() -> Frame {
    Frame::new().fill(BAR).inner_margin(Margin::symmetric(0, 4)).stroke(Stroke::new(1.0, BORDER))
}

pub fn bar_frame() -> Frame {
    Frame::new().fill(BAR).inner_margin(Margin::symmetric(12, 0)).stroke(Stroke::new(1.0, BORDER))
}

pub fn central_frame() -> Frame {
    Frame::new().fill(BG).inner_margin(Margin::same(6))
}

pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(6)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn chip(ui: &mut egui::Ui, text: &str, bg: Color32, fg: Color32) {
    Frame::new().fill(bg).corner_radius(3).inner_margin(Margin::symmetric(6, 1)).show(ui, |ui| {
        ui.label(egui::RichText::new(text).monospace().size(10.0).strong().color(fg));
    });
}

/// Waterfall colour map: near-black → navy → blue → cyan → yellow → orange → red.
pub fn waterfall_lut() -> [Color32; 256] {
    let stops: [(f32, [u8; 3]); 8] = [
        (0.0, [4, 6, 10]),
        (0.2, [10, 22, 48]),
        (0.36, [16, 62, 120]),
        (0.5, [24, 132, 180]),
        (0.63, [80, 208, 196]),
        (0.75, [226, 228, 110]),
        (0.87, [244, 156, 52]),
        (1.0, [232, 64, 58]),
    ];
    let mut lut = [Color32::BLACK; 256];
    for (i, c) in lut.iter_mut().enumerate() {
        let v = i as f32 / 255.0;
        let k = stops.windows(2).position(|w| v <= w[1].0).unwrap_or(stops.len() - 2);
        let (a, ca) = stops[k];
        let (b, cb) = stops[k + 1];
        let t = (v - a) / (b - a);
        let ch = |j: usize| (ca[j] as f32 + (cb[j] as f32 - ca[j] as f32) * t) as u8;
        *c = Color32::from_rgb(ch(0), ch(1), ch(2));
    }
    lut
}
