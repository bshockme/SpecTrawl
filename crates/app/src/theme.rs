//! The house guide's Instrument look (`~/auggiesoft-ui-guide`, `docs/ui-design.md` §0): flat neutral chrome, black
//! plot wells, colour only on data, the selection and real problems. Every colour here fills one of the guide's roles
//! and means only that.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, FontFamily, FontId, Frame, Margin, RichText, Stroke};

// Surfaces.
pub const BG: Color32 = Color32::from_rgb(0x11, 0x12, 0x14);
pub const PANEL: Color32 = Color32::from_rgb(0x1c, 0x1d, 0x21);
pub const HEADER: Color32 = Color32::from_rgb(0x16, 0x17, 0x1a);
/// Plot wells (spectrum, waterfall): flat black, so the chrome never tints the signal (C10).
pub const WELL: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
pub const LINE: Color32 = Color32::from_rgb(0x2c, 0x2e, 0x33);
pub const CHIP: Color32 = Color32::from_rgb(0x26, 0x27, 0x2c);
pub const BUTTON: Color32 = Color32::from_rgb(0x2b, 0x2d, 0x32);
pub const GRATICULE: Color32 = Color32::from_rgb(0x33, 0x36, 0x3c);

// Ink.
pub const INK: Color32 = Color32::from_rgb(0xe6, 0xe7, 0xea);
/// Secondary text and units.
pub const INK2: Color32 = Color32::from_rgb(0xa9, 0xab, 0xb1);
/// Disabled and decorative only, never meaning.
pub const INK3: Color32 = Color32::from_rgb(0x6c, 0x6e, 0x74);
/// Dark ink on a lit (accent) fill.
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x0b, 0x0c, 0x0e);

// The six coded state roles (guide `rules/color-semantics.md`): one meaning each, always with a word or shape.
/// On, selected, you are here. Never a data series.
pub const ACCENT: Color32 = Color32::from_rgb(0x5a, 0xa9, 0xf0);
/// Armed, tolerance, a warning that needs a look.
pub const CAUTION: Color32 = Color32::from_rgb(0xf0, 0xa8, 0x30);
/// Failure, overload, losing samples, device lost.
pub const ALARM: Color32 = Color32::from_rgb(0xff, 0x4d, 0x4d);
/// In spec, healthy: a locked decoder.
pub const OK: Color32 = Color32::from_rgb(0x3c, 0xcf, 0x6e);
/// Hot: audio playing out, recording.
pub const LIVE: Color32 = Color32::from_rgb(0xff, 0x5c, 0xc8);
/// Advisory: normal but notable (a decoder syncing).
pub const INFO: Color32 = Color32::from_rgb(0x9f, 0xd3, 0xff);

/// The gold thread: the wordmark, and the active page's underline. Nothing else.
pub const HOUSE: Color32 = Color32::from_rgb(0xc9, 0xa8, 0x4c);

/// The default single data trace.
pub const TRACE: Color32 = Color32::from_rgb(0x5c, 0xc8, 0xe8);

/// Identity colours: a category, always drawn beside its name. No red, amber, green or magenta, so they can't be read as
/// a state.
pub const ID: [Color32; 8] = [
    Color32::from_rgb(0x5c, 0xc8, 0xe8),
    Color32::from_rgb(0xb0, 0x8c, 0xff),
    Color32::from_rgb(0xd8, 0xc9, 0xa3),
    Color32::from_rgb(0x3f, 0xb9, 0xa3),
    Color32::from_rgb(0xe6, 0xe7, 0xea),
    Color32::from_rgb(0xb5, 0xe0, 0x5a),
    Color32::from_rgb(0x7a, 0xa2, 0xff),
    Color32::from_rgb(0x8a, 0x8d, 0x93),
];
/// Known wideband occupants (FM broadcast, TV, LTE): identity violet.
pub const KNOWN: Color32 = ID[1];
/// Records' boxes on the waterfall: ink outlines; the selected one is the accent.
pub const RECORD: Color32 = INK2;

/// The label face (Barlow Semi Condensed 600): section titles, column headers, field keys, always uppercase.
pub fn label_family() -> FontFamily {
    FontFamily::Name("label".into())
}

