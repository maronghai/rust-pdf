//! The main viewer window: toolbar, thumbnail sidebar, page canvas and status bar.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, AnyView, App, BoxShadow, ClickEvent, Context, CursorStyle, ExternalPaths,
    FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, Pixels, Point, RenderImage, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, SharedString, Size, Stateful, Div, Task, Timer, Window, actions, div, img,
    point, prelude::*, px, size, svg,
};

use crate::pdf_engine::{
    DocumentInfo, JobKey, JobKind, PageSize, RenderOutcome, RenderPool, RenderResult,
    load_document,
};
use crate::theme::{self, SCROLLBAR_SIZE, SIDEBAR_WIDTH, STATUS_HEIGHT, TOOLBAR_HEIGHT};

actions!(
    pdf_viewer,
    [
        OpenFile,
        CloseFile,
        ZoomIn,
        ZoomOut,
        ActualSize,
        FitWidth,
        FitPage,
        NextPage,
        PrevPage,
        FirstPage,
        LastPage,
        ScrollUp,
        ScrollDown,
        ScrollLeft,
        ScrollRight,
        ScreenDown,
        ScreenUp,
        RotateClockwise,
        RotateCounterClockwise,
        ToggleSidebar,
        ToggleViewMode,
        GoToPage,
    ]
);

/// CSS pixels per PDF point at 100 % zoom (96 dpi / 72 dpi).
const PX_PER_PT: f32 = 96.0 / 72.0;
const PAGE_PADDING: f32 = 28.0;
const PAGE_GAP: f32 = 20.0;
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.0;
const ZOOM_STEPS: &[f32] = &[
    0.1, 0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0,
    5.0, 6.0, 8.0,
];
/// Wait this long after the last zoom change before re-rendering at the new scale.
const RERENDER_DELAY: Duration = Duration::from_millis(140);
const LINE_SCROLL: f32 = 64.0;

const THUMB_MAX_W: f32 = 128.0;
const THUMB_MAX_H: f32 = 170.0;
const THUMB_LABEL_H: f32 = 22.0;
const THUMB_SPACING: f32 = 12.0;
const THUMB_TOP: f32 = 14.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FitMode {
    None,
    Width,
    Page,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Continuous,
    Single,
}

struct Bitmap {
    image: Arc<RenderImage>,
    key: JobKey,
}

/// Pages laid out in canvas coordinates (logical pixels).
struct Layout {
    /// Index of the first page in this layout (non-zero in single page mode).
    first: usize,
    tops: Vec<f32>,
    sizes: Vec<(f32, f32)>,
    content_w: f32,
    content_h: f32,
}

impl Layout {
    fn contains(&self, page: usize) -> bool {
        page >= self.first && page < self.first + self.tops.len()
    }

    fn page_rect(&self, page: usize) -> (f32, f32, f32, f32) {
        let ix = page - self.first;
        let (w, h) = self.sizes[ix];
        ((self.content_w - w) / 2.0, self.tops[ix], w, h)
    }

    /// The page at canvas height `y` (closest one when `y` lies in a gap).
    fn page_at(&self, y: f32) -> usize {
        let ix = self.tops.partition_point(|&top| top <= y);
        self.first + ix.saturating_sub(1).min(self.tops.len().saturating_sub(1))
    }

    /// Pages intersecting the vertical span `[y0, y1)`.
    fn pages_in(&self, y0: f32, y1: f32) -> Range<usize> {
        let start = self
            .tops
            .iter()
            .zip(&self.sizes)
            .position(|(&top, &(_, h))| top + h > y0)
            .unwrap_or(self.tops.len());
        let end = self.tops.partition_point(|&top| top < y1).max(start);
        self.first + start..self.first + end
    }
}

struct ScrollbarDrag {
    grab: f32,
}

struct Pan {
    mouse: Point<Pixels>,
    offset: Point<Pixels>,
}

pub struct Viewer {
    focus: FocusHandle,
    scroll: ScrollHandle,
    thumb_scroll: ScrollHandle,
    pool: RenderPool,
    next_doc_id: u64,
    doc: Option<DocumentInfo>,
    loading: Option<String>,
    error: Option<String>,
    zoom: f32,
    fit: FitMode,
    rotation: u32,
    view_mode: ViewMode,
    sidebar: bool,
    current_page: usize,
    pages: HashMap<usize, Bitmap>,
    thumbs: HashMap<usize, Bitmap>,
    in_flight: HashSet<JobKey>,
    failed: HashSet<JobKey>,
    to_drop: Vec<Arc<RenderImage>>,
    pending_scroll: Option<Point<Pixels>>,
    expected_offset: Option<Point<Pixels>>,
    last_scale_milli: u32,
    scale_changed_at: Instant,
    thumb_synced_page: Option<usize>,
    goto_input: Option<String>,
    pan: Option<Pan>,
    scrollbar_drag: Option<ScrollbarDrag>,
    last_render_ms: Option<f32>,
    last_title: String,
    _tasks: Vec<Task<()>>,
}

