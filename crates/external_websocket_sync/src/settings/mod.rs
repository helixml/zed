//! Settings module for Helix integration

mod helix_settings;

pub use helix_settings::*;

/// Initialize all helix integration settings
pub fn init(cx: &mut gpui::AppContext) {
    helix_settings::init(cx);
}