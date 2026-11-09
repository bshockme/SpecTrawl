//! SpecTrawl's interface on the new engine: it renders the engine's published
//! [`api::Snapshot`]s and sends [`api::Command`]s back through an [`api::Handle`].
//! It depends on `api`, not on `engine`: the launcher (`crates/launcher`) starts
//! the engine and hands this crate the handle, so nothing here can reach engine
//! state or wait on it (the handle's two calls never block).

pub mod app;
pub mod catalogue_pane;
pub mod channels_pane;
pub mod devices_pane;
pub mod emitters_pane;
pub mod health;
pub mod inspector;
pub mod log_pane;
pub mod pages_pane;
pub mod palette;
pub mod receiver_pane;
pub mod records_pane;
pub mod selection;
pub mod status_bar;
pub mod systems_pane;
pub mod tabs;
pub mod theme;
pub mod trace;
pub mod waterfall;
pub mod workspace;
