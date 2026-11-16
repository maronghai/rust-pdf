# rust-pdf

A fast, lightweight PDF viewer for Windows, built with [GPUI](https://www.gpui.rs/) for the UI and [Hayro](https://github.com/LaurenzV/hayro) for PDF parsing and rendering. Everything is written in Rust.

Pages are rasterized on background threads and drawn by GPUI, so the window stays responsive while you scroll and zoom.

## Features

- **Opening files**: toolbar button, `Ctrl+O`, drag and drop onto the window, or a path on the command line
- **Navigation**: thumbnail sidebar, continuous and single page modes, page number input, first/previous/next/last page
- **Zoom**: zoom in/out in preset steps, actual size, fit width, fit page, and `Ctrl` + mouse wheel zoom centered on the cursor
- **Rotation**: rotate the view clockwise or counter-clockwise in 90° steps
- **Panning**: drag the page with the left or middle mouse button, plus a draggable scrollbar
- **CJK text**: when a PDF references Chinese, Japanese or Korean fonts that are not embedded, the viewer uses the fonts that ship with Windows (Microsoft YaHei, SimSun, Microsoft JhengHei, Yu Gothic, Malgun Gothic and others)
- **Dark UI** with SVG icons compiled into the executable

## Requirements

- Windows 10 or later
- Rust 1.85 or newer (the crate uses edition 2024)

## Building

```sh
cargo build --release
```

The executable is written to `target/release/rust-pdf.exe`. Release builds use the Windows GUI subsystem, so no console window opens.

Hayro does all rendering on the CPU. To keep debug builds usable, `Cargo.toml` compiles all dependencies with `opt-level = 3`, including in the dev profile.

## Usage

```sh
# Start with an empty window
rust-pdf.exe

# Open a file directly
rust-pdf.exe path\to\document.pdf
```

You can also drop a `.pdf` file onto the window at any time.

### Keyboard shortcuts

| Action | Shortcut |
| --- | --- |
| Open file | `Ctrl+O` |
| Close file | `Ctrl+W` |
| Zoom in / out | `Ctrl+=` / `Ctrl+-` |
| Actual size (100 %) | `Ctrl+1` |
| Fit width | `Ctrl+2` |
| Fit page | `Ctrl+0` |
| Zoom around cursor | `Ctrl` + mouse wheel |
| Next / previous page | `Right` / `Left`, `PageDown` / `PageUp` |
| First / last page | `Home` / `End`, `Ctrl+Home` / `Ctrl+End` |
| Scroll up / down | `Up` / `Down` |
| Scroll left / right | `Shift+Left` / `Shift+Right` |
| One screen down / up | `Space` / `Shift+Space` |
| Rotate clockwise / counter-clockwise | `Ctrl+R` / `Ctrl+Shift+R` |
| Toggle thumbnail sidebar | `F4` |
| Toggle continuous / single page mode | `Ctrl+M` |
| Go to page | `Ctrl+G`, type a number, then `Enter` (`Esc` cancels) |

## How it works

```
src/
├── main.rs         App startup, window, key bindings, embedded icon assets
├── viewer.rs       Viewer view: toolbar, sidebar, page canvas, status bar, input handling
├── pdf_engine.rs   Document loading, render thread pool, font fallback
└── theme.rs        Colors and layout metrics
assets/icons/       SVG icons
assets/app-icon.ico Application icon (16 to 256 px)
assets/app.rc       Windows resource script that references the icon
build.rs            Embeds the icon into the executable
```

- **Loading**: the file is read and parsed on GPUI's background executor. Page sizes are collected up front so the whole layout can be computed before any page is rendered.
- **Render pool**: between 1 and 4 worker threads (half the available cores). Hayro's `Page` and `RenderCache` borrow the parsed `Pdf`, so each worker parses its own copy from the same shared `Arc<Vec<u8>>` and keeps one `RenderCache` for the lifetime of the document.
- **Scheduling**: the viewer tells the pool which pages and thumbnails are visible, and which page is current. Jobs that scroll out of view or no longer match the current zoom or rotation are dropped. Visible pages render first, closest to the current page, and thumbnails after them.
- **Zooming**: while you are still zooming, the existing bitmaps are scaled. Pages are re-rendered at the new resolution about 140 ms after the last zoom change.
- **Limits**: a bitmap is capped at 8192 px per edge and about 32 megapixels, which keeps it within GPU texture limits.
- **Robustness**: rendering runs inside `catch_unwind`. If Hayro panics on a page, that page is marked as failed and the rest of the document keeps working.

## Limitations

- Password-protected PDFs cannot be opened. If Hayro fails to decrypt a file, the viewer shows an error.
- No text selection, search, outline (bookmarks) panel or link navigation yet.
- Hayro is still a young renderer, so some complex PDFs may render imperfectly.
- The CJK font fallback reads fonts from `%WINDIR%\Fonts`, so it only works on Windows.

## License

MIT