impl Focusable for Viewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Viewer {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded::<RenderResult>();
        let pool = RenderPool::new(tx);
        let listener = cx.spawn(async move |this, cx| {
            while let Ok(result) = rx.recv().await {
                if this.update(cx, |this, cx| this.on_rendered(result, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            thumb_scroll: ScrollHandle::new(),
            pool,
            next_doc_id: 0,
            doc: None,
            loading: None,
            error: None,
            zoom: 1.0,
            fit: FitMode::Width,
            rotation: 0,
            view_mode: ViewMode::Continuous,
            sidebar: true,
            current_page: 0,
            pages: HashMap::new(),
            thumbs: HashMap::new(),
            in_flight: HashSet::new(),
            failed: HashSet::new(),
            to_drop: Vec::new(),
            pending_scroll: None,
            expected_offset: None,
            last_scale_milli: 0,
            scale_changed_at: Instant::now(),
            thumb_synced_page: None,
            goto_input: None,
            pan: None,
            scrollbar_drag: None,
            last_render_ms: None,
            last_title: String::new(),
            _tasks: vec![listener],
        }
    }

    // -----------------------------------------------------------------------
    // Documents
    // -----------------------------------------------------------------------

    pub fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.next_doc_id += 1;
        let id = self.next_doc_id;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.loading = Some(name);
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_document(id, &path) })
                .await;
            this.update(cx, |this, cx| this.finish_open(id, result, cx)).ok();
        })
        .detach();
    }

    fn finish_open(&mut self, id: u64, result: Result<DocumentInfo, String>, cx: &mut Context<Self>) {
        if id != self.next_doc_id {
            return; // a newer open request superseded this one
        }
        self.loading = None;
        match result {
            Ok(doc) => {
                self.release_bitmaps();
                self.pool.set_document(&doc);
                self.doc = Some(doc);
                self.current_page = 0;
                self.rotation = 0;
                if self.fit == FitMode::None {
                    self.fit = FitMode::Width;
                }
                self.pending_scroll = Some(point(px(0.0), px(0.0)));
                self.thumb_scroll.set_offset(point(px(0.0), px(0.0)));
                self.thumb_synced_page = None;
                self.last_render_ms = None;
                self.goto_input = None;
            }
            Err(message) => self.error = Some(message),
        }
        cx.notify();
    }

    fn close_file(&mut self, cx: &mut Context<Self>) {
        self.next_doc_id += 1;
        self.release_bitmaps();
        self.pool.clear_document();
        self.doc = None;
        self.loading = None;
        self.goto_input = None;
        cx.notify();
    }

    fn release_bitmaps(&mut self) {
        self.to_drop.extend(self.pages.drain().map(|(_, b)| b.image));
        self.to_drop.extend(self.thumbs.drain().map(|(_, b)| b.image));
        self.in_flight.clear();
        self.failed.clear();
    }

    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Open PDF")
            .add_filter("PDF Documents", &["pdf"])
            .add_filter("All Files", &["*"])
            .pick_file();
        cx.spawn(async move |this, cx| {
            if let Some(file) = dialog.await {
                let path = file.path().to_path_buf();
                this.update(cx, |this, cx| this.open_path(path, cx)).ok();
            }
        })
        .detach();
    }

    fn on_rendered(&mut self, result: RenderResult, cx: &mut Context<Self>) {
        let key = result.key;
        self.in_flight.remove(&key);
        let current_doc = self.doc.as_ref().map(|d| d.id);
        match result.outcome {
            RenderOutcome::Done { image, millis } => {
                if current_doc != Some(key.doc_id) {
                    self.to_drop.push(image);
                    return;
                }
                let map = match key.kind {
                    JobKind::Page => {
                        self.last_render_ms = Some(millis);
                        &mut self.pages
                    }
                    JobKind::Thumbnail => &mut self.thumbs,
                };
                if let Some(old) = map.insert(key.page, Bitmap { image, key }) {
                    self.to_drop.push(old.image);
                }
                cx.notify();
            }
            RenderOutcome::Failed(message) => {
                #[cfg(debug_assertions)]
                eprintln!("page {} failed to render: {message}", key.page + 1);
                let _ = message;
                self.failed.insert(key);
                cx.notify();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Geometry
    // -----------------------------------------------------------------------

    fn viewport(&self, window: &Window) -> Size<f32> {
        let win = window.viewport_size();
        let sidebar = if self.sidebar && self.doc.is_some() { SIDEBAR_WIDTH } else { 0.0 };
        size(
            (f32::from(win.width) - sidebar).max(1.0),
            (f32::from(win.height) - TOOLBAR_HEIGHT - STATUS_HEIGHT).max(1.0),
        )
    }

    fn page_size(&self, page: usize) -> PageSize {
        self.doc
            .as_ref()
            .and_then(|d| d.pages.get(page).copied())
            .unwrap_or(PageSize { width: 612.0, height: 792.0 })
            .rotated(self.rotation)
    }

    /// The zoom actually used for layout, resolving fit modes.
    fn effective_zoom(&self, viewport: Size<f32>) -> f32 {
        let Some(doc) = &self.doc else { return self.zoom };
        let avail_w = (viewport.width - 2.0 * PAGE_PADDING - SCROLLBAR_SIZE).max(40.0);
        let avail_h = (viewport.height - 2.0 * PAGE_PADDING).max(40.0);
        let zoom = match self.fit {
            FitMode::None => self.zoom,
            FitMode::Width => {
                let widest = doc
                    .pages
                    .iter()
                    .map(|p| p.rotated(self.rotation).width)
                    .fold(1.0f32, f32::max);
                avail_w / (widest * PX_PER_PT)
            }
            FitMode::Page => {
                let page = self.page_size(self.current_page);
                (avail_w / (page.width * PX_PER_PT)).min(avail_h / (page.height * PX_PER_PT))
            }
        };
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    }

    fn layout(&self, viewport: Size<f32>, zoom: f32) -> Layout {
        let Some(doc) = &self.doc else {
            return Layout { first: 0, tops: vec![], sizes: vec![], content_w: 0.0, content_h: 0.0 };
        };
        let scale = zoom * PX_PER_PT;
        let range = match self.view_mode {
            ViewMode::Continuous => 0..doc.pages.len(),
            ViewMode::Single => self.current_page..self.current_page + 1,
        };
        let mut tops = Vec::with_capacity(range.len());
        let mut sizes = Vec::with_capacity(range.len());
        let mut y = PAGE_PADDING;
        let mut max_w: f32 = 0.0;
        for page in range.clone() {
            let s = doc.pages[page].rotated(self.rotation);
            let (w, h) = ((s.width * scale).round(), (s.height * scale).round());
            tops.push(y);
            sizes.push((w, h));
            y += h + PAGE_GAP;
            max_w = max_w.max(w);
        }
        let mut content_h = y - PAGE_GAP + PAGE_PADDING;
        if self.view_mode == ViewMode::Single && content_h < viewport.height {
            // Centre a single small page vertically.
            let shift = (viewport.height - content_h) / 2.0;
            for top in &mut tops {
                *top += shift;
            }
            content_h = viewport.height;
        }
        Layout {
            first: range.start,
            tops,
            sizes,
            content_w: (max_w + 2.0 * PAGE_PADDING).max(viewport.width),
            content_h,
        }
    }

    fn clamp_offset(&self, offset: Point<Pixels>, layout: &Layout, viewport: Size<f32>) -> Point<Pixels> {
        let max_x = (layout.content_w - viewport.width).max(0.0);
        let max_y = (layout.content_h - viewport.height).max(0.0);
        point(
            px(f32::from(offset.x).clamp(-max_x, 0.0)),
            px(f32::from(offset.y).clamp(-max_y, 0.0)),
        )
    }

    fn current_offset(&self) -> Point<Pixels> {
        self.pending_scroll.unwrap_or_else(|| self.scroll.offset())
    }

    fn set_offset(&mut self, offset: Point<Pixels>, window: &Window) {
        let viewport = self.viewport(window);
        let layout = self.layout(viewport, self.effective_zoom(viewport));
        self.pending_scroll = Some(self.clamp_offset(offset, &layout, viewport));
    }

    /// Apply a change that affects the layout while keeping the document point
    /// under `focus` (viewport coordinates, default: centre) in place.
    fn relayout(
        &mut self,
        focus: Option<Point<f32>>,
        window: &Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Self),
    ) {
        let viewport = self.viewport(window);
        let old_layout = self.layout(viewport, self.effective_zoom(viewport));
        let offset = self.current_offset();
        let focus = focus.unwrap_or(point(viewport.width / 2.0, viewport.height / 2.0));
        let doc_x = focus.x - f32::from(offset.x);
        let doc_y = focus.y - f32::from(offset.y);

        let anchor = (!old_layout.tops.is_empty()).then(|| {
            let page = old_layout.page_at(doc_y);
            let (x, top, w, h) = old_layout.page_rect(page);
            (page, (doc_x - (x + w / 2.0)) / w, (doc_y - top) / h)
        });

        change(self);

        let viewport = self.viewport(window);
        let new_layout = self.layout(viewport, self.effective_zoom(viewport));
        let new_offset = match anchor {
            Some((page, fx, fy)) if new_layout.contains(page) => {
                let (x, top, w, h) = new_layout.page_rect(page);
                point(px(focus.x - (x + w / 2.0 + fx * w)), px(focus.y - (top + fy * h)))
            }
            _ => point(px(0.0), px(0.0)),
        };
        self.pending_scroll = Some(self.clamp_offset(new_offset, &new_layout, viewport));
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    fn set_zoom(&mut self, zoom: f32, focus: Option<Point<f32>>, window: &Window, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.relayout(focus, window, cx, |this| {
            this.zoom = zoom;
            this.fit = FitMode::None;
        });
    }

    fn zoom_step(&mut self, direction: i32, window: &Window, cx: &mut Context<Self>) {
        let viewport = self.viewport(window);
        let current = self.effective_zoom(viewport);
        let next = if direction > 0 {
            ZOOM_STEPS.iter().copied().find(|&z| z > current * 1.001).unwrap_or(MAX_ZOOM)
        } else {
            ZOOM_STEPS.iter().rev().copied().find(|&z| z < current * 0.999).unwrap_or(MIN_ZOOM)
        };
        self.set_zoom(next, None, window, cx);
    }

    fn set_fit(&mut self, fit: FitMode, window: &Window, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        let page = self.current_page;
        self.relayout(None, window, cx, |this| this.fit = fit);
        if fit == FitMode::Page {
            self.go_to_page(page, window, cx);
        }
    }

    fn rotate(&mut self, clockwise: bool, window: &Window, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        self.relayout(None, window, cx, |this| {
            this.rotation = if clockwise { (this.rotation + 90) % 360 } else { (this.rotation + 270) % 360 };
        });
    }

    fn toggle_view_mode(&mut self, window: &Window, cx: &mut Context<Self>) {
        let page = self.current_page;
        self.view_mode = match self.view_mode {
            ViewMode::Continuous => ViewMode::Single,
            ViewMode::Single => ViewMode::Continuous,
        };
        self.go_to_page(page, window, cx);
    }

    fn toggle_sidebar(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.relayout(None, window, cx, |this| this.sidebar = !this.sidebar);
        self.thumb_synced_page = None;
    }

    fn page_count(&self) -> usize {
        self.doc.as_ref().map_or(0, |d| d.pages.len())
    }

    fn go_to_page(&mut self, page: usize, window: &Window, cx: &mut Context<Self>) {
        let count = self.page_count();
        if count == 0 {
            return;
        }
        let page = page.min(count - 1);
        self.current_page = page;
        let viewport = self.viewport(window);
        let layout = self.layout(viewport, self.effective_zoom(viewport));
        let offset_x = self.current_offset().x;
        let y = match self.view_mode {
            ViewMode::Continuous => layout.page_rect(page).1 - PAGE_PADDING / 2.0,
            ViewMode::Single => 0.0,
        };
        let target = self.clamp_offset(point(offset_x, px(-y.max(0.0))), &layout, viewport);
        self.pending_scroll = Some(target);
        self.expected_offset = Some(target);
        cx.notify();
    }

    fn scroll_by(&mut self, dx: f32, dy: f32, window: &Window, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        let viewport = self.viewport(window);
        let layout = self.layout(viewport, self.effective_zoom(viewport));
        let offset = self.current_offset();
        let max_y = (layout.content_h - viewport.height).max(0.0);
        let y = f32::from(offset.y);
        if self.view_mode == ViewMode::Single {
            // Flip pages when scrolling past the edge of the single page.
            if dy > 0.0 && -y >= max_y - 0.5 && self.current_page + 1 < self.page_count() {
                return self.go_to_page(self.current_page + 1, window, cx);
            }
            if dy < 0.0 && y >= -0.5 && self.current_page > 0 {
                self.go_to_page(self.current_page - 1, window, cx);
                let viewport = self.viewport(window);
                let layout = self.layout(viewport, self.effective_zoom(viewport));
                let bottom = point(offset.x, px(-(layout.content_h - viewport.height).max(0.0)));
                self.pending_scroll = Some(self.clamp_offset(bottom, &layout, viewport));
                return;
            }
        }
        let target = point(offset.x - px(dx), offset.y - px(dy));
        self.pending_scroll = Some(self.clamp_offset(target, &layout, viewport));
        cx.notify();
    }

    fn commit_goto(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(text) = self.goto_input.take() {
            if let Ok(n) = text.trim().parse::<usize>() {
                if n >= 1 {
                    self.go_to_page(n - 1, window, cx);
                }
            }
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.goto_input.as_mut() else { return };
        let key = event.keystroke.key.as_str();
        match key {
            "enter" => self.commit_goto(window, cx),
            "escape" => self.goto_input = None,
            "backspace" => {
                input.pop();
            }
            k if k.len() == 1 && k.chars().all(|c| c.is_ascii_digit()) => {
                if input.len() < 6 {
                    input.push_str(k);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_canvas_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !event.modifiers.control || self.doc.is_none() {
            return;
        }
        let steps = match event.delta {
            ScrollDelta::Lines(lines) => lines.y,
            ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 50.0,
        };
        if steps == 0.0 {
            return;
        }
        let viewport = self.viewport(window);
        let zoom = self.effective_zoom(viewport) * 1.12f32.powf(steps);
        let origin = self.scroll.bounds().origin;
        let focus = point(
            f32::from(event.position.x - origin.x),
            f32::from(event.position.y - origin.y),
        );
        self.set_zoom(zoom, Some(focus), window, cx);
        cx.stop_propagation();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pan) = &self.pan {
            if event.pressed_button.is_none() {
                self.pan = None;
                cx.notify();
                return;
            }
            let target = pan.offset + (event.position - pan.mouse);
            self.set_offset(target, window);
            cx.notify();
        } else if let Some(drag) = &self.scrollbar_drag {
            if event.pressed_button != Some(MouseButton::Left) {
                self.scrollbar_drag = None;
                cx.notify();
                return;
            }
            let grab = drag.grab;
            self.drag_scrollbar_to(f32::from(event.position.y), grab, window, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.pan.take().is_some() || self.scrollbar_drag.take().is_some() {
            cx.notify();
        }
    }

    fn start_pan(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.goto_input = None;
        window.focus(&self.focus);
        if self.doc.is_some() {
            self.pan = Some(Pan { mouse: event.position, offset: self.current_offset() });
            cx.notify();
        }
    }

    /// Geometry of the vertical scrollbar thumb: (top, height) within the track.
    fn scrollbar_thumb(&self, layout: &Layout, viewport: Size<f32>, offset_y: f32) -> Option<(f32, f32)> {
        if layout.content_h <= viewport.height + 0.5 {
            return None;
        }
        let track = viewport.height;
        let thumb_h = (track * viewport.height / layout.content_h).max(28.0).min(track);
        let max_y = layout.content_h - viewport.height;
        let top = (-offset_y / max_y).clamp(0.0, 1.0) * (track - thumb_h);
        Some((top, thumb_h))
    }

    fn start_scrollbar_drag(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let viewport = self.viewport(window);
        let layout = self.layout(viewport, self.effective_zoom(viewport));
        let offset = self.current_offset();
        let Some((thumb_top, thumb_h)) = self.scrollbar_thumb(&layout, viewport, f32::from(offset.y)) else {
            return;
        };
        let track_top = TOOLBAR_HEIGHT;
        let y = f32::from(event.position.y) - track_top;
        let grab = if y >= thumb_top && y <= thumb_top + thumb_h { y - thumb_top } else { thumb_h / 2.0 };
        self.scrollbar_drag = Some(ScrollbarDrag { grab });
        self.drag_scrollbar_to(f32::from(event.position.y), grab, window, cx);
    }

    fn drag_scrollbar_to(&mut self, mouse_y: f32, grab: f32, window: &Window, cx: &mut Context<Self>) {
        let viewport = self.viewport(window);
        let layout = self.layout(viewport, self.effective_zoom(viewport));
        let offset = self.current_offset();
        let Some((_, thumb_h)) = self.scrollbar_thumb(&layout, viewport, f32::from(offset.y)) else {
            return;
        };
        let travel = (viewport.height - thumb_h).max(1.0);
        let top = (mouse_y - TOOLBAR_HEIGHT - grab).clamp(0.0, travel);
        let max_y = layout.content_h - viewport.height;
        self.pending_scroll =
            Some(self.clamp_offset(point(offset.x, px(-(top / travel) * max_y)), &layout, viewport));
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------

struct ThumbSlot {
    top: f32,
    width: f32,
    height: f32,
}

fn thumb_slots(viewer: &Viewer) -> (Vec<ThumbSlot>, f32) {
    let Some(doc) = &viewer.doc else { return (vec![], 0.0) };
    let mut y = THUMB_TOP;
    let slots = doc
        .pages
        .iter()
        .map(|page| {
            let s = page.rotated(viewer.rotation);
            let scale = (THUMB_MAX_W / s.width).min(THUMB_MAX_H / s.height);
            let slot = ThumbSlot {
                top: y,
                width: (s.width * scale).round(),
                height: (s.height * scale).round(),
            };
            y += slot.height + 6.0 + THUMB_LABEL_H + THUMB_SPACING;
            slot
        })
        .collect();
    (slots, y + THUMB_TOP)
}

fn slots_in(slots: &[ThumbSlot], y0: f32, y1: f32) -> Range<usize> {
    let item_h = |s: &ThumbSlot| s.height + 6.0 + THUMB_LABEL_H;
    let start = slots.iter().position(|s| s.top + item_h(s) > y0).unwrap_or(slots.len());
    let end = slots.partition_point(|s| s.top < y1).max(start);
    start..end
}

// ---------------------------------------------------------------------------
// Small UI helpers
// ---------------------------------------------------------------------------

struct TooltipView(SharedString);

impl Render for TooltipView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .font_family(theme::UI_FONT)
            .text_size(px(12.0))
            .text_color(theme::text())
            .bg(theme::tooltip_bg())
            .border_1()
            .border_color(theme::border())
            .rounded(px(5.0))
            .px(px(8.0))
            .py(px(4.0))
            .child(self.0.clone())
    }
}

fn tooltip(text: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_, cx| cx.new(|_| TooltipView(text.into())).into()
}

fn icon_button(id: &'static str, icon: &'static str, tip: &'static str, enabled: bool, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(30.0))
        .rounded(px(6.0))
        .when(active, |d| d.bg(theme::active_bg()))
        .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(theme::hover_bg())))
        .child(
            svg()
                .path(icon)
                .size(px(17.0))
                .text_color(if enabled { theme::text() } else { theme::text_disabled() }),
        )
        .tooltip(tooltip(tip))
}

fn separator() -> Div {
    div().flex_none().w(px(1.0)).h(px(20.0)).mx(px(6.0)).bg(theme::border())
}

fn format_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl Viewer {
    fn render_toolbar(&self, zoom: f32, cx: &mut Context<Self>) -> AnyElement {
        let has_doc = self.doc.is_some();
        let count = self.page_count();
        let editing = self.goto_input.is_some();
        let page_text: SharedString = match &self.goto_input {
            Some(input) => format!("{input}|").into(),
            None if has_doc => (self.current_page + 1).to_string().into(),
            None => "-".into(),
        };
        let title: SharedString = match (&self.doc, &self.loading) {
            (_, Some(name)) => format!("Opening {name}...").into(),
            (Some(doc), None) => doc.file_name.clone().into(),
            (None, None) => "PDF Viewer".into(),
        };

        let left = div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(
                icon_button("sidebar", "icons/sidebar.svg", "Toggle Thumbnails (F4)", has_doc, self.sidebar && has_doc)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                        if this.doc.is_some() {
                            this.toggle_sidebar(w, cx)
                        }
                    })),
            )
            .child(
                icon_button("open", "icons/folder-open.svg", "Open File (Ctrl+O)", true, false)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.open_dialog(cx))),
            )
            .child(separator())
            .child(
                div()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(theme::text_muted())
                    .child(title),
            );

        let page_field = div()
            .id("page-field")
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .min_w(px(46.0))
            .h(px(26.0))
            .px(px(8.0))
            .rounded(px(5.0))
            .bg(theme::field_bg())
            .border_1()
            .border_color(if editing { theme::accent() } else { theme::border() })
            .when(has_doc, |d| d.cursor(CursorStyle::IBeam))
            .child(page_text)
            .tooltip(tooltip("Go to Page (Ctrl+G)"))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                if this.doc.is_some() {
                    this.goto_input = Some(String::new());
                    window.focus(&this.focus);
                    cx.notify();
                }
            }));

        let zoom_label = div()
            .id("zoom-label")
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .w(px(56.0))
            .h(px(26.0))
            .rounded(px(5.0))
            .when(has_doc, |d| d.cursor_pointer().hover(|s| s.bg(theme::hover_bg())))
            .text_color(if has_doc { theme::text() } else { theme::text_disabled() })
            .child(if has_doc { format!("{}%", (zoom * 100.0).round()) } else { "-".into() })
            .tooltip(tooltip("Actual Size (Ctrl+1)"))
            .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.set_zoom(1.0, None, w, cx)));

        let center = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.0))
            .child(
                icon_button("prev", "icons/chevron-left.svg", "Previous Page (Left)", has_doc && self.current_page > 0, false)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                        let page = this.current_page.saturating_sub(1);
                        this.go_to_page(page, w, cx)
                    })),
            )
            .child(page_field)
            .child(
                div()
                    .flex_none()
                    .px(px(6.0))
                    .text_color(theme::text_muted())
                    .child(format!("of {count}")),
            )
            .child(
                icon_button("next", "icons/chevron-right.svg", "Next Page (Right)", has_doc && self.current_page + 1 < count, false)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                        let page = this.current_page + 1;
                        this.go_to_page(page, w, cx)
                    })),
            )
            .child(separator())
            .child(
                icon_button("zoom-out", "icons/zoom-out.svg", "Zoom Out (Ctrl+-)", has_doc, false)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.zoom_step(-1, w, cx))),
            )
            .child(zoom_label)
            .child(
                icon_button("zoom-in", "icons/zoom-in.svg", "Zoom In (Ctrl+=)", has_doc, false)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.zoom_step(1, w, cx))),
            )
            .child(separator())
            .child(
                icon_button("fit-width", "icons/fit-width.svg", "Fit Width (Ctrl+2)", has_doc, has_doc && self.fit == FitMode::Width)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.set_fit(FitMode::Width, w, cx))),
            )
            .child(
                icon_button("fit-page", "icons/fit-page.svg", "Fit Page (Ctrl+0)", has_doc, has_doc && self.fit == FitMode::Page)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.set_fit(FitMode::Page, w, cx))),
            );

        let (mode_icon, mode_tip) = match self.view_mode {
            ViewMode::Continuous => ("icons/scroll-mode.svg", "Continuous Scroll (Ctrl+M to switch)"),
            ViewMode::Single => ("icons/single-mode.svg", "Single Page (Ctrl+M to switch)"),
        };
        let right = div()
            .flex_1()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(2.0))
            .child(
                icon_button("rotate-ccw", "icons/rotate-ccw.svg", "Rotate Left (Ctrl+Shift+R)", has_doc, false)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.rotate(false, w, cx))),
            )
            .child(
                icon_button("rotate-cw", "icons/rotate-cw.svg", "Rotate Right (Ctrl+R)", has_doc, false)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.rotate(true, w, cx))),
            )
            .child(separator())
            .child(
                icon_button("view-mode", mode_icon, mode_tip, has_doc, false)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.toggle_view_mode(w, cx))),
            );

        div()
            .flex_none()
            .h(px(TOOLBAR_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .px(px(8.0))
            .gap(px(8.0))
            .bg(theme::toolbar_bg())
            .border_b_1()
            .border_color(theme::border())
            .child(left)
            .child(center)
            .child(right)
            .into_any_element()
    }

    fn render_sidebar(
        &mut self,
        viewport: Size<f32>,
        window: &mut Window,
        dpr: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let doc_id = self.doc.as_ref().map_or(0, |d| d.id);
        let (slots, total_h) = thumb_slots(self);

        // Keep the current page's thumbnail in view when the page changes.
        if self.thumb_synced_page != Some(self.current_page) {
            if let Some(slot) = slots.get(self.current_page) {
                let top = -f32::from(self.thumb_scroll.offset().y);
                let item_bottom = slot.top + slot.height + 6.0 + THUMB_LABEL_H;
                if slot.top < top || item_bottom > top + viewport.height {
                    let max = (total_h - viewport.height).max(0.0);
                    let target = (slot.top - viewport.height / 3.0).clamp(0.0, max);
                    self.thumb_scroll.set_offset(point(px(0.0), px(-target)));
                }
            }
            self.thumb_synced_page = Some(self.current_page);
        }

        let top = -f32::from(self.thumb_scroll.offset().y);
        let visible = slots_in(&slots, top - 200.0, top + viewport.height + 200.0);

        // Request missing thumbnails.
        for page in visible.clone() {
            let slot = &slots[page];
            let size = self.page_size(page);
            let key = JobKey {
                doc_id,
                page,
                kind: JobKind::Thumbnail,
                scale_milli: ((slot.width * dpr / size.width) * 1000.0).round().max(1.0) as u32,
                rotation: self.rotation,
            };
            let fresh = self.thumbs.get(&page).is_some_and(|b| b.key == key);
            if !fresh && !self.in_flight.contains(&key) && !self.failed.contains(&key) {
                self.pool.request(key);
                self.in_flight.insert(key);
            }
        }

        // Forget thumbnails that are far away.
        let keep = visible.start.saturating_sub(40)..visible.end + 40;
        let mut dropped = Vec::new();
        self.thumbs.retain(|page, bitmap| {
            let keep_it = keep.contains(page);
            if !keep_it {
                dropped.push(bitmap.image.clone());
            }
            keep_it
        });
        for image in dropped {
            window.drop_image(image).ok();
        }

        let items = visible.map(|page| {
            let slot = &slots[page];
            let selected = page == self.current_page;
            let bitmap = self.thumbs.get(&page).filter(|b| b.key.rotation == self.rotation);
            let content = match bitmap {
                Some(b) => img(b.image.clone()).size_full().object_fit(ObjectFit::Fill).into_any_element(),
                None => div().size_full().bg(theme::placeholder()).into_any_element(),
            };
            div()
                .id(("thumb", page))
                .absolute()
                .top(px(slot.top))
                .left(px(0.0))
                .w_full()
                .flex()
                .flex_col()
                .items_center()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.go_to_page(page, w, cx)))
                .child(
                    div()
                        .w(px(slot.width + 6.0))
                        .h(px(slot.height + 6.0))
                        .p(px(2.0))
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(if selected { theme::accent() } else { gpui::rgba(0x00000000) })
                        .when(selected, |d| d.bg(theme::accent_soft()))
                        .hover(|s| s.bg(theme::hover_bg()))
                        .child(div().size_full().bg(theme::paper()).child(content)),
                )
                .child(
                    div()
                        .h(px(THUMB_LABEL_H))
                        .flex()
                        .items_center()
                        .text_size(px(11.5))
                        .text_color(if selected { theme::text() } else { theme::text_muted() })
                        .child((page + 1).to_string()),
                )
        });

        div()
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .bg(theme::sidebar_bg())
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .id("thumbs")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.thumb_scroll)
                    .child(div().relative().w_full().h(px(total_h)).children(items)),
            )
            .into_any_element()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .child(
                div()
                    .size(px(76.0))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::toolbar_bg())
                    .border_1()
                    .border_color(theme::border())
                    .child(svg().path("icons/file.svg").size(px(32.0)).text_color(theme::text_muted())),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .text_size(px(19.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("No document open"),
            )
            .child(
                div()
                    .text_color(theme::text_muted())
                    .child("Open a PDF file, or drop one anywhere in this window."),
            )
            .child(
                div()
                    .id("open-primary")
                    .mt(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(36.0))
                    .px(px(16.0))
                    .rounded(px(7.0))
                    .bg(theme::accent())
                    .text_color(gpui::rgb(0xffffff))
                    .cursor_pointer()
                    .hover(|s| s.bg(gpui::rgb(0x5d99ff)))
                    .child(svg().path("icons/folder-open.svg").size(px(16.0)).text_color(gpui::rgb(0xffffff)))
                    .child(div().font_weight(gpui::FontWeight::MEDIUM).child("Open File"))
                    .child(div().text_size(px(12.0)).opacity(0.75).child("Ctrl+O"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.open_dialog(cx))),
            )
            .into_any_element()
    }

    fn render_status(&self, zoom: f32) -> AnyElement {
        let mut left: Vec<String> = Vec::new();
        let mut right: Vec<String> = Vec::new();
        if let Some(doc) = &self.doc {
            left.push(format!("Page {} of {}", self.current_page + 1, doc.pages.len()));
            let size = self.page_size(self.current_page);
            left.push(format!("{:.2} x {:.2} in", size.width / 72.0, size.height / 72.0));
            if self.rotation != 0 {
                left.push(format!("Rotated {}\u{b0}", self.rotation));
            }
            if let Some(ms) = self.last_render_ms {
                right.push(format!("Rendered in {ms:.0} ms"));
            }
            right.push(format_size(doc.file_size));
            right.push(format!("{}%", (zoom * 100.0).round()));
        } else {
            left.push("Ready".into());
        }
        div()
            .flex_none()
            .h(px(STATUS_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .px(px(12.0))
            .bg(theme::toolbar_bg())
            .border_t_1()
            .border_color(theme::border())
            .text_size(px(12.0))
            .text_color(theme::text_muted())
            .child(div().flex().gap(px(16.0)).children(left))
            .child(div().flex().gap(px(16.0)).children(right))
            .into_any_element()
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.to_drop.drain(..) {
            window.drop_image(image).ok();
        }

        let title = match &self.doc {
            Some(doc) => format!("{} - PDF Viewer", doc.file_name),
            None => "PDF Viewer".to_string(),
        };
        if title != self.last_title {
            window.set_window_title(&title);
            self.last_title = title;
        }

        let viewport = self.viewport(window);
        let zoom = self.effective_zoom(viewport);
        if self.fit != FitMode::None {
            self.zoom = zoom;
        }
        let dpr = window.scale_factor();

        let mut canvas_children: Vec<AnyElement> = Vec::new();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some(doc_id) = self.doc.as_ref().map(|d| d.id) {
            let count = self.page_count();
            let mut layout = self.layout(viewport, zoom);

            // Apply programmatic scrolling before reading the offset.
            if let Some(offset) = self.pending_scroll.take() {
                self.scroll.set_offset(self.clamp_offset(offset, &layout, viewport));
            }
            let offset = self.clamp_offset(self.scroll.offset(), &layout, viewport);

            // Track the current page while the user scrolls (navigation pins it).
            let pinned = self.expected_offset.is_some_and(|e| {
                (f32::from(e.y) - f32::from(offset.y)).abs() < 1.5
                    && (f32::from(e.x) - f32::from(offset.x)).abs() < 1.5
            });
            if !pinned {
                self.expected_offset = None;
                if self.view_mode == ViewMode::Continuous {
                    let probe = -f32::from(offset.y) + viewport.height * 0.35;
                    let page = layout.page_at(probe);
                    if page != self.current_page {
                        self.current_page = page;
                        if self.fit == FitMode::Page {
                            layout = self.layout(viewport, zoom);
                        }
                    }
                }
            }

            // Re-render at a new scale only after zooming settles.
            let scale_milli = (zoom * PX_PER_PT * dpr * 1000.0).round().max(1.0) as u32;
            if scale_milli != self.last_scale_milli {
                self.last_scale_milli = scale_milli;
                self.scale_changed_at = Instant::now();
                cx.spawn(async move |this, cx| {
                    Timer::after(RERENDER_DELAY + Duration::from_millis(10)).await;
                    this.update(cx, |_, cx| cx.notify()).ok();
                })
                .detach();
            }
            let settled = self.scale_changed_at.elapsed() >= RERENDER_DELAY;

            let top = -f32::from(offset.y);
            let bottom = top + viewport.height;
            let preload = layout.pages_in(top - viewport.height * 0.5, bottom + viewport.height * 0.75);
            let keep = preload.start.saturating_sub(3)..(preload.end + 3).min(count);

            let (slots, _) = if self.sidebar { thumb_slots(self) } else { (vec![], 0.0) };
            let thumb_top = -f32::from(self.thumb_scroll.offset().y);
            let thumb_range = if self.sidebar {
                let v = slots_in(&slots, thumb_top - 400.0, thumb_top + viewport.height + 400.0);
                v.start.saturating_sub(4)..(v.end + 4).min(count)
            } else {
                0..0
            };
            for key in self.pool.set_interest(
                doc_id,
                keep.clone(),
                thumb_range,
                scale_milli,
                self.rotation,
                self.current_page,
            ) {
                self.in_flight.remove(&key);
            }

            // Request page bitmaps.
            for page in preload.clone() {
                let key = JobKey { doc_id, page, kind: JobKind::Page, scale_milli, rotation: self.rotation };
                let needs = match self.pages.get(&page) {
                    Some(b) => b.key != key && (settled || b.key.rotation != self.rotation),
                    None => true,
                };
                if needs && !self.in_flight.contains(&key) && !self.failed.contains(&key) {
                    self.pool.request(key);
                    self.in_flight.insert(key);
                }
            }

            // Release bitmaps that scrolled far away.
            let mut dropped = Vec::new();
            self.pages.retain(|page, bitmap| {
                let keep_it = keep.contains(page);
                if !keep_it {
                    dropped.push(bitmap.image.clone());
                }
                keep_it
            });
            for image in dropped {
                window.drop_image(image).ok();
            }

            let shadow = vec![BoxShadow {
                color: theme::page_shadow().into(),
                offset: point(px(0.0), px(2.0)),
                blur_radius: px(10.0),
                spread_radius: px(0.0),
            }];
            let page_elements = preload.clone().map(|page| {
                let (x, y, w, h) = layout.page_rect(page);
                let bitmap = self.pages.get(&page).filter(|b| b.key.rotation == self.rotation);
                let failed = self
                    .failed
                    .iter()
                    .any(|k| k.page == page && k.kind == JobKind::Page && k.rotation == self.rotation);
                let content = match bitmap {
                    Some(b) => img(b.image.clone()).size_full().object_fit(ObjectFit::Fill).into_any_element(),
                    None => div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(theme::placeholder())
                        .text_size(px(13.0))
                        .text_color(gpui::rgb(0x8a8f98))
                        .child(if failed { "Unable to render this page" } else { "Rendering..." })
                        .into_any_element(),
                };
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(w))
                    .h(px(h))
                    .bg(theme::paper())
                    .shadow(shadow.clone())
                    .child(content)
            });

            canvas_children.push(
                div()
                    .id("canvas")
                    .size_full()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .cursor(if self.pan.is_some() { CursorStyle::ClosedHand } else { CursorStyle::OpenHand })
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::start_pan))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::start_pan))
                    .on_scroll_wheel(cx.listener(Self::on_canvas_wheel))
                    .child(
                        div()
                            .relative()
                            .w(px(layout.content_w))
                            .h(px(layout.content_h))
                            .children(page_elements),
                    )
                    .into_any_element(),
            );

            // Vertical scrollbar (draggable).
            if let Some((thumb_top, thumb_h)) = self.scrollbar_thumb(&layout, viewport, f32::from(offset.y)) {
                let dragging = self.scrollbar_drag.is_some();
                overlays.push(
                    div()
                        .id("vscroll")
                        .absolute()
                        .top(px(0.0))
                        .right(px(0.0))
                        .w(px(SCROLLBAR_SIZE + 2.0))
                        .h_full()
                        .cursor_default()
                        .on_mouse_down(MouseButton::Left, cx.listener(Self::start_scrollbar_drag))
                        .child(
                            div()
                                .absolute()
                                .top(px(thumb_top))
                                .right(px(2.0))
                                .w(px(SCROLLBAR_SIZE - 3.0))
                                .h(px(thumb_h))
                                .rounded(px(4.0))
                                .bg(if dragging { theme::scrollbar_thumb_active() } else { theme::scrollbar_thumb() })
                                .hover(|s| s.bg(theme::scrollbar_thumb_active())),
                        )
                        .into_any_element(),
                );
            }
            // Horizontal position indicator.
            if layout.content_w > viewport.width + 0.5 {
                let thumb_w = (viewport.width * viewport.width / layout.content_w).max(28.0);
                let max_x = layout.content_w - viewport.width;
                let left = (-f32::from(offset.x) / max_x).clamp(0.0, 1.0) * (viewport.width - thumb_w);
                overlays.push(
                    div()
                        .absolute()
                        .bottom(px(2.0))
                        .left(px(left))
                        .w(px(thumb_w))
                        .h(px(SCROLLBAR_SIZE - 3.0))
                        .rounded(px(4.0))
                        .bg(theme::scrollbar_thumb())
                        .into_any_element(),
                );
            }
        } else {
            canvas_children.push(self.render_empty_state(cx));
        }

        if let Some(name) = &self.loading {
            overlays.push(
                div()
                    .absolute()
                    .top(px(16.0))
                    .left(px(0.0))
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px(px(14.0))
                            .py(px(7.0))
                            .rounded(px(16.0))
                            .bg(theme::toolbar_bg())
                            .border_1()
                            .border_color(theme::border())
                            .text_color(theme::text())
                            .child(format!("Opening {name}...")),
                    )
                    .into_any_element(),
            );
        }
        if let Some(message) = &self.error {
            overlays.push(
                div()
                    .absolute()
                    .top(px(16.0))
                    .left(px(0.0))
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .pl(px(12.0))
                            .pr(px(6.0))
                            .py(px(6.0))
                            .rounded(px(8.0))
                            .bg(theme::danger_bg())
                            .border_1()
                            .border_color(theme::danger_border())
                            .text_color(theme::text())
                            .child(svg().path("icons/alert.svg").size(px(16.0)).text_color(gpui::rgb(0xff7b86)))
                            .child(message.clone())
                            .child(
                                div()
                                    .id("dismiss-error")
                                    .size(px(24.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::hover_bg()))
                                    .child(svg().path("icons/close.svg").size(px(14.0)).text_color(theme::text_muted()))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.error = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .into_any_element(),
            );
        }

        let sidebar = if self.sidebar && self.doc.is_some() {
            Some(self.render_sidebar(viewport, window, dpr, cx))
        } else {
            None
        };

        let canvas = div()
            .id("canvas-area")
            .relative()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .bg(theme::canvas_bg())
            .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(theme::accent_soft()))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                let pdf = paths
                    .paths()
                    .iter()
                    .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")))
                    .or_else(|| paths.paths().first())
                    .cloned();
                if let Some(path) = pdf {
                    this.open_path(path, cx);
                }
            }))
            .children(canvas_children)
            .children(overlays);

        let toolbar = self.render_toolbar(zoom, cx);
        let status = self.render_status(zoom);

        div()
            .key_context("Viewer")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_action(cx.listener(|this, _: &OpenFile, _, cx| this.open_dialog(cx)))
            .on_action(cx.listener(|this, _: &CloseFile, _, cx| this.close_file(cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, w, cx| this.zoom_step(1, w, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, w, cx| this.zoom_step(-1, w, cx)))
            .on_action(cx.listener(|this, _: &ActualSize, w, cx| this.set_zoom(1.0, None, w, cx)))
            .on_action(cx.listener(|this, _: &FitWidth, w, cx| this.set_fit(FitMode::Width, w, cx)))
            .on_action(cx.listener(|this, _: &FitPage, w, cx| this.set_fit(FitMode::Page, w, cx)))
            .on_action(cx.listener(|this, _: &NextPage, w, cx| {
                let page = this.current_page + 1;
                this.go_to_page(page, w, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevPage, w, cx| {
                let page = this.current_page.saturating_sub(1);
                this.go_to_page(page, w, cx)
            }))
            .on_action(cx.listener(|this, _: &FirstPage, w, cx| this.go_to_page(0, w, cx)))
            .on_action(cx.listener(|this, _: &LastPage, w, cx| this.go_to_page(usize::MAX, w, cx)))
            .on_action(cx.listener(|this, _: &ScrollUp, w, cx| this.scroll_by(0.0, -LINE_SCROLL, w, cx)))
            .on_action(cx.listener(|this, _: &ScrollDown, w, cx| this.scroll_by(0.0, LINE_SCROLL, w, cx)))
            .on_action(cx.listener(|this, _: &ScrollLeft, w, cx| this.scroll_by(-LINE_SCROLL, 0.0, w, cx)))
            .on_action(cx.listener(|this, _: &ScrollRight, w, cx| this.scroll_by(LINE_SCROLL, 0.0, w, cx)))
            .on_action(cx.listener(|this, _: &ScreenDown, w, cx| {
                let h = this.viewport(w).height * 0.9;
                this.scroll_by(0.0, h, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ScreenUp, w, cx| {
                let h = this.viewport(w).height * 0.9;
                this.scroll_by(0.0, -h, w, cx)
            }))
            .on_action(cx.listener(|this, _: &RotateClockwise, w, cx| this.rotate(true, w, cx)))
            .on_action(cx.listener(|this, _: &RotateCounterClockwise, w, cx| this.rotate(false, w, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, w, cx| {
                if this.doc.is_some() {
                    this.toggle_sidebar(w, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleViewMode, w, cx| this.toggle_view_mode(w, cx)))
            .on_action(cx.listener(|this, _: &GoToPage, _, cx| {
                if this.doc.is_some() {
                    this.goto_input = Some(String::new());
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::app_bg())
            .font_family(theme::UI_FONT)
            .text_size(px(13.0))
            .text_color(theme::text())
            .child(toolbar)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .flex_row()
                    .children(sidebar)
                    .child(canvas),
            )
            .child(status)
    }
}
