//! Human-facing terminal rendering and optional interactive menu.

mod menu;
mod render;
mod text;

pub(super) use menu::menu;
pub(super) use render::{render, render_scan};
