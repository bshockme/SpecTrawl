//! Workspaces (docs/ui-design.md §6, the guide's L3 and L8): three designed presets, layouts the user saved by name,
//! and the layout last used, all kept in the app's settings. Receiver tabs are left out of a saved layout (a receiver's
//! id lasts one run); the receivers open now are placed back when one is loaded.

use std::collections::BTreeMap;

use egui_dock::{DockState, NodeIndex};
use serde_json::Value;

use crate::tabs::Tab;

/// The presets, in workflow order: find signals, follow systems, read pages.
pub const PRESETS: [&str; 3] = ["Survey", "Trunking", "Paging"];

/// The Devices column's share of the window: wide enough for a row with `open as A` and `Close`.
pub const DEVICES_SHARE: f32 = 0.3;
/// Devices' share of the left column; the inspector takes the rest.
pub const INSPECTOR_SPLIT: f32 = 0.4;

/// The tables under the receivers, in the order their tabs show.
pub const TABLES: [Tab; 7] = [Tab::Log, Tab::Records, Tab::Emitters, Tab::Systems, Tab::Channels, Tab::Pages, Tab::Catalogue];

/// The default layout: Devices over the inspector at the left, the tables at the right (receivers open above them),
/// with `front` in front.
pub fn layout(front: &Tab) -> DockState<Tab> {
    let mut dock = DockState::new(vec![Tab::Devices]);
    let [left, _] = dock.main_surface_mut().split_right(NodeIndex::root(), DEVICES_SHARE, TABLES.to_vec());
    dock.main_surface_mut().split_below(left, INSPECTOR_SPLIT, vec![Tab::Inspector]);
    if let Some(path) = dock.find_tab(front) {
        let _ = dock.set_active_tab(path);
    }
    dock
}

/// A preset's layout: the default with the table its job needs in front.
pub fn preset(name: &str) -> Option<DockState<Tab>> {
    let front = match name {
        "Survey" => Tab::Records,
        "Trunking" => Tab::Systems,
        "Paging" => Tab::Pages,
        _ => return None,
    };
    Some(layout(&front))
}

/// A layout as saved: receiver tabs left out.
pub fn save(dock: &DockState<Tab>) -> Option<Value> {
    let mut d = dock.clone();
    while let Some(path) = d.find_tab_from(|t| matches!(t, Tab::Receiver(_))) {
        d.remove_tab(path);
    }
    serde_json::to_value(&d).map_err(|e| log::warn!("could not save the layout: {e}")).ok()
}

/// A saved layout, if it still reads. egui_dock saves rects it hasn't laid out yet as NaN, which JSON writes as null
/// and can't read back as a number; they are recomputed on the first frame, so a null coordinate reads as 0.
pub fn load(v: &Value) -> Option<DockState<Tab>> {
    fn unnull(v: &mut Value) {
        match v {
            Value::Object(m) => {
                if m.contains_key("x") && m.contains_key("y") {
                    for k in ["x", "y"] {
                        if m[k].is_null() {
                            m[k] = serde_json::json!(0.0);
                        }
                    }
                }
                m.values_mut().for_each(unnull);
            }
            Value::Array(a) => a.iter_mut().for_each(unnull),
            _ => {}
        }
    }
    let mut v = v.clone();
    unnull(&mut v);
    serde_json::from_value(v).map_err(|e| log::warn!("a saved layout no longer reads: {e}")).ok()
}

/// What is kept between runs.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Saved {
    /// Layouts saved by name.
    #[serde(default)]
    pub named: BTreeMap<String, Value>,
    /// The layout in use when the app last closed, and the page it was.
    #[serde(default)]
    pub last: Option<Value>,
    #[serde(default)]
    pub active: Option<String>,
}

/// Whether `tab` is in front in its node.
pub fn in_front(dock: &DockState<Tab>, tab: &Tab) -> bool {
    dock.main_surface().find_tab(tab).is_some_and(|(node, i)| dock.main_surface()[node].get_leaf().is_some_and(|leaf| leaf.active == i))
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::SourceId;

    #[test]
    fn a_saved_layout_comes_back_without_its_receivers() {
        let mut dock = preset("Paging").unwrap();
        let (node, _) = dock.main_surface().find_tab(&Tab::Log).unwrap();
        dock.main_surface_mut().split_above(node, 0.7, vec![Tab::Receiver(SourceId(3))]);
        let v = save(&dock).unwrap();
        // Through text, as the settings store it.
        let back = load(&serde_json::from_str(&v.to_string()).unwrap()).unwrap();
        assert!(back.find_tab(&Tab::Receiver(SourceId(3))).is_none());
        for t in TABLES.iter().chain([&Tab::Devices, &Tab::Inspector]) {
            assert!(back.find_tab(t).is_some(), "{t:?} is gone");
        }
        assert!(in_front(&back, &Tab::Pages));
    }

    #[test]
    fn every_preset_has_its_table_in_front() {
        for (name, front) in [("Survey", Tab::Records), ("Trunking", Tab::Systems), ("Paging", Tab::Pages)] {
            assert!(in_front(&preset(name).unwrap(), &front), "{name}");
        }
        assert!(preset("nope").is_none());
    }
}
