//! PDF engine: document loading and a background rendering pool built on Hayro.
//!
//! Hayro's `Page` and `RenderCache` borrow the parsed `Pdf`, so every worker thread
//! parses its own `Pdf` (sharing the same `Arc` bytes, no copy) and keeps one
//! `RenderCache` per document for the whole lifetime of that document.

use std::collections::HashMap;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use gpui::RenderImage;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_interpret::font::{FallbackFontQuery, FontData, FontQuery};
use hayro::hayro_interpret::hayro_cmap::CidFamily;
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_syntax::Pdf;
use hayro::kurbo::Affine;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::vello_cpu::{Pixmap, RasterizerSettings, RenderContext, Resources, TargetInit};
use hayro::{RenderCache, RenderSettings, render_into};
use image::{Frame, ImageBuffer, Rgba};
use smallvec::smallvec;

/// Largest bitmap edge we are willing to produce (GPU texture limits).
const MAX_EDGE: f32 = 8192.0;
/// Largest bitmap area in pixels (~128 MB of BGRA).
const MAX_AREA: f32 = 32_000_000.0;

/// Size of a page in PDF points, already accounting for the page's own /Rotate.
#[derive(Clone, Copy, Debug)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

impl PageSize {
    /// Size after applying an additional view rotation (degrees, multiple of 90).
    pub fn rotated(self, rotation: u32) -> Self {
        if rotation % 180 == 90 {
            Self { width: self.height, height: self.width }
        } else {
            self
        }
    }
}

/// A successfully opened document.
pub struct DocumentInfo {
    pub id: u64,
    pub file_name: String,
    pub file_size: u64,
    pub pages: Vec<PageSize>,
    data: Arc<Vec<u8>>,
}

