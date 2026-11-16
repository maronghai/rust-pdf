//! Colors and metrics for the viewer UI (dark theme).

use gpui::{Rgba, rgb, rgba};

pub fn app_bg() -> Rgba {
    rgb(0x17181c)
}
pub fn toolbar_bg() -> Rgba {
    rgb(0x1f2126)
}
pub fn sidebar_bg() -> Rgba {
    rgb(0x1b1d21)
}
pub fn canvas_bg() -> Rgba {
    rgb(0x2a2c31)
}
pub fn border() -> Rgba {
    rgb(0x2f3238)
}
pub fn hover_bg() -> Rgba {
    rgb(0x30333a)
}
pub fn active_bg() -> Rgba {
    rgb(0x383c44)
}
pub fn field_bg() -> Rgba {
    rgb(0x16171a)
}
pub fn text() -> Rgba {
    rgb(0xe4e6eb)
}
pub fn text_muted() -> Rgba {
    rgb(0x9a9fa8)
}
pub fn text_disabled() -> Rgba {
    rgb(0x5c6068)
}
pub fn accent() -> Rgba {
    rgb(0x4c8dff)
}
pub fn accent_soft() -> Rgba {
    rgba(0x4c8dff2e)
}
pub fn danger_bg() -> Rgba {
    rgb(0x3a2226)
}
pub fn danger_border() -> Rgba {
    rgb(0x7a3540)
}
pub fn paper() -> Rgba {
    rgb(0xffffff)
}
pub fn placeholder() -> Rgba {
    rgb(0xf1f2f4)
}
pub fn page_shadow() -> Rgba {
    rgba(0x0000008c)
}
pub fn scrollbar_thumb() -> Rgba {
    rgba(0xffffff38)
}
pub fn scrollbar_thumb_active() -> Rgba {
    rgba(0xffffff66)
}
pub fn tooltip_bg() -> Rgba {
    rgb(0x0f1013)
}

pub const UI_FONT: &str = "Segoe UI";
pub const TOOLBAR_HEIGHT: f32 = 46.0;
pub const STATUS_HEIGHT: f32 = 26.0;
pub const SIDEBAR_WIDTH: f32 = 196.0;
pub const SCROLLBAR_SIZE: f32 = 10.0;