/// Installs the house type: Barlow for the UI, Barlow Semi Condensed for labels, Auggie Mono for numbers and
/// identifiers. egui's own fonts stay behind each as fallbacks for glyphs the subsets lack (`⏷`, `▸`, `★`).
pub fn fonts(ctx: &egui::Context) {
    let mut f = egui::FontDefinitions::default();
    let add = |f: &mut egui::FontDefinitions, name: &str, bytes: &'static [u8]| {
        f.font_data.insert(name.into(), std::sync::Arc::new(egui::FontData::from_static(bytes)));
    };
    add(&mut f, "barlow", include_bytes!("../assets/fonts/Barlow-500.ttf"));
    add(&mut f, "barlow-sc", include_bytes!("../assets/fonts/BarlowSemiCondensed-600.ttf"));
    add(&mut f, "auggie-mono", include_bytes!("../assets/fonts/AuggieMono-400.ttf"));
    let fallbacks = f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mono_fallbacks = f.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let with = |first: &str, rest: &[String]| std::iter::once(first.to_string()).chain(rest.iter().cloned()).collect();
    f.families.insert(FontFamily::Proportional, with("barlow", &fallbacks));
    f.families.insert(FontFamily::Monospace, with("auggie-mono", &mono_fallbacks));
    f.families.insert(label_family(), with("barlow-sc", &fallbacks));
    ctx.set_fonts(f);
}

pub fn apply(ctx: &egui::Context) {
    fonts(ctx);
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, LINE);
    v.extreme_bg_color = WELL;
    v.faint_bg_color = HEADER;
    v.override_text_color = Some(INK);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = CAUTION;
    v.error_fg_color = ALARM;
    for w in [&mut v.widgets.inactive, &mut v.widgets.noninteractive] {
        w.bg_fill = BUTTON;
        w.weak_bg_fill = BUTTON;
        w.bg_stroke = Stroke::new(1.0, LINE);
        w.corner_radius = CornerRadius::same(4);
    }
    v.widgets.noninteractive.weak_bg_fill = PANEL;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, INK);
    // The hovered and pressed rings are neutral (C5): only a selected control is the accent.
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x33, 0x35, 0x3b);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x33, 0x35, 0x3b);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, INK2);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x3a, 0x3c, 0x42);
    v.widgets.active.bg_fill = Color32::from_rgb(0x3a, 0x3c, 0x42);
    v.widgets.active.bg_stroke = Stroke::new(1.0, INK);
    v.widgets.active.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.open.fg_stroke = Stroke::new(1.0, INK);
    // Always dark, regardless of the OS light/dark setting.
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 5.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
        s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(13.5));
        s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(13.5));
        s.text_styles.insert(egui::TextStyle::Small, FontId::proportional(11.5));
        s.text_styles.insert(egui::TextStyle::Monospace, FontId::monospace(12.5));
        s.text_styles.insert(egui::TextStyle::Heading, FontId::new(15.0, label_family()));
    });
}

/// A label in the label face: uppercase, tracked 0.08 em, ink-2 (field keys, column headers, section titles).
pub fn label(text: &str) -> RichText {
    let size = 11.5;
    RichText::new(text.to_uppercase()).family(label_family()).size(size).extra_letter_spacing(0.08 * size).color(INK2)
}

/// A value with its unit: the value in Auggie Mono and ink, then a thin space, then the unit one step smaller in ink-2
/// (`851.0125 MHz`, `−3.6 dB`). The unit is always there and always quieter than the value.
pub fn value_unit(value: &str, unit: &str) -> LayoutJob {
    value_unit_in(value, unit, INK)
}

/// [`value_unit`] with the value in another colour (a selected row's accent).
pub fn value_unit_in(value: &str, unit: &str, ink: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(value, 0.0, TextFormat::simple(FontId::monospace(12.5), ink));
    if !unit.is_empty() {
        // A thin space as a gap: neither face has U+2009, and egui's fallbacks draw it at no width.
        job.append(unit, 3.0, TextFormat::simple(FontId::proportional(11.5), INK2));
    }
    job
}

/// The top bar: the header surface with a hairline under it.
pub fn bar_frame() -> Frame {
    Frame::new().fill(HEADER).inner_margin(Margin::symmetric(12, 4)).stroke(Stroke::new(1.0, LINE))
}

/// The status bar under the top bar.
pub fn status_frame() -> Frame {
    Frame::new().fill(BG).inner_margin(Margin::symmetric(12, 3)).stroke(Stroke::new(1.0, LINE))
}

/// The icon rail: a bar with no side margin so 36 px fits an icon.
pub fn rail_frame() -> Frame {
    Frame::new().fill(HEADER).inner_margin(Margin::symmetric(0, 4)).stroke(Stroke::new(1.0, LINE))
}

pub fn central_frame() -> Frame {
    Frame::new().fill(BG).inner_margin(Margin::same(6))
}

pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(6)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A small chip: an identifier in Auggie Mono on a fill.
pub fn chip(ui: &mut egui::Ui, text: &str, bg: Color32, fg: Color32) {
    Frame::new().fill(bg).corner_radius(3).inner_margin(Margin::symmetric(6, 1)).show(ui, |ui| {
        ui.label(RichText::new(text).monospace().size(11.0).color(fg));
    });
}

/// A lamp: a painted dot, filled when on, a ring when off (a shape, so the state reads without its colour; and egui's
/// bundled fonts have no `●`).
pub fn dot(ui: &mut egui::Ui, colour: Color32, filled: bool) -> egui::Response {
    let size = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size * 0.8, size), egui::Sense::hover());
    let r = size * 0.27;
    if filled {
        ui.painter().circle_filled(rect.center(), r, colour);
    } else {
        ui.painter().circle_stroke(rect.center(), r - 0.5, Stroke::new(1.2, colour));
    }
    resp
}

/// The wordmark, top left, in the house gold.
pub fn wordmark(ui: &mut egui::Ui) {
    let size = 15.0;
    ui.label(RichText::new("SPECTRAWL").family(label_family()).size(size).extra_letter_spacing(0.12 * size).color(HOUSE));
}

/// The colour map stops: inferno at every tenth (the Instrument look's five stops are among them), perceptually
/// ordered (C9). Interpolating the guide's five alone lifts the noise floor to a bright purple. The floor and ceiling
/// are the receiver's dB range.
pub const CMAP: [[u8; 3]; 11] = [
    [0x00, 0x00, 0x04],
    [0x16, 0x0b, 0x39],
    [0x42, 0x0a, 0x68],
    [0x6a, 0x17, 0x6e],
    [0x93, 0x26, 0x67],
    [0xbc, 0x37, 0x54],
    [0xdd, 0x51, 0x3a],
    [0xf3, 0x78, 0x19],
    [0xfc, 0xa5, 0x0a],
    [0xf6, 0xd7, 0x46],
    [0xfc, 0xff, 0xa4],
];

/// The waterfall's 256-entry colour map, from [`CMAP`].
pub fn waterfall_lut() -> [Color32; 256] {
    let n = CMAP.len() - 1;
    let mut lut = [Color32::BLACK; 256];
    for (i, c) in lut.iter_mut().enumerate() {
        let v = i as f32 / 255.0 * n as f32;
        let k = (v.floor() as usize).min(n - 1);
        let t = v - k as f32;
        let (a, b) = (CMAP[k], CMAP[k + 1]);
        let ch = |j: usize| (a[j] as f32 + (b[j] as f32 - a[j] as f32) * t).round() as u8;
        *c = Color32::from_rgb(ch(0), ch(1), ch(2));
    }
    lut
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance.
    fn lum(c: Color32) -> f64 {
        let f = |v: u8| {
            let s = v as f64 / 255.0;
            if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn text_meets_the_guides_contrast_floors_on_every_surface() {
        // T1: body text >= 4.5:1; state indicators >= 3:1.
        for ground in [BG, PANEL, HEADER, WELL, BUTTON, CHIP] {
            for ink in [INK, INK2] {
                assert!(contrast(ink, ground) >= 4.5, "{ink:?} on {ground:?}: {:.2}", contrast(ink, ground));
            }
            for state in [ACCENT, CAUTION, ALARM, OK, LIVE, INFO, HOUSE] {
                assert!(contrast(state, ground) >= 3.0, "{state:?} on {ground:?}: {:.2}", contrast(state, ground));
            }
        }
        assert!(contrast(ON_ACCENT, ACCENT) >= 4.5);
    }

    #[test]
    fn identity_colours_never_borrow_a_state_colour() {
        for id in ID {
            for state in [ACCENT, CAUTION, ALARM, OK, LIVE, INFO, HOUSE] {
                assert_ne!(id, state);
            }
        }
    }

    #[test]
    fn the_colour_map_rises_in_lightness_from_floor_to_ceiling() {
        // C9: perceptually ordered. Luminance must never fall along the map.
        let lut = waterfall_lut();
        assert_eq!(lut[0], Color32::from_rgb(0, 0, 4));
        assert_eq!(lut[255], Color32::from_rgb(0xfc, 0xff, 0xa4));
        for w in lut.windows(2) {
            assert!(lum(w[1]) + 1e-4 >= lum(w[0]), "{:?} then {:?}", w[0], w[1]);
        }
    }
}