/// Read and parse a PDF file. Runs on a background thread.
pub fn load_document(id: u64, path: &Path) -> Result<DocumentInfo, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Could not read file: {e}"))?;
    let data = Arc::new(bytes);
    let pdf = Pdf::new(data.clone()).map_err(|e| match e {
        hayro::hayro_syntax::LoadPdfError::Decryption(_) => {
            "This document is encrypted and cannot be opened.".to_string()
        }
        hayro::hayro_syntax::LoadPdfError::Invalid => {
            "The file is not a valid PDF document.".to_string()
        }
    })?;
    let pages: Vec<PageSize> = pdf
        .pages()
        .iter()
        .map(|p| {
            let (width, height) = p.render_dimensions();
            PageSize { width: width.max(1.0), height: height.max(1.0) }
        })
        .collect();
    if pages.is_empty() {
        return Err("The document does not contain any pages.".to_string());
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled.pdf".to_string());
    Ok(DocumentInfo {
        id,
        file_name,
        file_size: data.len() as u64,
        pages,
        data,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobKind {
    Page,
    Thumbnail,
}

/// Identifies one rendered bitmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JobKey {
    pub doc_id: u64,
    pub page: usize,
    pub kind: JobKind,
    /// Render scale (pixels per PDF point) multiplied by 1000.
    pub scale_milli: u32,
    pub rotation: u32,
}

impl JobKey {
    pub fn scale(&self) -> f32 {
        self.scale_milli as f32 / 1000.0
    }
}

pub enum RenderOutcome {
    Done { image: Arc<RenderImage>, millis: f32 },
    Failed(String),
}

pub struct RenderResult {
    pub key: JobKey,
    pub outcome: RenderOutcome,
}

/// What the UI currently cares about; jobs that fall outside are dropped.
#[derive(Clone, Debug, Default)]
struct Interest {
    doc_id: u64,
    page_range: Range<usize>,
    thumb_range: Range<usize>,
    page_scale_milli: u32,
    rotation: u32,
    focus_page: usize,
}

#[derive(Default)]
struct Queue {
    doc: Option<(u64, Arc<Vec<u8>>)>,
    jobs: Vec<JobKey>,
    interest: Interest,
    shutdown: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    signal: Condvar,
}

/// A pool of rendering threads.
pub struct RenderPool {
    shared: Arc<Shared>,
}

impl RenderPool {
    pub fn new(results: async_channel::Sender<RenderResult>) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            signal: Condvar::new(),
        });
        let threads = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).clamp(1, 4))
            .unwrap_or(2);
        for index in 0..threads {
            let shared = shared.clone();
            let results = results.clone();
            std::thread::Builder::new()
                .name(format!("pdf-render-{index}"))
                .spawn(move || worker_main(shared, results))
                .expect("failed to spawn render thread");
        }
        Self { shared }
    }

    /// Switch the pool to a new document; pending jobs for the old one are discarded.
    pub fn set_document(&self, doc: &DocumentInfo) {
        let mut q = self.shared.queue.lock().unwrap();
        q.doc = Some((doc.id, doc.data.clone()));
        q.jobs.clear();
        q.interest = Interest { doc_id: doc.id, ..Default::default() };
        drop(q);
        self.shared.signal.notify_all();
    }

    pub fn clear_document(&self) {
        let mut q = self.shared.queue.lock().unwrap();
        q.doc = None;
        q.jobs.clear();
        drop(q);
        self.shared.signal.notify_all();
    }

    /// Tell the pool which pages are visible. Returns the jobs that were dropped
    /// so the caller can forget about them.
    pub fn set_interest(
        &self,
        doc_id: u64,
        page_range: Range<usize>,
        thumb_range: Range<usize>,
        page_scale_milli: u32,
        rotation: u32,
        focus_page: usize,
    ) -> Vec<JobKey> {
        let mut q = self.shared.queue.lock().unwrap();
        q.interest = Interest {
            doc_id,
            page_range,
            thumb_range,
            page_scale_milli,
            rotation,
            focus_page,
        };
        let interest = q.interest.clone();
        let mut dropped = Vec::new();
        q.jobs.retain(|job| {
            let keep = is_wanted(job, &interest);
            if !keep {
                dropped.push(*job);
            }
            keep
        });
        dropped
    }

    pub fn request(&self, key: JobKey) {
        let mut q = self.shared.queue.lock().unwrap();
        if !q.jobs.contains(&key) {
            q.jobs.push(key);
        }
        drop(q);
        self.shared.signal.notify_one();
    }
}

impl Drop for RenderPool {
    fn drop(&mut self) {
        if let Ok(mut q) = self.shared.queue.lock() {
            q.shutdown = true;
            q.jobs.clear();
        }
        self.shared.signal.notify_all();
    }
}

fn is_wanted(job: &JobKey, interest: &Interest) -> bool {
    if job.doc_id != interest.doc_id || job.rotation != interest.rotation {
        return false;
    }
    match job.kind {
        JobKind::Page => {
            interest.page_range.contains(&job.page) && job.scale_milli == interest.page_scale_milli
        }
        JobKind::Thumbnail => interest.thumb_range.contains(&job.page),
    }
}

enum Next {
    Job(JobKey),
    SwitchDocument(u64, Arc<Vec<u8>>),
    Idle,
    Shutdown,
}

/// Pick the most useful job: visible pages first (closest to the focus page),
/// thumbnails afterwards.
fn next_job(q: &mut Queue, current_doc: Option<u64>) -> Next {
    if q.shutdown {
        return Next::Shutdown;
    }
    let Some((doc_id, data)) = q.doc.clone() else {
        return Next::Idle;
    };
    if current_doc != Some(doc_id) {
        return Next::SwitchDocument(doc_id, data);
    }
    if q.jobs.is_empty() {
        return Next::Idle;
    }
    let focus = q.interest.focus_page;
    let best = q
        .jobs
        .iter()
        .enumerate()
        .min_by_key(|(_, job)| {
            let kind_rank = match job.kind {
                JobKind::Page => 0usize,
                JobKind::Thumbnail => 1,
            };
            (kind_rank, job.page.abs_diff(focus))
        })
        .map(|(ix, _)| ix)
        .unwrap();
    Next::Job(q.jobs.swap_remove(best))
}

