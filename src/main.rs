#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod pdf_engine;
mod theme;
mod viewer;

use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{
    App, AppContext, Application, AssetSource, Bounds, Focusable, KeyBinding, SharedString, TitlebarOptions,
    WindowBounds, WindowOptions, px, size,
};

use viewer::*;

/// SVG icons embedded into the binary.
struct Assets;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        fn icon_bytes(path: &str) -> Option<&'static [u8]> {
            match path {
                $(concat!("icons/", $name, ".svg") => {
                    Some(include_bytes!(concat!("../assets/icons/", $name, ".svg")))
                })*
                _ => None,
            }
        }
        const ICON_NAMES: &[&str] = &[$(concat!("icons/", $name, ".svg")),*];
    };
}

icons!(
    "alert", "chevron-left", "chevron-right", "close", "file", "fit-page", "fit-width",
    "folder-open", "rotate-ccw", "rotate-cw", "scroll-mode", "sidebar", "single-mode",
    "zoom-in", "zoom-out",
);

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(icon_bytes(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(ICON_NAMES
            .iter()
            .filter(|name| name.starts_with(path))
            .map(|name| SharedString::from(*name))
            .collect())
    }
}

fn key_bindings() -> Vec<KeyBinding> {
    let ctx = Some("Viewer");
    vec![
        KeyBinding::new("ctrl-o", OpenFile, ctx),
        KeyBinding::new("ctrl-w", CloseFile, ctx),
        KeyBinding::new("ctrl-=", ZoomIn, ctx),
        KeyBinding::new("ctrl-shift-=", ZoomIn, ctx),
        KeyBinding::new("ctrl--", ZoomOut, ctx),
        KeyBinding::new("ctrl-1", ActualSize, ctx),
        KeyBinding::new("ctrl-2", FitWidth, ctx),
        KeyBinding::new("ctrl-0", FitPage, ctx),
        KeyBinding::new("right", NextPage, ctx),
        KeyBinding::new("left", PrevPage, ctx),
        KeyBinding::new("pagedown", NextPage, ctx),
        KeyBinding::new("pageup", PrevPage, ctx),
        KeyBinding::new("home", FirstPage, ctx),
        KeyBinding::new("end", LastPage, ctx),
        KeyBinding::new("ctrl-home", FirstPage, ctx),
        KeyBinding::new("ctrl-end", LastPage, ctx),
        KeyBinding::new("up", ScrollUp, ctx),
        KeyBinding::new("down", ScrollDown, ctx),
        KeyBinding::new("shift-left", ScrollLeft, ctx),
        KeyBinding::new("shift-right", ScrollRight, ctx),
        KeyBinding::new("space", ScreenDown, ctx),
        KeyBinding::new("shift-space", ScreenUp, ctx),
        KeyBinding::new("ctrl-r", RotateClockwise, ctx),
        KeyBinding::new("ctrl-shift-r", RotateCounterClockwise, ctx),
        KeyBinding::new("f4", ToggleSidebar, ctx),
        KeyBinding::new("ctrl-m", ToggleViewMode, ctx),
        KeyBinding::new("ctrl-g", GoToPage, ctx),
    ]
}

fn main() {
    let initial_file = std::env::args_os().nth(1).map(PathBuf::from);

    Application::new().with_assets(Assets).run(move |cx: &mut App| {
        cx.bind_keys(key_bindings());

        let bounds = Bounds::centered(None, size(px(1180.0), px(860.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("PDF Viewer".into()),
                ..Default::default()
            }),
            window_min_size: Some(size(px(640.0), px(420.0))),
            ..Default::default()
        };

        let window = cx
            .open_window(options, |window, cx| {
                let view = cx.new(Viewer::new);
                let focus = view.read(cx).focus_handle(cx);
                window.focus(&focus);
                if let Some(path) = initial_file.clone() {
                    view.update(cx, |viewer, cx| viewer.open_path(path, cx));
                }
                view
            })
            .expect("failed to open window");
        let _ = window;

        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