fn worker_main(shared: Arc<Shared>, results: async_channel::Sender<RenderResult>) {
    let settings = interpreter_settings();
    let mut pending_doc: Option<(u64, Arc<Vec<u8>>)> = None;
    loop {
        // Wait until there is a document to serve.
        let (doc_id, data) = match pending_doc.take() {
            Some(doc) => doc,
            None => {
                let mut q = shared.queue.lock().unwrap();
                loop {
                    match next_job(&mut q, None) {
                        Next::Shutdown => return,
                        Next::SwitchDocument(id, data) => break (id, data),
                        _ => q = shared.signal.wait(q).unwrap(),
                    }
                }
            }
        };
        match serve_document(doc_id, data, &shared, &results, &settings) {
            Some(next) => pending_doc = next,
            None => return,
        }
    }
}

/// Render jobs for one document until the document changes.
/// Returns `None` on shutdown, `Some(next_doc)` when the document was switched or closed.
fn serve_document(
    doc_id: u64,
    data: Arc<Vec<u8>>,
    shared: &Shared,
    results: &async_channel::Sender<RenderResult>,
    settings: &InterpreterSettings,
) -> Option<Option<(u64, Arc<Vec<u8>>)>> {
    let pdf = Pdf::new(data).ok();
    let cache = RenderCache::new();
    let mut ctx = RenderContext::new(1, 1);
    let mut resources = Resources::default();
    let render_settings = RenderSettings::default();

    loop {
        let job = {
            let mut q = shared.queue.lock().unwrap();
            loop {
                match next_job(&mut q, Some(doc_id)) {
                    Next::Shutdown => return None,
                    Next::SwitchDocument(id, data) => return Some(Some((id, data))),
                    Next::Job(job) => break job,
                    Next::Idle => {
                        if q.doc.is_none() {
                            return Some(None);
                        }
                        q = shared.signal.wait(q).unwrap();
                    }
                }
            }
        };

        let outcome = match pdf.as_ref() {
            None => RenderOutcome::Failed("Document could not be parsed".into()),
            Some(pdf) => {
                let started = Instant::now();
                let rendered = catch_unwind(AssertUnwindSafe(|| {
                    render_page(
                        pdf,
                        &cache,
                        settings,
                        &render_settings,
                        &mut ctx,
                        &mut resources,
                        job,
                    )
                }));
                match rendered {
                    Ok(Ok(image)) => RenderOutcome::Done {
                        image,
                        millis: started.elapsed().as_secs_f32() * 1000.0,
                    },
                    Ok(Err(message)) => RenderOutcome::Failed(message),
                    Err(_) => {
                        // A panic may leave the context in a bad state; start fresh.
                        ctx = RenderContext::new(1, 1);
                        resources = Resources::default();
                        RenderOutcome::Failed("The renderer crashed on this page".into())
                    }
                }
            }
        };
        if results.send_blocking(RenderResult { key: job, outcome }).is_err() {
            return None;
        }
    }
}

fn render_page<'a>(
    pdf: &'a Pdf,
    cache: &RenderCache<'a>,
    settings: &InterpreterSettings,
    render_settings: &RenderSettings,
    ctx: &mut RenderContext,
    resources: &mut Resources,
    job: JobKey,
) -> Result<Arc<RenderImage>, String> {
    let pages = pdf.pages();
    let page = pages.get(job.page).ok_or("Page does not exist")?;
    let (base_w, base_h) = page.render_dimensions();
    let (base_w, base_h) = (base_w.max(1.0), base_h.max(1.0));
    let rotation = job.rotation % 360;
    let (out_w, out_h) = if rotation % 180 == 90 { (base_h, base_w) } else { (base_w, base_h) };

    // Clamp the scale so the bitmap stays within texture and memory limits.
    let mut scale = job.scale().max(0.01);
    scale = scale.min(MAX_EDGE / out_w).min(MAX_EDGE / out_h);
    scale = scale.min((MAX_AREA / (out_w * out_h)).sqrt());

    let width = (out_w * scale).round().clamp(1.0, MAX_EDGE) as u16;
    let height = (out_h * scale).round().clamp(1.0, MAX_EDGE) as u16;

    // Page space (after /Rotate, origin top-left) -> scaled -> rotated around the centre.
    let transform = Affine::translate((width as f64 / 2.0, height as f64 / 2.0))
        * Affine::rotate((rotation as f64).to_radians())
        * Affine::scale(scale as f64)
        * Affine::translate((-(base_w as f64) / 2.0, -(base_h as f64) / 2.0))
        * page.initial_transform(true).to_kurbo();

    ctx.reset_and_resize(width, height);
    render_into(page, cache, settings, render_settings, ctx, transform);
    ctx.flush();

    let mut pixmap = Pixmap::new(width, height);
    ctx.render_with(
        &mut pixmap,
        resources,
        RasterizerSettings { target_init: TargetInit::Clear(WHITE), ..Default::default() },
    );

    // Hayro produces premultiplied RGBA on an opaque white background;
    // GPUI expects BGRA, so swap the red and blue channels.
    let mut bytes = pixmap.data_as_u8_slice().to_vec();
    for px in bytes.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width as u32, height as u32, bytes)
        .ok_or("Invalid bitmap size")?;
    Ok(Arc::new(RenderImage::new(smallvec![Frame::new(buffer)])))
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

fn interpreter_settings() -> InterpreterSettings {
    InterpreterSettings {
        font_resolver: Arc::new(|query: &FontQuery| match query {
            FontQuery::Standard(font) => Some(font.get_font_data()),
            FontQuery::Fallback(fallback) => {
                cjk_fallback(fallback).or_else(|| Some(fallback.pick_standard_font().get_font_data()))
            }
        }),
        ..Default::default()
    }
}

/// Non-embedded CJK fonts fall back to the fonts shipped with Windows.
fn cjk_fallback(query: &FallbackFontQuery) -> Option<(FontData, u32)> {
    let collection = query.character_collection.as_ref()?;
    let candidates: &[&str] = match (&collection.family, query.is_bold) {
        (CidFamily::AdobeGB1, false) => &["msyh.ttc", "simsun.ttc", "simhei.ttf"],
        (CidFamily::AdobeGB1, true) => &["msyhbd.ttc", "msyh.ttc", "simhei.ttf"],
        (CidFamily::AdobeCNS1, false) => &["msjh.ttc", "mingliu.ttc", "msyh.ttc"],
        (CidFamily::AdobeCNS1, true) => &["msjhbd.ttc", "msjh.ttc", "msyh.ttc"],
        (CidFamily::AdobeJapan1, false) => &["YuGothR.ttc", "msgothic.ttc", "meiryo.ttc"],
        (CidFamily::AdobeJapan1, true) => &["YuGothB.ttc", "meiryob.ttc", "msgothic.ttc"],
        (CidFamily::AdobeKorea1, false) => &["malgun.ttf", "gulim.ttc"],
        (CidFamily::AdobeKorea1, true) => &["malgunbd.ttf", "malgun.ttf"],
        #[allow(unreachable_patterns)]
        _ => return None,
    };
    candidates.iter().find_map(|name| system_font(name)).map(|data| (data, 0))
}

fn system_font(name: &str) -> Option<FontData> {
    static FONTS: OnceLock<Mutex<HashMap<String, Option<FontData>>>> = OnceLock::new();
    let fonts = FONTS.get_or_init(Default::default);
    let mut fonts = fonts.lock().unwrap();
    fonts
        .entry(name.to_string())
        .or_insert_with(|| {
            let dir = std::env::var_os("WINDIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
            std::fs::read(dir.join("Fonts").join(name))
                .ok()
                .map(|bytes| Arc::new(bytes) as FontData)
        })
        .clone()
}
