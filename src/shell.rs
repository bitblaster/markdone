//! winit + wgpu + Vello application shell — the gpui replacement (see MIGRATION-PLAN.md).
//!
//! Phase 0–5: a working editor with inline git diff — renders a full markdown
//! document (variable-height lines, tree-sitter highlighting, browser-grade
//! wrapping, cursor-aware marker hiding) via Vello, with typing/arrows/backspace/
//! enter/tab, click-to-place and drag selection (Parley hit-testing through the
//! segment map), caret + selection painting, scroll (wheel + scroll-to-cursor),
//! minimal IME commit, inline diff vs HEAD (green added lines/words, red ghost
//! deleted lines interleaved above), and chrome (a title bar + a status bar with
//! nesting context and cursor position; the editor is inset + clipped between
//! them). CSD (custom window frame) and the async-blocked overlays are deferred.
//! Run with
//! `WGPU_BACKEND=vulkan cargo run --bin writ-next` on Asahi; set
//! `MARKDONE_SHELL_SNAPSHOT=out.png` (+ optional `MARKDONE_SHELL_{W,H,SCROLL,CURSOR,SEL_A,SEL_B}`)
//! to render one frame headlessly instead.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use unicode_segmentation::UnicodeSegmentation;
use vello::kurbo::{Affine, BezPath, Point, Rect, Stroke};
use vello::peniko::Fill;
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu;
use vello::wgpu::CurrentSurfaceTexture;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Theme, Window, WindowId};

use winit::event_loop::ControlFlow;

use crate::buffer::Buffer;
use crate::chrome::{BarRect, FindButtonRects, StatusInfo, draw_find_bar, draw_status_bar};
use crate::config::{self, Config, ExternalChange, Preferences, ResolvedConfig};
use crate::consts::{
    CARET_WIDTH, FIND_ROW_H, FONT_SIZE, LINE_HEIGHT, MAX_CONTENT_WIDTH, OUTLINE_WIDTH, PADDING,
    STATUS_BAR_H, WHEEL_LINE_STEP,
};
use crate::core::{
    AutocompleteSuggestion, AutocompleteTrigger, Editor, FieldFocus, FindMode, WatchKind,
};
use crate::doc_layout::{
    DocLayout, GithubRenderData, HeightCache, LayoutParams, LineCache, PreeditView, RenderCache,
    ScreenRect, TableCache,
};
use crate::editor::{Direction, EditorTheme};
use crate::git::{detect_github_context, head_blob_text, parse_github_repo_string};
use crate::github::{GitHubClient, ValidationResult};
use crate::image_cache::ImageCache;
use crate::image_load::{RepaintSignal, load_local_images_blocking, spawn_image_loads};
use crate::inline::{GitHubContext, GitHubRef};
#[cfg(feature = "math")]
use crate::math;
#[cfg(feature = "mermaid")]
use crate::mermaid;
use crate::outline::current_heading_index;
use crate::overlay::{
    HoverTarget, draw_autocomplete, draw_hover_popover, find_hover_target, hover_target_at_offset,
};
use crate::raster::rasterize_scene_to_png;
use crate::scrollbar;

/// Frame profiling, enabled with `MARKDONE_PROFILE=1`: one stderr line per frame with the
/// number/cost of layout rebuilds since the previous frame and the frame phase timings.
/// `MARKDONE_SCROLL_TRACE=1`: log every wheel step, scrollbar drag and deferred rebuild
/// with the viewport, scroll position, anchor and materialized band — enough to replay a
/// scroll problem deterministically in a test.
mod scroll_trace {
    use std::sync::OnceLock;

    use crate::doc_layout::DocLayout;

    pub fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("MARKDONE_SCROLL_TRACE").is_some_and(|v| v != "0"))
    }

    pub fn log(tag: &str, doc: Option<&DocLayout>, w: f32, vh: f32, scale: f32) {
        if !enabled() {
            return;
        }
        let Some(doc) = doc else {
            eprintln!("[scroll] {tag} w={w} vh={vh} scale={scale} (no doc)");
            return;
        };
        eprintln!(
            "[scroll] {tag} w={w} vh={vh} scale={scale} lines={} scroll_y={:.1} max={:.1} \
             content_h={:.1} anchor={:?} visible={:?} band={:?} remeasure={}",
            doc.line_count(),
            doc.scroll_y,
            doc.max_scroll(vh),
            doc.content_height(),
            doc.scroll_anchor(),
            doc.visible_range(vh),
            doc.measured_band(),
            doc.needs_remeasure(vh),
        );
    }
}

mod profile {
    use std::cell::Cell;
    use std::sync::OnceLock;
    use std::time::Instant;

    thread_local! {
        static REBUILDS: Cell<(u32, u64)> = const { Cell::new((0, 0)) };
    }

    pub fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("MARKDONE_PROFILE").is_some_and(|v| v != "0"))
    }

    pub fn record_rebuild(start: Instant) {
        if enabled() {
            let us = start.elapsed().as_micros() as u64;
            REBUILDS.with(|c| {
                let (n, t) = c.get();
                c.set((n + 1, t + us));
            });
        }
    }

    /// Rebuilds (count, total µs) since the last call, resetting the counter.
    pub fn take_rebuilds() -> (u32, u64) {
        REBUILDS.with(|c| c.replace((0, 0)))
    }

    thread_local! {
        /// Arrival time of the oldest input event not yet shown on screen.
        static PENDING_INPUT: Cell<Option<Instant>> = const { Cell::new(None) };
    }

    /// Times one input event's handling (printed on drop, so early returns count too)
    /// and marks it as awaiting a frame for the input-to-present latency.
    pub struct EventTimer {
        label: &'static str,
        start: Instant,
        rebuilds_before: (u32, u64),
    }

    impl EventTimer {
        pub fn new(label: &'static str) -> Option<Self> {
            if !enabled() {
                return None;
            }
            let start = Instant::now();
            PENDING_INPUT.with(|c| {
                if c.get().is_none() {
                    c.set(Some(start));
                }
            });
            Some(Self {
                label,
                start,
                rebuilds_before: REBUILDS.with(|c| c.get()),
            })
        }
    }

    impl Drop for EventTimer {
        fn drop(&mut self) {
            let (n, us) = REBUILDS.with(|c| c.get());
            eprintln!(
                "[markdone-event] {:<10} handler {:7.2} ms (rebuilds {} = {:.2} ms)",
                self.label,
                self.start.elapsed().as_secs_f64() * 1e3,
                n - self.rebuilds_before.0,
                (us - self.rebuilds_before.1) as f64 / 1e3,
            );
        }
    }

    /// Latency (ms) from the oldest unpresented input event to now, clearing it.
    /// Benchmark aid: `MARKDONE_AUTOSCROLL=<device px per frame>` scrolls the document to
    /// the bottom by itself (through the normal wheel path), then logs once and stops.
    pub fn autoscroll_step() -> Option<f32> {
        static STEP: OnceLock<Option<f32>> = OnceLock::new();
        *STEP.get_or_init(|| {
            std::env::var("MARKDONE_AUTOSCROLL")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|v| *v > 0.0)
        })
    }

    pub fn take_input_latency() -> Option<f64> {
        PENDING_INPUT
            .with(|c| c.take())
            .map(|t| t.elapsed().as_secs_f64() * 1e3)
    }
}
use crate::commands::Command;
use crate::text_engine::{TextEngine, peniko_color, peniko_color_alpha};
use crate::text_input;
use crate::links::{self, LinkTarget};
use crate::session::Session;
use crate::single_instance::{self, Instance};
use crate::ui::UiLayer;
use crate::ui::dialogs::{self, Answer, DialogView};
use crate::ui::preferences;
use crate::ui::toc::{self, TocEntry};
use crate::ui::menu::{MenuState, menu_bar};
use crate::ui::tabs::{TabInfo, tab_strip};
use crate::validation::{GitHubValidationCache, IssueOrPr, MentionableUser, ValidatedRefData};

/// Chrome layout in device px: y where editor content begins, and its height. The
/// bottom is inset by the status bar plus the find bar (`find_h`, 0 when closed), so
/// the document viewport reflows above both.
fn chrome_metrics(scale: f32, height_dev: f32, find_h: f32) -> (f32, f32) {
    // The title bar is the native window decoration now, so editor content starts at
    // the surface top; only the bottom strips are inset.
    let content_top = 0.0;
    let editor_h = (height_dev - STATUS_BAR_H * scale - find_h).max(1.0);
    (content_top, editor_h)
}

/// Width of the table of contents panel, logical px (f32 bits). Global because every
/// layout path reads it through [`outline_width`]; changed by dragging the panel edge.
static TOC_WIDTH: AtomicU32 = AtomicU32::new(OUTLINE_WIDTH.to_bits());

fn toc_width() -> f32 {
    f32::from_bits(TOC_WIDTH.load(Ordering::Relaxed))
}

fn set_toc_width(width: f32) {
    TOC_WIDTH.store(width.clamp(toc::MIN_W, toc::MAX_W).to_bits(), Ordering::Relaxed);
}

/// Device-px width of the right-docked table of contents (0 when closed). Mirrors
/// `find_bar_height`: the document region insets by this so it never draws under the panel.
fn outline_width(editor: &Editor, scale: f32) -> f32 {
    if editor.outline_open() {
        (toc_width() * scale).round()
    } else {
        0.0
    }
}

/// A small "Exporting…" card with a spinner at the bottom right, above the status bar.
fn export_indicator(ctx: &egui::Context, name: &str, status_bar_h: f32) {
    let corner = ctx.content_rect().right_bottom() - egui::vec2(12.0, status_bar_h + 10.0);
    egui::Area::new(egui::Id::new("markdone-export"))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::RIGHT_BOTTOM)
        .fixed_pos(corner)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label(format!("Exporting “{name}”…"));
                });
            });
        });
}

/// Where Up/Down should put the caret: the next/previous visual row at the remembered
/// x. Within a wrapped paragraph that is its next/previous row; past its first/last row,
/// the adjacent visible line's first/last row. `None` = no layout to go by (the caller
/// falls back to moving by source line).
fn visual_vertical_target(
    engine: &DocEngine,
    goal: &mut Option<(f32, usize)>,
    down: bool,
    scale: f32,
) -> Option<usize> {
    let editor = &engine.editor;
    let doc = engine.doc.as_ref()?;
    let caret = editor.state.selection.head;
    let rect = doc.caret_rect(caret, CARET_WIDTH * scale)?;
    let x = match *goal {
        Some((x, at)) if at == caret => x,
        _ => rect.0 as f32,
    };
    *goal = Some((x, caret));
    let (top, bottom) = (rect.1 as f32, rect.3 as f32);
    let line = editor.line_of(caret);

    // Another row of the same source line?
    let probe = if down { bottom + 1.0 } else { top - 1.0 };
    if let Some(off) = doc.hit_test(x, probe)
        && editor.line_of(off) == line
        && let Some(r) = doc.caret_rect(off, CARET_WIDTH * scale)
        && (r.1 as f32 - top).abs() > 0.5
    {
        return Some(off);
    }
    // The adjacent visible line, entering at its first (down) or last (up) row.
    let hidden = editor.hidden_line_ranges();
    let visible = |l: usize| !hidden.iter().any(|r| r.contains(&l));
    let count = editor.line_count();
    let next = if down {
        (line + 1..count).find(|&l| visible(l))?
    } else {
        (0..line).rev().find(|&l| visible(l))?
    };
    let line_top = doc.line_top_screen(next)?;
    let y = if down {
        line_top + 1.0
    } else {
        line_top + (doc.line_text_height(next) - 1.0).max(1.0)
    };
    let off = doc.hit_test(x, y)?;
    // Only trust a hit on the intended line (estimated, not yet laid out lines may not
    // map); otherwise fall back.
    (editor.line_of(off) == next).then_some(off)
}

/// Extra left padding (logical px) for the line numbers: room for the widest number in
/// a monospace digit at the numbers' size, plus a gap.
fn line_number_gutter(font_size: f32, line_count: usize) -> f32 {
    let digits = line_count.max(1).ilog10() as f32 + 1.0;
    digits * line_number_size(font_size) * 0.62 + 8.0
}

/// Font size of the line numbers for a document font size.
fn line_number_size(font_size: f32) -> f32 {
    (font_size * 0.8).max(9.0)
}

/// Device-px height of the bottom find bar for the current find state (0 when closed):
/// one `FIND_ROW_H` row in Find mode, two in Replace mode, plus a small vertical pad.
fn find_bar_height(editor: &Editor, scale: f32) -> f32 {
    match editor.find_state() {
        None => 0.0,
        Some(find) => {
            let rows = if find.mode == FindMode::Replace {
                2.0
            } else {
                1.0
            };
            rows * FIND_ROW_H * scale + 8.0 * scale
        }
    }
}

/// The native window title: the file name with a `●` dirty marker, shown in the OS
/// decoration (we no longer draw our own title bar).
fn window_title(editor: &Editor) -> String {
    let name = doc_name(editor);
    if editor.is_dirty() {
        format!("● {name} — Markdone")
    } else {
        format!("{name} — Markdone")
    }
}

/// The body width cap for the layout: `None` (full width) or the configured maximum.
fn content_width(prefs: &Preferences) -> Option<f32> {
    (!prefs.full_width).then_some(prefs.max_width)
}

/// The document's display name: its file name, or "Untitled".
fn doc_name(editor: &Editor) -> String {
    editor
        .file_path()
        .and_then(Path::file_name)
        .map_or_else(|| "Untitled".to_string(), |n| n.to_string_lossy().into_owned())
}

/// Nearest heading level at or above `line`, if any.
fn heading_at(buffer: &Buffer, line: usize) -> Option<u8> {
    let headings = buffer.headings();
    current_heading_index(headings, line).map(|i| headings[i].level)
}

/// Gather the status-bar inputs from the editor + viewport.
fn build_status_info(editor: &Editor, doc: &DocLayout, editor_h: f32) -> StatusInfo {
    let buffer = &editor.state.buffer;
    let cursor = editor.cursor_position();
    let line = buffer.byte_to_line(cursor);
    let line_start = buffer.line_to_byte(line);
    // Column counts grapheme clusters, not codepoints, so a multi-codepoint character
    // (emoji ZWJ sequence, combining accent) advances the column by one, matching what
    // the user sees on screen.
    let col = buffer.slice_cow(line_start..cursor).graphemes(true).count() + 1;
    let (first, last) = doc.visible_range(editor_h);
    StatusInfo {
        context: editor.state.build_nested_context(cursor),
        heading_level: heading_at(buffer, line),
        cursor_line: line + 1,
        cursor_col: col,
        total_lines: buffer.line_count(),
        first_visible: first,
        last_visible: last.saturating_sub(1),
    }
}

/// Sample document shown until the shell loads real files (Phase 4+ wires input).
/// The `--demo` (and no-args) showcase document — exercises headings, emphasis,
/// inline code, task lists, a colored code fence, a blockquote, and links.
const DEMO_DOC: &str = r##"# writ — a live Markdown diff viewer

**writ** renders Markdown as you type, and shows a live *inline diff* against
git `HEAD`: added lines glow green, deletions appear as ghost rows, and
word-level changes highlight in place. Point it at a file an agent is editing and
watch the changes land in real time.

## Features

- [x] ~~GPU-rendered Markdown (winit + Vello + Parley)~~
- [x] ~~Live inline `git HEAD` diff with word-level changes~~
- [x] ~~GitHub `#123` / `@user` refs — validated, with hover cards + autocomplete~~
- [ ] GFM tables
- [ ] Multi-cursor

## A code block

```rust
fn main() {
    let editor = Editor::new("# hello");
    println!("{}", editor.text());
}
```

> Ctrl-click any link or `#ref` to open it in your browser.

Try editing this file — every keystroke re-renders and re-diffs against HEAD.
"##;

/// A synthetic "git HEAD" version of `DEMO_DOC` so `--demo` shows the live inline
/// diff (added/deleted/changed lines) without needing a real git repo.
const DEMO_BASE: &str = r##"# writ — a Markdown viewer

**writ** renders Markdown as you type. Point it at a file and read it nicely.

## Features

- GPU-rendered Markdown
- This line is removed in the working copy, so it renders as a deleted ghost row.

## A code block

```rust
fn main() {
    let editor = Editor::new("# hello");
    println!("{}", editor.text());
}
```

Try editing this file.
"##;

/// The demo editor: the showcase doc with a synthetic HEAD base so the inline diff
/// (the headline feature) is visible on first open.
fn demo_editor() -> Editor {
    let mut editor = Editor::new(DEMO_DOC);
    editor.set_head_base(DEMO_BASE);
    editor
}

/// `(new file content, git HEAD blob text)` read off-thread for a file reload.
type ReloadData = (String, Option<String>);

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReloadTicket {
    file_generation: u64,
    base_generation: u64,
    buffer_version: u64,
    save_generation: u64,
}

#[derive(Default)]
struct ReloadState {
    file_generation: u64,
    base_generation: u64,
}

impl ReloadState {
    fn begin_file(&mut self, editor: &Editor) -> ReloadTicket {
        self.file_generation += 1;
        ReloadTicket {
            file_generation: self.file_generation,
            base_generation: self.base_generation,
            buffer_version: editor.state.buffer.version(),
            save_generation: editor.save_generation(),
        }
    }

    /// Whether `ticket` belongs to the most recent file read (no newer read started).
    fn is_latest(&self, ticket: ReloadTicket) -> bool {
        ticket.file_generation == self.file_generation
    }

    fn begin_base(&mut self) -> u64 {
        self.base_generation += 1;
        self.base_generation
    }

    fn apply_file(&self, editor: &mut Editor, ticket: ReloadTicket, data: ReloadData) -> bool {
        // A slow disk read must not overwrite a newer read or edits made while it ran.
        if ticket.file_generation != self.file_generation
            || ticket.buffer_version != editor.state.buffer.version()
            || ticket.save_generation != editor.save_generation()
        {
            return false;
        }
        editor.apply_file_content(data.0);
        self.apply_base(editor, ticket.base_generation, data.1);
        true
    }

    fn apply_base(&self, editor: &mut Editor, generation: u64, base: Option<String>) -> bool {
        if generation != self.base_generation {
            return false;
        }
        editor.apply_git_base(base);
        true
    }
}

/// Shared slot a debounced autocomplete fetch drops its results into for the main thread.
type AcSlot = Arc<Mutex<Option<FetchedSuggestions>>>;

#[derive(Debug, Clone)]
pub(crate) enum WritEvent {
    GithubUpdated,
    /// A standalone image finished loading (local or remote). A load changes a line's
    /// height, so the loop rebuilds (not just redraws) to reflow around it.
    ImageLoaded,
    /// The watched file of document `id` changed on disk (forwarded from its watcher
    /// thread). The loop kicks off a blocking read off-thread rather than touching disk
    /// here; for a background tab it only marks the tab stale (reloaded on activation).
    FileChanged(u64),
    /// A watched git dir of document `id` moved HEAD; refresh its diff base.
    GitBaseChanged(u64),
    FileReloaded(ReloadTicket, ReloadData),
    GitBaseReloaded(u64, Option<String>),
    /// The Save As dialog for tab `id` closed (`path` = `None`: cancelled). `then` is the
    /// close request that asked for the save, resumed once the document is saved.
    SaveAsPicked {
        id: u64,
        path: Option<PathBuf>,
        then: Option<CloseRequest>,
    },
    /// The Open dialog closed with these files (empty: cancelled).
    OpenPicked(Vec<PathBuf>),
    /// Another launch handed over these files (empty: just raise the window).
    InstanceOpen(Vec<PathBuf>),
    /// Files dropped on the window (Wayland; winit reports them itself elsewhere).
    FilesDropped(Vec<PathBuf>),
    /// The Insert → Image dialog for tab `id` closed (`None`: cancelled).
    ImagePicked { id: u64, path: Option<PathBuf> },
    /// The export dialog for tab `id` closed (`None`: cancelled).
    ExportPicked { id: u64, path: Option<PathBuf> },
    /// A PDF export finished: the file, or an error for the user.
    ExportDone(Result<PathBuf, String>),
}

/// Tabs to close (by id), and whether the application quits once they are closed.
#[derive(Debug, Clone)]
pub(crate) struct CloseRequest {
    ids: Vec<u64>,
    quit: bool,
}

/// A dialog waiting for the user, with the state needed to act on the answer.
enum PendingModal {
    /// Some tabs of `req` have unsaved changes (`dirty`, by id in tab order).
    Unsaved { req: CloseRequest, dirty: Vec<u64> },
    Message { title: String, text: String },
    /// A PDF export finished.
    Exported { path: PathBuf },
    /// Reloading tab `id` would discard its unsaved changes.
    ConfirmReload { id: u64 },
}

/// A tab's file changed on disk and the user must choose between the disk version
/// (`data`) and the buffer. Kept on the tab: the question shows whenever it is active.
struct DiskConflict {
    ticket: ReloadTicket,
    data: ReloadData,
}

/// Concrete `RepaintSignal` for the winit shell: wakes the loop with `ImageLoaded` so a
/// finished background image load reflows the doc around its now-known height.
#[derive(Clone)]
struct RedrawOnImageLoad(EventLoopProxy<WritEvent>);

impl RepaintSignal for RedrawOnImageLoad {
    fn notify(&self) {
        let _ = self.0.send_event(WritEvent::ImageLoaded);
    }
}

/// Autocomplete results a tokio task fetched, handed back to the main thread (via a
/// shared slot) where they're turned into `AutocompleteSuggestion`s and installed.
enum FetchedSuggestions {
    Issues {
        prefix: String,
        issues: Vec<IssueOrPr>,
    },
    Users {
        prefix: String,
        users: Vec<MentionableUser>,
    },
}

struct ActiveSurface {
    surface: RenderSurface<'static>,
    window: Arc<Window>,
    scale: f32,
    /// Current find-bar height (device px), 0 when the bar is closed. Kept here so the
    /// shared `viewport()` accessor can shrink the editor height without threading the
    /// editor's find state through its ten call sites; synced by `sync_find_chrome`.
    find_bar_h: f32,
    /// Whether the egui menu bar is shown (off with `MARKDONE_NO_UI=1`, for A/B measurements).
    menu_bar: bool,
}

struct SelectionDrag {
    origin: (f32, f32),
    position: (f32, f32),
    threshold: f32,
    active: bool,
    pending: bool,
}

impl SelectionDrag {
    fn new(position: (f32, f32), scale: f32) -> Self {
        Self {
            origin: position,
            position,
            threshold: 5.0 * scale,
            active: false,
            pending: false,
        }
    }

    fn update(&mut self, position: (f32, f32)) -> bool {
        if position == self.position {
            return false;
        }
        self.position = position;
        // Revealing raw Markdown can move text beneath a stationary pointer. Only
        // physical travel from the press starts selection, independent of relayout.
        if !self.active
            && (position.0 - self.origin.0).hypot(position.1 - self.origin.1) < self.threshold
        {
            return false;
        }
        self.active = true;
        self.pending = true;
        true
    }

    fn take_pending(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }
}

struct FrameRetry {
    deadline: Option<Instant>,
    delay: Duration,
}

impl Default for FrameRetry {
    fn default() -> Self {
        Self {
            deadline: None,
            delay: Duration::from_millis(100),
        }
    }
}

impl FrameRetry {
    fn schedule(&mut self, now: Instant) {
        self.deadline = Some(now + self.delay);
        // Hidden windows can remain occluded indefinitely. Keep probing without
        // rendering continuously; Wayland does not emit winit's Occluded event.
        self.delay = (self.delay * 2).min(Duration::from_secs(1));
    }

    fn take_due(&mut self, now: Instant) -> bool {
        if self.deadline.is_some_and(|deadline| deadline <= now) {
            self.deadline = None;
            true
        } else {
            false
        }
    }

    fn control_flow(&self, other_deadline: Option<Instant>) -> ControlFlow {
        self.deadline
            .into_iter()
            .chain(other_deadline)
            .min()
            .map_or(ControlFlow::Wait, ControlFlow::WaitUntil)
    }
}

impl ActiveSurface {
    /// Height of the egui menu bar at the top (device px). The document scene is drawn
    /// shifted down by this, and pointer positions are shifted up by it, so all the
    /// document/chrome code below keeps its origin at the top of the document area.
    fn top_inset(&self) -> f32 {
        if self.menu_bar {
            UiLayer::top_inset(self.scale)
        } else {
            0.0
        }
    }

    /// Height of the area below the menu bar (device px): what the document code sees
    /// as the surface height.
    fn doc_height(&self) -> f32 {
        (self.surface.config.height as f32 - self.top_inset()).max(1.0)
    }

    /// The editor viewport in device px: (surface width, editor content height, scale).
    /// The content height excludes the bottom status-bar and find-bar strips.
    fn viewport(&self) -> (f32, f32, f32) {
        let (_, editor_h) = chrome_metrics(self.scale, self.doc_height(), self.find_bar_h);
        (self.surface.config.width as f32, editor_h, self.scale)
    }
}

/// Re-sync the find-bar inset to the editor's current find state and reflow the document
/// at the new editor height. The single sync point named by `find_bar_h`'s doc comment —
/// every open/close/scale-change path routes through it so the inset can't drift. Takes
/// `doc_engine` and `state` separately (they're disjoint `App` fields) so the reflow's
/// `&mut DocEngine` and the surface's `&mut ActiveSurface` don't alias.
fn sync_find_chrome(doc_engine: &mut DocEngine, state: &mut ActiveSurface) {
    state.find_bar_h = find_bar_height(&doc_engine.editor, state.scale);
    let (w, editor_h, scale) = state.viewport();
    doc_engine.rebuild_preserving_scroll(w, scale, editor_h);
}

/// In-progress IME composition (owned form of [`PreeditView`]): the composing `text`
/// and its caret/selection `cursor` (byte offsets within `text`). Held on `DocEngine`
/// so `rebuild` can splice it into the caret line without threading a param everywhere.
struct Preedit {
    text: String,
    cursor: Option<(usize, usize)>,
}

/// The document engine: the editor/buffer plus the caches and Parley/Vello text
/// engine that lay it out into `doc`. Held as one field kept SEPARATE from `state`
/// (the GPU surface) so `&mut self.doc_engine` and `&self.state` are disjoint
/// borrows — the split that lets a rebuild run while the surface stays borrowed at
/// the call site. The per-line caches + `Rc`-wrapped layouts/renders avoid deep
/// clones on the typing hot path.
struct DocEngine {
    text_engine: TextEngine,
    line_cache: LineCache,
    render_cache: RenderCache,
    height_cache: HeightCache,
    table_cache: TableCache,
    theme: EditorTheme,
    /// Base document font size (logical px), from config; defaults to `FONT_SIZE`.
    font_size: f32,
    /// Maximum body width (logical px); `None` = full window width.
    max_content_width: Option<f32>,
    /// Line numbers are shown (the left padding makes room for them).
    line_numbers: bool,
    /// The git differences are shown (when the file is in a repository).
    git_diff: bool,
    editor: Editor,
    doc: Option<DocLayout>,
    /// Shared cache of decoded standalone images, threaded into `DocLayout::build`.
    images: ImageCache,
    /// Active IME composition, spliced into the caret line at render time. `None` when
    /// not composing.
    preedit: Option<Preedit>,
}

/// The per-document half of [`DocEngine`]: everything except the resources shared by
/// all tabs (fonts/text engine, theme, font size). The active document's state lives in
/// `App::doc_engine` (so every existing code path keeps working on "the" document);
/// background tabs park theirs here and are swapped in on activation.
struct DocState {
    editor: Editor,
    doc: Option<DocLayout>,
    images: ImageCache,
    preedit: Option<Preedit>,
    line_cache: LineCache,
    render_cache: RenderCache,
    height_cache: HeightCache,
    table_cache: TableCache,
}

impl DocState {
    fn new(editor: Editor) -> Self {
        Self {
            editor,
            doc: None,
            images: ImageCache::new(),
            preedit: None,
            line_cache: LineCache::new(),
            render_cache: RenderCache::new(),
            height_cache: HeightCache::new(),
            table_cache: TableCache::new(),
        }
    }

    /// Exchange this state with the engine's active document.
    fn swap_with(&mut self, engine: &mut DocEngine) {
        std::mem::swap(&mut self.editor, &mut engine.editor);
        std::mem::swap(&mut self.doc, &mut engine.doc);
        std::mem::swap(&mut self.images, &mut engine.images);
        std::mem::swap(&mut self.preedit, &mut engine.preedit);
        std::mem::swap(&mut self.line_cache, &mut engine.line_cache);
        std::mem::swap(&mut self.render_cache, &mut engine.render_cache);
        std::mem::swap(&mut self.height_cache, &mut engine.height_cache);
        std::mem::swap(&mut self.table_cache, &mut engine.table_cache);
    }

    /// Release what a background tab doesn't need: the layout and its caches (rebuilt
    /// on activation from the saved scroll anchor) and all decoded image pixels (sizes
    /// are kept, so the rebuilt layout has the same heights). The text, parse tree and
    /// undo history stay.
    fn park(&mut self) {
        self.doc = None;
        self.preedit = None;
        self.line_cache = LineCache::new();
        self.render_cache = RenderCache::new();
        self.height_cache = HeightCache::new();
        self.table_cache = TableCache::new();
        self.images.park();
    }
}

/// One open document tab.
struct Tab {
    /// Stable id, carried by its file-watch events.
    id: u64,
    /// `None` for the active tab (its state is in `App::doc_engine`).
    state: Option<DocState>,
    /// Scroll anchor (line, offset) saved when the tab went to the background.
    anchor: (usize, f32),
    /// The file / git base changed on disk while in the background.
    file_stale: bool,
    /// The file changed on disk and the user has not chosen which version to keep.
    conflict: Option<DiskConflict>,
    base_stale: bool,
}

/// Forward an editor's file-watch notifications into the loop, tagged with its tab id,
/// so the loop can park on `Wait` rather than polling. The thread ends when the watcher
/// (and its sender) drop, making `recv()` return `Err`.
fn forward_file_watch(editor: &mut Editor, id: u64, proxy: &EventLoopProxy<WritEvent>) {
    if let Some(rx) = editor.take_file_watch_rx() {
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            while let Ok(kind) = rx.recv() {
                let event = match kind {
                    WatchKind::File => WritEvent::FileChanged(id),
                    WatchKind::GitBase => WritEvent::GitBaseChanged(id),
                };
                if proxy.send_event(event).is_err() {
                    break;
                }
            }
        });
    }
}

struct App {
    context: RenderContext,
    // One renderer suffices; keyed to the surface's device.
    renderer: Option<Renderer>,
    state: Option<ActiveSurface>,
    scene: Scene,
    /// The document scene shifted below the menu bar (rebuilt from `scene` each frame).
    frame_scene: Scene,
    /// egui chrome: menu bar, menus, dialogs. Created with the surface.
    ui: Option<UiLayer>,
    doc_engine: DocEngine,
    /// Open documents in tab order; `tabs[active_tab]` is the one in `doc_engine`.
    tabs: Vec<Tab>,
    active_tab: usize,
    /// Id for the next tab opened (ids tag file-watch events).
    next_tab_id: u64,
    /// The dialog being shown, if any. While set, the document gets no input.
    modal: Option<PendingModal>,
    /// A native Open / Save As dialog is open (at most one at a time).
    file_dialog_open: bool,
    /// A PDF export is running: the output file name (shown with a spinner).
    export_running: Option<String>,
    /// Recent files (its `open`/`active` are recomputed from the tabs when saved).
    session: Session,
    /// Where the session is saved; `None` = not saved (`--demo`).
    session_path: Option<PathBuf>,
    /// The session as last written, to rewrite the file only when it changes.
    session_saved: Option<Session>,
    /// The preferences in effect.
    prefs: Preferences,
    /// Themes offered in the Preferences window.
    theme_names: Vec<String>,
    /// The config file preferences are saved to.
    config_path: Option<PathBuf>,
    /// The Preferences window is shown.
    prefs_open: bool,
    /// Preferences changed and not yet saved: saved at this time (debounced, so dragging
    /// a slider does not rewrite the file every frame).
    prefs_save_at: Option<Instant>,
    /// GitHub lookups for documents opened from now on.
    github: GithubSetup,
    modifiers: ModifiersState,
    mouse_pos: (f32, f32),
    selection_drag: Option<SelectionDrag>,
    /// The OS cursor icon currently set on the window, so hover updates only call
    /// `set_cursor` on a real change (Ctrl-hover over a link → pointer, else default).
    cursor_icon: CursorIcon,
    /// Click-count tracking for double/triple-click select (winit doesn't provide it):
    /// the last press's time + position, and the running count (cycles 1→2→3).
    last_click: Option<(std::time::Instant, (f32, f32))>,
    click_count: usize,
    /// Non-zero while a drag is in a top/bottom edge hot zone: the auto-scroll velocity
    /// in physical px/second. Drives continuous auto-scroll from `about_to_wait` so the
    /// selection keeps growing even when the pointer is held still at the edge.
    drag_scroll_dy: f32,
    /// Scrollbar thumb drag in progress: the pointer's offset from the thumb top (device
    /// px) at grab time, so the thumb doesn't jump under the pointer.
    scrollbar_drag: Option<f32>,
    /// Pointer over the scrollbar hit zone (drives the stronger thumb tint).
    scrollbar_hover: bool,
    /// Scrollbar drag motion not yet applied: applied once per frame in RedrawRequested.
    scrollbar_pending: bool,
    /// Wheel scroll (device px) accumulated since the last frame, applied once per frame.
    wheel_pending_dy: f32,
    /// Ctrl+wheel zoom not yet applied, in steps (fractions from touchpad pixel deltas).
    wheel_zoom_acc: f32,
    /// `MARKDONE_AUTOSCROLL` benchmark run finished (reached the bottom).
    autoscroll_done: bool,
    /// Tab switch requested from inside a redraw (where `state` is borrowed), applied
    /// in `about_to_wait`. Used by the `MARKDONE_AUTOTABS` benchmark.
    pending_tab_switch: Option<usize>,
    /// `MARKDONE_AUTOTABS`: the final switch back to the first tab has been queued.
    autotabs_returning: bool,
    /// `MARKDONE_AUTOTABS=<rounds>`: the current round (1-based).
    autotabs_round: u32,
    /// Timestamp of the last auto-scroll tick, so scrolling integrates against real
    /// elapsed time (frame-rate independent) rather than a fixed amount per tick.
    last_drag_tick: Option<std::time::Instant>,
    /// Set by async completions (validation/image); the next redraw does a single
    /// rebuild, coalescing many same-frame completions into one relayout.
    pending_rebuild: bool,
    frame_retry: FrameRetry,
    reloads: ReloadState,
    /// System clipboard for copy/cut/paste. Held for the app's lifetime (on Wayland the
    /// instance keeps serving the copied data). `None` if the platform init failed.
    clipboard: Option<arboard::Clipboard>,
    /// Last title pushed to the native window (filename + dirty marker); re-set only
    /// when it changes, so the OS decoration carries the document name.
    title: String,
    /// GitHub ref under the pointer, if any (drives the hover popover).
    hovered: Option<HoverTarget>,
    /// Screen rects of the currently-drawn autocomplete rows (for click routing).
    ac_row_rects: Vec<ScreenRect>,
    /// The document's right-click menu: where it opens (points), and whether it must
    /// open on the next frame.
    context_menu: Option<egui::Pos2>,
    context_menu_open: bool,
    /// A right button press on the document waits for its release to open the menu.
    context_menu_pending: bool,
    /// Wayland file drag-and-drop listener (stopped in `exiting`).
    #[cfg(all(unix, not(target_os = "macos")))]
    dnd: Option<crate::wayland_dnd::Listener>,
    /// Up/Down column memory: the x (device px) to aim for, valid while the caret is
    /// still at the offset the last vertical move left it.
    visual_goal: Option<(f32, usize)>,
    /// Table of contents: its width (logical px) as saved in the session (updated when
    /// a resize drag ends), and the current heading last frame (a change scrolls the
    /// list to it).
    toc_width: f32,
    toc_current: Option<usize>,
    /// Fold chevron hit rects (heading byte offset → gutter rect) from the last paint.
    fold_chevron_rects: Vec<(usize, ScreenRect)>,
    /// Heading (by byte offset) whose gutter chevron is under the pointer, if any. Drives
    /// hover-reveal of the expanded (▾) chevron on non-folded headings.
    gutter_hover: Option<usize>,
    /// Screen rects of the find bar's Replace/All buttons (for click routing), when the
    /// bar is open in Replace mode.
    find_btn_rects: Option<FindButtonRects>,
    /// Monotonic generation for autocomplete-fetch debounce (latest wins).
    ac_gen: Arc<AtomicU64>,
    /// Slot a finished fetch task drops its results into for the main thread.
    ac_slot: AcSlot,
    /// Wakeup channel into the winit loop for finished tokio work.
    proxy: EventLoopProxy<WritEvent>,
    /// Handle to the process tokio runtime for spawning GitHub work.
    runtime: tokio::runtime::Handle,
}

impl App {
    fn new(
        startup: Startup,
        proxy: EventLoopProxy<WritEvent>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let Startup {
            editors,
            active,
            missing,
            config,
            session,
            session_path,
            config_path,
            github,
            instance,
        } = startup;
        if let Some(listener) = instance {
            let proxy = proxy.clone();
            single_instance::serve(listener, move |paths| {
                let _ = proxy.send_event(WritEvent::InstanceOpen(paths));
            });
        }
        let active = active.min(editors.len() - 1);
        let mut editor = None;
        let mut tabs = Vec::with_capacity(editors.len());
        for (i, mut doc) in editors.into_iter().enumerate() {
            let id = i as u64;
            forward_file_watch(&mut doc, id, &proxy);
            // The active document lives in `doc_engine`; the others are parked.
            let state = if i == active {
                editor = Some(doc);
                None
            } else {
                Some(DocState::new(doc))
            };
            tabs.push(Tab {
                id,
                state,
                anchor: (0, 0.0),
                file_stale: false,
                base_stale: false,
                conflict: None,
            });
        }
        let mut editor = editor.expect("at least one document");
        editor.set_outline_open(session.toc_open);
        if let Some(width) = session.toc_width {
            set_toc_width(width);
        }
        let next_tab_id = tabs.len() as u64;
        let title = window_title(&editor);
        // Apply the configured font family, but only if it renders fixed-pitch — a
        // proportional (or unloadable) family would break writ's deterministic wrapping.
        let mut text_engine = TextEngine::new();
        if let Some(family) = config.font_family {
            text_engine.set_font_family(Some(family.clone()));
            if !text_engine.is_monospace() {
                eprintln!("[markdone] config: font '{family}' is not monospace; using the default");
                text_engine.set_font_family(None);
            }
        }
        Self {
            context: RenderContext::new(),
            renderer: None,
            state: None,
            scene: Scene::new(),
            frame_scene: Scene::new(),
            ui: None,
            doc_engine: DocEngine {
                text_engine,
                line_cache: LineCache::new(),
                render_cache: RenderCache::new(),
                height_cache: HeightCache::new(),
                table_cache: TableCache::new(),
                theme: config.theme,
                font_size: config.font_size,
                max_content_width: content_width(&config.prefs),
                line_numbers: config.prefs.line_numbers,
                git_diff: config.prefs.git_diff,
                editor,
                doc: None,
                images: ImageCache::new(),
                preedit: None,
            },
            tabs,
            active_tab: active,
            next_tab_id,
            session,
            session_path,
            session_saved: None,
            prefs: config.prefs.clone(),
            theme_names: config.theme_names.clone(),
            config_path,
            prefs_open: false,
            prefs_save_at: None,
            github,
            modal: missing_files_notice(&missing),
            file_dialog_open: false,
            export_running: None,
            modifiers: ModifiersState::empty(),
            mouse_pos: (0.0, 0.0),
            selection_drag: None,
            cursor_icon: CursorIcon::Default,
            last_click: None,
            click_count: 0,
            drag_scroll_dy: 0.0,
            scrollbar_drag: None,
            scrollbar_hover: false,
            scrollbar_pending: false,
            wheel_pending_dy: 0.0,
            wheel_zoom_acc: 0.0,
            autoscroll_done: false,
            pending_tab_switch: None,
            autotabs_returning: false,
            autotabs_round: 1,
            last_drag_tick: None,
            pending_rebuild: false,
            frame_retry: FrameRetry::default(),
            reloads: ReloadState::default(),
            clipboard: arboard::Clipboard::new().ok(),
            title,
            hovered: None,
            ac_row_rects: Vec::new(),
            #[cfg(all(unix, not(target_os = "macos")))]
            dnd: None,
            visual_goal: None,
            context_menu: None,
            context_menu_open: false,
            context_menu_pending: false,
            toc_width: toc_width(),
            toc_current: None,
            fold_chevron_rects: Vec::new(),
            gutter_hover: None,
            find_btn_rects: None,
            ac_gen: Arc::new(AtomicU64::new(0)),
            ac_slot: Arc::new(Mutex::new(None)),
            proxy,
            runtime,
        }
    }

    /// Make tab `i` the active document: park the current one (layout and image pixels
    /// released, scroll anchor saved), swap the target's state in and lay it out at its
    /// saved anchor. Per-view pointer state is reset; a reload missed while in the
    /// background is replayed.
    fn switch_tab(&mut self, i: usize) {
        if i == self.active_tab || i >= self.tabs.len() {
            return;
        }
        let Some(mut incoming) = self.tabs[i].state.take() else {
            return;
        };
        if profile::enabled() {
            eprintln!(
                "[markdone-mem] switch tab {} -> {i} | {}",
                self.active_tab,
                allocator_stats()
            );
        }
        let old = self.active_tab;
        let anchor = self
            .doc_engine
            .doc
            .as_ref()
            .map_or((0, 0.0), |d| d.scroll_anchor());
        // The table of contents is a window-wide panel: it stays as it was.
        let toc_open = self.doc_engine.editor.outline_open();
        incoming.swap_with(&mut self.doc_engine);
        self.doc_engine.editor.set_outline_open(toc_open);
        incoming.park();
        self.doc_engine.images.unpark();
        self.tabs[old].state = Some(incoming);
        self.tabs[old].anchor = anchor;
        self.active_tab = i;

        self.hovered = None;
        self.selection_drag = None;
        self.scrollbar_drag = None;
        self.scrollbar_pending = false;
        self.wheel_pending_dy = 0.0;
        self.drag_scroll_dy = 0.0;
        self.fold_chevron_rects.clear();
        self.gutter_hover = None;
        self.find_btn_rects = None;

        if let Some(state) = self.state.as_mut() {
            state.find_bar_h = find_bar_height(&self.doc_engine.editor, state.scale);
            let (w, vh, scale) = state.viewport();
            let (line, off) = self.tabs[i].anchor;
            let mut doc = self.doc_engine.rebuild(w, scale, line, vh);
            doc.scroll_y = doc.anchor_scroll_y(line, off);
            doc.clamp_scroll(vh);
            self.doc_engine.doc = Some(doc);
            // Re-detect over the restored viewport and relayout in place.
            self.doc_engine.rebuild_preserving_scroll(w, scale, vh);
            state.window.request_redraw();
        }
        self.spawn_validations();

        let id = self.tabs[i].id;
        if std::mem::take(&mut self.tabs[i].file_stale) {
            let _ = self.proxy.send_event(WritEvent::FileChanged(id));
        } else {
            self.check_active_on_disk();
        }
        if std::mem::take(&mut self.tabs[i].base_stale) {
            let _ = self.proxy.send_event(WritEvent::GitBaseChanged(id));
        }
    }

    /// The editor of tab `i` (the active one lives in `doc_engine`).
    fn tab_editor(&self, i: usize) -> &Editor {
        self.tabs[i]
            .state
            .as_ref()
            .map_or(&self.doc_engine.editor, |s| &s.editor)
    }

    fn tab_editor_mut(&mut self, i: usize) -> &mut Editor {
        match self.tabs[i].state.as_mut() {
            Some(s) => &mut s.editor,
            None => &mut self.doc_engine.editor,
        }
    }

    /// Index of the tab with id `id`, if still open.
    fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn redraw(&self) {
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }

    /// Close the tabs of `req` (and quit, if asked), first asking what to do with
    /// unsaved changes. Re-entrant: after a Save As the request is simply re-run.
    fn request_close(&mut self, req: CloseRequest, event_loop: &ActiveEventLoop) {
        let dirty: Vec<u64> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, t)| req.ids.contains(&t.id) && self.tab_editor(*i).is_dirty())
            .map(|(_, t)| t.id)
            .collect();
        // Without the egui layer (`MARKDONE_NO_UI`, benchmarks) there is no way to ask.
        if dirty.is_empty() || self.ui.is_none() {
            self.finish_close(req, event_loop);
            return;
        }
        self.modal = Some(PendingModal::Unsaved { req, dirty });
        self.redraw();
    }

    fn finish_close(&mut self, req: CloseRequest, event_loop: &ActiveEventLoop) {
        if req.quit {
            event_loop.exit();
        } else {
            self.remove_tabs(&req.ids);
        }
    }

    /// Drop the tabs with these ids (their file watchers stop with them). If the active
    /// tab goes, its nearest surviving neighbour (right first) is activated; if none
    /// survives, an empty untitled tab takes its place, so the window stays open.
    fn remove_tabs(&mut self, ids: &[u64]) {
        let closing = |t: &Tab| ids.contains(&t.id);
        let active = self.active_tab;
        if closing(&self.tabs[active]) {
            let n = self.tabs.len();
            let next = (active + 1..n)
                .chain((0..active).rev())
                .find(|&i| !closing(&self.tabs[i]));
            match next {
                Some(i) => self.switch_tab(i),
                None => self.open_blank_tab(),
            }
        }
        let active_id = self.tabs[self.active_tab].id;
        self.tabs.retain(|t| !closing(t));
        self.active_tab = self.tab_index(active_id).expect("active tab survives");
        self.redraw();
    }

    fn close_tab(&mut self, i: usize, event_loop: &ActiveEventLoop) {
        if let Some(tab) = self.tabs.get(i) {
            let req = CloseRequest {
                ids: vec![tab.id],
                quit: false,
            };
            self.request_close(req, event_loop);
        }
    }

    fn show_message(&mut self, title: &str, text: String) {
        self.modal = Some(PendingModal::Message {
            title: title.to_string(),
            text,
        });
        self.redraw();
    }

    /// Save tab `i` to its file. An untitled document asks for a path instead (the save
    /// then completes asynchronously). Returns `true` only if the document was saved.
    fn save_tab(&mut self, i: usize) -> bool {
        if self.tab_editor(i).file_path().is_none() {
            self.start_save_as(i, None);
            return false;
        }
        match self.tab_editor_mut(i).save() {
            Ok(()) => {
                self.tabs[i].conflict = None;
                true
            }
            Err(e) => {
                let name = doc_name(self.tab_editor(i));
                self.show_message("Could not save", format!("Saving “{name}” failed: {e}"));
                false
            }
        }
    }

    /// Directory a file dialog starts in: the folder of tab `i`'s file, else the
    /// working directory.
    fn dialog_dir(&self, i: usize) -> Option<PathBuf> {
        self.tab_editor(i)
            .file_path()
            .and_then(|p| std::path::absolute(p).ok())
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .or_else(|| std::env::current_dir().ok())
    }

    /// Ask for a path for tab `i` and save it there. The native dialog runs on its own
    /// thread (the loop keeps drawing); the answer comes back as `SaveAsPicked`.
    fn start_save_as(&mut self, i: usize, then: Option<CloseRequest>) {
        if self.file_dialog_open {
            return;
        }
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let editor = self.tab_editor(i);
        let name = editor
            .file_path()
            .and_then(Path::file_name)
            .map_or_else(|| "untitled.md".to_string(), |n| n.to_string_lossy().into_owned());
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save As")
            .add_filter("Markdown", &["md", "markdown"])
            .set_file_name(name)
            .set_parent(&*state.window);
        if let Some(dir) = self.dialog_dir(i) {
            dialog = dialog.set_directory(dir);
        }
        let id = self.tabs[i].id;
        let proxy = self.proxy.clone();
        self.file_dialog_open = true;
        std::thread::spawn(move || {
            let path = dialog.save_file();
            let _ = proxy.send_event(WritEvent::SaveAsPicked { id, path, then });
        });
    }

    /// Re-read tab `id` (the active one) from disk, discarding its unsaved changes.
    fn reload_tab(&mut self, id: u64) {
        if self.tabs[self.active_tab].id != id {
            return;
        }
        let Some(path) = self.doc_engine.editor.file_path().map(Path::to_path_buf) else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let editor = &mut self.doc_engine.editor;
                editor.apply_file_content(content);
                editor.mark_clean();
                editor.note_disk_state();
                editor.refresh_git_base();
                self.tabs[self.active_tab].conflict = None;
                self.after_reload();
            }
            Err(e) => self.show_message(
                "Could not reload",
                format!("Reading {} failed: {e}", path.display()),
            ),
        }
    }

    /// Ask where to export tab `i` as PDF (answer comes back as `ExportPicked`).
    fn start_export(&mut self, i: usize) {
        if self.file_dialog_open {
            return;
        }
        if let Some(name) = &self.export_running {
            self.show_message(
                "Export in progress",
                format!("“{name}” is still being exported; try again when it is done."),
            );
            return;
        }
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let editor = self.tab_editor(i);
        let stem = editor
            .file_path()
            .and_then(Path::file_stem)
            .map_or_else(|| "untitled".to_string(), |s| s.to_string_lossy().into_owned());
        let mut dialog = rfd::FileDialog::new()
            .set_title("Export as PDF")
            .add_filter("PDF", &["pdf"])
            .set_file_name(format!("{stem}.pdf"))
            .set_parent(&*state.window);
        if let Some(dir) = self.dialog_dir(i) {
            dialog = dialog.set_directory(dir);
        }
        let id = self.tabs[i].id;
        let proxy = self.proxy.clone();
        self.file_dialog_open = true;
        std::thread::spawn(move || {
            let path = dialog.save_file();
            let _ = proxy.send_event(WritEvent::ExportPicked { id, path });
        });
    }

    /// Export tab `id` (its current text, unsaved changes included) to `path` on a
    /// worker thread; the result comes back as `ExportDone`.
    fn run_export(&mut self, id: u64, mut path: PathBuf) {
        let Some(i) = self.tab_index(id) else {
            return;
        };
        if path.extension().is_none() {
            path.set_extension("pdf");
        }
        let editor = self.tab_editor(i);
        let job = crate::export::ExportJob {
            markdown: editor.text(),
            base_dir: editor
                .file_path()
                .and_then(|p| std::path::absolute(p).ok())
                .and_then(|p| p.parent().map(Path::to_path_buf)),
            title: editor
                .file_path()
                .and_then(Path::file_stem)
                .map_or_else(|| "Untitled".to_string(), |s| s.to_string_lossy().into_owned()),
            output: path.clone(),
        };
        self.export_running = Some(
            path.file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        );
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = crate::export::export_pdf(&job).map(|()| job.output);
            let _ = proxy.send_event(WritEvent::ExportDone(result));
        });
        self.redraw();
    }

    /// Ask for files to open (answer comes back as `OpenPicked`).
    fn start_open(&mut self) {
        if self.file_dialog_open {
            return;
        }
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open")
            .add_filter("Markdown", &["md", "markdown", "mdown", "mkd"])
            .add_filter("Text", &["txt"])
            .set_parent(&*state.window);
        if let Some(dir) = self.dialog_dir(self.active_tab) {
            dialog = dialog.set_directory(dir);
        }
        let proxy = self.proxy.clone();
        self.file_dialog_open = true;
        std::thread::spawn(move || {
            let paths = dialog.pick_files().unwrap_or_default();
            let _ = proxy.send_event(WritEvent::OpenPicked(paths));
        });
    }

    /// The Save As dialog answered: save tab `id` at `path`, watch the new file, and
    /// resume the close request that asked for it, if any.
    fn finish_save_as(
        &mut self,
        id: u64,
        path: PathBuf,
        then: Option<CloseRequest>,
        event_loop: &ActiveEventLoop,
    ) {
        let Some(i) = self.tab_index(id) else {
            return;
        };
        let proxy = self.proxy.clone();
        let editor = self.tab_editor_mut(i);
        if let Err(e) = editor.save_as(path.clone()) {
            self.show_message(
                "Could not save",
                format!("Saving “{}” failed: {e}", path.display()),
            );
            return;
        }
        editor.refresh_git_base();
        if let Err(e) = editor.watch_file() {
            eprintln!("[markdone] file watch failed: {e}");
        }
        forward_file_watch(editor, id, &proxy);
        self.tabs[i].conflict = None;
        self.session.touch_recent(&path);
        self.redraw();
        if let Some(req) = then {
            self.request_close(req, event_loop);
        }
    }

    /// Open files in new tabs (a file already open just gets activated). A pristine
    /// untitled tab that was active is replaced.
    fn open_paths(&mut self, paths: Vec<PathBuf>) {
        let pristine = {
            let editor = &self.doc_engine.editor;
            (editor.file_path().is_none() && editor.is_empty() && !editor.is_dirty())
                .then(|| self.tabs[self.active_tab].id)
        };
        let mut last = None;
        for path in paths {
            let path = std::path::absolute(&path).unwrap_or(path);
            self.session.touch_recent(&path);
            let key = path.canonicalize().ok();
            let open = (0..self.tabs.len()).find(|&i| {
                key.is_some()
                    && self.tab_editor(i).file_path().and_then(|p| p.canonicalize().ok()) == key
            });
            if let Some(i) = open {
                last = Some(i);
                continue;
            }
            let id = self.next_tab_id;
            self.next_tab_id += 1;
            let mut editor = open_document(&path, &self.github, false);
            forward_file_watch(&mut editor, id, &self.proxy);
            self.tabs.push(Tab {
                id,
                state: Some(DocState::new(editor)),
                anchor: (0, 0.0),
                file_stale: false,
                base_stale: false,
                conflict: None,
            });
            last = Some(self.tabs.len() - 1);
        }
        let Some(i) = last else {
            return;
        };
        self.switch_tab(i);
        if let Some(blank) = pristine
            && blank != self.tabs[self.active_tab].id
        {
            self.remove_tabs(&[blank]);
        }
        self.redraw();
    }

    /// Apply new preferences: relayout / restyle what changed, and schedule the save.
    fn apply_preferences(&mut self, new: Preferences) {
        let old = std::mem::replace(&mut self.prefs, new);
        if old == self.prefs {
            return;
        }
        self.github.enabled = self.prefs.github;
        let mut relayout = false;
        if old.font_size != self.prefs.font_size {
            self.doc_engine.font_size = self.prefs.font_size;
            relayout = true;
        }
        if old.line_numbers != self.prefs.line_numbers {
            self.doc_engine.line_numbers = self.prefs.line_numbers;
            relayout = true;
        }
        if old.git_diff != self.prefs.git_diff {
            self.doc_engine.git_diff = self.prefs.git_diff;
            relayout = true;
        }
        if old.full_width != self.prefs.full_width || old.max_width != self.prefs.max_width {
            self.doc_engine.max_content_width = content_width(&self.prefs);
            relayout = true;
        }
        if old.theme != self.prefs.theme {
            // The theme may be a custom one of the config file: save, then resolve it.
            self.prefs_save_at = None;
            self.save_preferences();
            let theme = match self.config_path.as_deref() {
                Some(path) if path.exists() => config::load(Some(path)).theme,
                _ => EditorTheme::by_name(&self.prefs.theme).unwrap_or_else(EditorTheme::dracula),
            };
            self.set_theme(theme);
            relayout = true;
        } else {
            self.prefs_save_at = Some(Instant::now() + Duration::from_millis(500));
        }
        if relayout {
            self.relayout_all();
        }
    }

    /// Install a new editor theme: document, chrome and window decoration.
    fn set_theme(&mut self, theme: EditorTheme) {
        if let Some(state) = self.state.as_ref() {
            state.window.set_theme(Some(if theme.is_light() {
                Theme::Light
            } else {
                Theme::Dark
            }));
            if let Some(ui) = self.ui.as_mut() {
                ui.set_theme(&state.window, &theme);
            }
        }
        self.doc_engine.theme = theme;
    }

    /// Drop the active document's layout caches and lay it out again (font size, width
    /// or theme changed). Parked tabs drop theirs when parked, so they relayout on
    /// activation anyway.
    fn relayout_all(&mut self) {
        let engine = &mut self.doc_engine;
        engine.line_cache = LineCache::new();
        engine.render_cache = RenderCache::new();
        engine.height_cache = HeightCache::new();
        engine.table_cache = TableCache::new();
        if let Some(state) = self.state.as_mut() {
            sync_image_raster_width(state.scale, self.doc_engine.max_content_width);
            sync_find_chrome(&mut self.doc_engine, state);
            state.window.request_redraw();
        }
    }

    /// Write the preferences to the config file now.
    fn save_preferences(&mut self) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        if let Err(e) = config::save_preferences(&path, &self.prefs) {
            self.show_message(
                "Could not save the preferences",
                format!("Writing {} failed: {e}", path.display()),
            );
        }
    }

    /// Change the font size by `delta` px (zoom in/out), or reset it (`None`).
    fn zoom(&mut self, delta: Option<f32>) {
        let mut prefs = self.prefs.clone();
        prefs.font_size = match delta {
            Some(d) => (prefs.font_size + d)
                .clamp(*config::FONT_SIZE_RANGE.start(), *config::FONT_SIZE_RANGE.end()),
            None => FONT_SIZE,
        };
        self.apply_preferences(prefs);
    }

    /// Jump to heading `i` of the active document (table of contents click): caret on
    /// the heading, unfolded if needed, pinned to the top of the view.
    fn go_to_heading(&mut self, i: usize) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let (w, vh, scale) = state.viewport();
        let Some(off) = self
            .doc_engine
            .editor
            .state
            .buffer
            .headings()
            .get(i)
            .map(|h| h.byte_offset)
        else {
            return;
        };
        self.doc_engine.editor.set_cursor(off);
        // The heading may sit inside a folded ancestor; reveal it and relayout so its
        // geometry exists before pinning it.
        if self.doc_engine.editor.reveal_cursor() {
            self.doc_engine.rebuild_preserving_scroll(w, scale, vh);
        }
        let mut remeasure = false;
        if let Some(doc) = self.doc_engine.doc.as_mut() {
            doc.scroll_line_to_top(doc.line_of(off), vh);
            remeasure = doc.needs_remeasure(vh);
        }
        // Outside the materialized band: rebuild around the new anchor, then re-pin.
        if remeasure {
            self.doc_engine.rebuild_preserving_scroll(w, scale, vh);
            if let Some(doc) = self.doc_engine.doc.as_mut() {
                doc.scroll_line_to_top(doc.line_of(off), vh);
            }
        }
        sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
        self.redraw();
    }

    /// Listen for files dropped on the window when running on Wayland, where winit does
    /// not report them.
    fn start_wayland_dnd(&mut self, window: &Window) {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
            let Ok(handle) = window.display_handle() else {
                return;
            };
            if let RawDisplayHandle::Wayland(wayland) = handle.as_raw() {
                let proxy = self.proxy.clone();
                // SAFETY: winit's display lives as long as the event loop, i.e. the app.
                // The listener is stopped in `exiting`, before winit disconnects.
                let started = unsafe {
                    crate::wayland_dnd::start(wayland.display.as_ptr(), move |paths| {
                        let _ = proxy.send_event(WritEvent::FilesDropped(paths));
                    })
                };
                match started {
                    Ok(listener) => self.dnd = Some(listener),
                    Err(e) => eprintln!("[markdone] file drag-and-drop unavailable: {e}"),
                }
            }
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let _ = window;
    }

    /// Files dropped on the window: images go into the document as image links (at the
    /// caret), everything else opens in tabs.
    fn files_dropped(&mut self, paths: Vec<PathBuf>) {
        let (images, docs): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.into_iter().partition(|p| crate::format::is_image(p));
        if !images.is_empty() {
            let doc_dir = self
                .doc_engine
                .editor
                .file_path()
                .and_then(|p| std::path::absolute(p).ok())
                .and_then(|p| p.parent().map(Path::to_path_buf));
            let links: Vec<String> = images
                .iter()
                .map(|p| crate::format::image_markdown(p, doc_dir.as_deref()))
                .collect();
            let selection = self
                .doc_engine
                .editor
                .selection_range()
                .unwrap_or_else(|| {
                    let c = self.doc_engine.editor.cursor_position();
                    c..c
                });
            let text = links.join("\n");
            let end = selection.start + text.len();
            self.apply_format(crate::format::TextEdit {
                range: selection,
                text,
                selection: end..end,
            });
        }
        if !docs.is_empty() {
            self.open_paths(docs);
            if let Some(state) = self.state.as_ref() {
                state.window.focus_window();
            }
        }
    }

    /// The active document's text and selection (the caret as an empty range).
    fn text_and_selection(&self) -> (String, std::ops::Range<usize>) {
        let editor = &self.doc_engine.editor;
        let sel = editor.selection_range().unwrap_or_else(|| {
            let c = editor.cursor_position();
            c..c
        });
        (editor.text(), sel)
    }

    /// The folder of the active document's file (absolute), if it has one.
    fn active_doc_dir(&self) -> Option<PathBuf> {
        self.doc_engine
            .editor
            .file_path()
            .and_then(|p| std::path::absolute(p).ok())
            .and_then(|p| p.parent().map(Path::to_path_buf))
    }

    /// Paste from the clipboard. `rich`: HTML content (from a browser, an office suite)
    /// is converted to Markdown; plain text is pasted as is. Without `rich`, or when
    /// the HTML adds nothing to the text (e.g. a terminal's styled copy), the plain
    /// text is used.
    fn paste(&mut self, rich: bool) {
        let Some(cb) = self.clipboard.as_mut() else {
            return;
        };
        let plain = cb.get_text().ok().filter(|t| !t.is_empty());
        let converted = if rich {
            cb.get()
                .html()
                .ok()
                .and_then(|h| crate::export::html_to_markdown(&h))
        } else {
            None
        };
        let text = match (converted, plain) {
            (Some(md), Some(plain)) if md.replace('\\', "").trim() == plain.trim() => plain,
            (Some(md), _) => md,
            (None, Some(plain)) => plain,
            (None, None) => return,
        };
        let Some(state) = self.state.as_ref() else {
            return;
        };
        self.doc_engine.editor.paste(&text);
        let (w, vh, scale) = state.viewport();
        let window = state.window.clone();
        apply_edit_effects(
            &mut self.doc_engine,
            &mut self.hovered,
            &self.runtime,
            &self.proxy,
            Some((&self.ac_gen, &self.ac_slot)),
            &window,
            w,
            scale,
            vh,
        );
    }

    /// Copy the selection rendered as HTML, with its Markdown as the plain-text
    /// alternative (pasting into a text editor gives the Markdown).
    fn copy_as_html(&mut self) {
        let Some(markdown) = self.doc_engine.editor.selected_text() else {
            return;
        };
        let html = crate::export::markdown_fragment(&markdown, self.active_doc_dir().as_deref());
        if let Some(cb) = self.clipboard.as_mut()
            && let Err(e) = cb.set_html(html, Some(markdown))
        {
            eprintln!("[markdone] failed to copy as HTML: {e}");
        }
    }

    /// Ask for an image to insert at the caret (answer comes back as `ImagePicked`).
    fn start_insert_image(&mut self) {
        if self.file_dialog_open {
            return;
        }
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let mut dialog = rfd::FileDialog::new()
            .set_title("Insert Image")
            .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "avif"])
            .set_parent(&*state.window);
        if let Some(dir) = self.dialog_dir(self.active_tab) {
            dialog = dialog.set_directory(dir);
        }
        let id = self.tabs[self.active_tab].id;
        let proxy = self.proxy.clone();
        self.file_dialog_open = true;
        std::thread::spawn(move || {
            let path = dialog.pick_file();
            let _ = proxy.send_event(WritEvent::ImagePicked { id, path });
        });
    }

    /// Apply a formatting edit to the active document and refresh like after typing.
    fn apply_format(&mut self, edit: crate::format::TextEdit) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        self.doc_engine.editor.apply_text_edit(&edit);
        let (w, vh, scale) = state.viewport();
        let window = state.window.clone();
        apply_edit_effects(
            &mut self.doc_engine,
            &mut self.hovered,
            &self.runtime,
            &self.proxy,
            None,
            &window,
            w,
            scale,
            vh,
        );
    }

    /// Follow a Ctrl+clicked link: a heading of this document, another Markdown file
    /// (in a tab, at its anchor if any), or the desktop's handler for anything else.
    fn follow_link(&mut self, url: &str) {
        match links::classify(url) {
            LinkTarget::Anchor(anchor) => self.go_to_anchor(&anchor),
            LinkTarget::Document { path, anchor } => {
                if !path.is_file() {
                    self.show_message(
                        "File not found",
                        format!("The link points to “{}”, which does not exist.", path.display()),
                    );
                    return;
                }
                self.open_paths(vec![path]);
                if let Some(anchor) = anchor {
                    self.go_to_anchor(&anchor);
                }
            }
            LinkTarget::External(target) => {
                if let Err(e) = open::that_detached(&target) {
                    eprintln!("[markdone] failed to open {target}: {e}");
                }
            }
        }
    }

    /// Jump to the heading of the active document that `anchor` designates, if any.
    fn go_to_anchor(&mut self, anchor: &str) {
        let index = self
            .doc_engine
            .editor
            .state
            .buffer
            .headings()
            .iter()
            .position(|h| links::anchor_matches(&h.text, anchor));
        match index {
            Some(i) => self.go_to_heading(i),
            None => eprintln!("[markdone] no heading for #{anchor}"),
        }
    }

    /// With "check on focus" on, compare the active document with its file on disk
    /// (a stat) and reload / ask if it changed.
    fn check_active_on_disk(&mut self) {
        if self.prefs.check_on_focus && self.doc_engine.editor.changed_on_disk() {
            let id = self.tabs[self.active_tab].id;
            let _ = self.proxy.send_event(WritEvent::FileChanged(id));
        }
    }

    /// The session as it stands: the open documents (with a file) and the recent list.
    fn current_session(&self) -> Session {
        let mut open = Vec::new();
        let mut active = 0;
        for i in 0..self.tabs.len() {
            if let Some(path) = self.tab_editor(i).file_path() {
                if i == self.active_tab {
                    active = open.len();
                }
                open.push(path.to_path_buf());
            }
        }
        Session {
            open,
            active,
            recent: self.session.recent.clone(),
            toc_open: self.doc_engine.editor.outline_open(),
            toc_width: Some(self.toc_width),
        }
    }

    /// Write the session file if the session changed since the last write.
    fn persist_session(&mut self) {
        let Some(path) = self.session_path.clone() else {
            return;
        };
        let current = self.current_session();
        if self.session_saved.as_ref() == Some(&current) {
            return;
        }
        if let Err(e) = current.save(&path) {
            eprintln!("[markdone] failed to save the session to {}: {e}", path.display());
        }
        self.session_saved = Some(current);
    }

    /// Open entry `i` of the recent files (a file that no longer exists is dropped from
    /// the list, with a message).
    fn open_recent(&mut self, i: usize) {
        let Some(path) = self.session.recent.get(i).cloned() else {
            return;
        };
        if path.is_file() {
            self.open_paths(vec![path]);
        } else {
            self.session.recent.remove(i);
            self.show_message(
                "File not found",
                format!("“{}” no longer exists; it was removed from the recent files.", path.display()),
            );
        }
    }

    /// What the pending dialog shows.
    fn dialog_view(&self) -> Option<DialogView> {
        let name = |id: u64| self.tab_index(id).map(|i| doc_name(self.tab_editor(i)));
        Some(match self.modal.as_ref()? {
            PendingModal::Unsaved { req, dirty } => DialogView::Unsaved {
                names: dirty.iter().filter_map(|&id| name(id)).collect(),
                quitting: req.quit,
            },
            PendingModal::Message { title, text } => DialogView::Message {
                title: title.clone(),
                text: text.clone(),
            },
            PendingModal::ConfirmReload { id } => DialogView::ConfirmReload {
                name: name(*id).unwrap_or_default(),
            },
            PendingModal::Exported { path } => DialogView::Exported {
                name: path
                    .file_name()
                    .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned()),
            },
        })
    }

    /// Act on the user's answer to the pending dialog.
    fn answer_dialog(&mut self, answer: Answer, event_loop: &ActiveEventLoop) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        match modal {
            PendingModal::Unsaved { req, dirty } => match answer {
                Answer::Save => {
                    // Documents with a file first, then the first untitled one (its
                    // Save As resumes the request, which asks again for the rest).
                    let mut untitled = None;
                    for id in dirty {
                        let Some(i) = self.tab_index(id) else {
                            continue;
                        };
                        if self.tab_editor(i).file_path().is_none() {
                            untitled = untitled.or(Some(i));
                        } else if !self.save_tab(i) {
                            return; // the error is shown; nothing is closed
                        }
                    }
                    match untitled {
                        Some(i) => self.start_save_as(i, Some(req)),
                        None => self.finish_close(req, event_loop),
                    }
                }
                Answer::Discard => self.finish_close(req, event_loop),
                _ => {}
            },
            PendingModal::Message { .. } => {}
            PendingModal::ConfirmReload { id } => {
                if answer == Answer::Reload {
                    self.reload_tab(id);
                }
            }
            PendingModal::Exported { path } => match answer {
                Answer::OpenFile => {
                    if let Err(e) = open::that_detached(&path) {
                        eprintln!("[markdone] failed to open {}: {e}", path.display());
                    }
                }
                Answer::Reveal => crate::platform::reveal_in_file_manager(&path),
                _ => {}
            },
        }
        self.redraw();
    }

    /// Act on the answer to the active tab's disk conflict.
    fn answer_conflict(&mut self, answer: Answer) {
        let Some(DiskConflict { ticket, data }) = self.tabs[self.active_tab].conflict.take() else {
            return;
        };
        let editor = &mut self.doc_engine.editor;
        editor.note_disk_state();
        if answer == Answer::Reload {
            editor.apply_file_content(data.0);
            self.reloads.apply_base(editor, ticket.base_generation, data.1);
            self.after_reload();
        } else {
            // Keep the buffer: it now differs from the file, so it is unsaved.
            editor.mark_dirty();
            self.reloads.apply_base(editor, ticket.base_generation, data.1);
        }
        self.redraw();
    }

    /// The active document's text was replaced from disk: treat it like an edit
    /// (refresh detection, revalidate refs, reload images) and redraw.
    fn after_reload(&mut self) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let (w, vh, scale) = state.viewport();
        let window = state.window.clone();
        apply_edit_effects(
            &mut self.doc_engine,
            &mut self.hovered,
            &self.runtime,
            &self.proxy,
            None,
            &window,
            w,
            scale,
            vh,
        );
    }

    /// Append an empty untitled document and activate it.
    fn open_blank_tab(&mut self) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.push(Tab {
            id,
            state: Some(DocState::new(Editor::new(""))),
            anchor: (0, 0.0),
            file_stale: false,
            base_stale: false,
            conflict: None,
        });
        self.switch_tab(self.tabs.len() - 1);
    }

    /// Move tab `from` to position `to` (drag-and-drop reorder), keeping the active
    /// document active.
    fn move_tab(&mut self, from: usize, to: usize) {
        let n = self.tabs.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let active_id = self.tabs[self.active_tab].id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active_tab = self
            .tabs
            .iter()
            .position(|t| t.id == active_id)
            .expect("active tab still present");
    }

    /// Execute an application command (from the menu bar).
    fn run_command(&mut self, cmd: Command, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match cmd {
            Command::New => self.open_blank_tab(),
            Command::Preferences => self.prefs_open = true,
            Command::GoToHeading(i) => {
                self.go_to_heading(i);
                return;
            }
            Command::ToggleGitDiff => {
                let mut prefs = self.prefs.clone();
                prefs.git_diff = !prefs.git_diff;
                self.apply_preferences(prefs);
                return;
            }
            Command::ToggleLineNumbers => {
                let mut prefs = self.prefs.clone();
                prefs.line_numbers = !prefs.line_numbers;
                self.apply_preferences(prefs);
                return;
            }
            Command::ZoomIn => {
                self.zoom(Some(1.0));
                return;
            }
            Command::ZoomOut => {
                self.zoom(Some(-1.0));
                return;
            }
            Command::ResetZoom => {
                self.zoom(None);
                return;
            }
            Command::OpenRecent(i) => {
                self.open_recent(i);
                return;
            }
            Command::ClearRecent => {
                self.session.recent.clear();
                return;
            }
            Command::Cut | Command::Copy => {
                let cut = cmd == Command::Cut;
                let clipboard = &mut self.clipboard;
                let copied = clipboard_copy_or_cut(&mut self.doc_engine.editor, cut, |selection| {
                    clipboard
                        .as_mut()
                        .ok_or_else(|| anyhow!("clipboard unavailable"))?
                        .set_text(selection)?;
                    Ok(())
                });
                if let Err(error) = &copied {
                    eprintln!("[markdone] failed to copy selection: {error}");
                }
                if cut && matches!(copied, Ok(true)) {
                    let (w, vh, scale) = state.viewport();
                    apply_edit_effects(
                        &mut self.doc_engine,
                        &mut self.hovered,
                        &self.runtime,
                        &self.proxy,
                        None,
                        &state.window,
                        w,
                        scale,
                        vh,
                    );
                }
            }
            Command::Paste | Command::PasteWithoutStyle => {
                self.paste(cmd == Command::Paste);
                return;
            }
            Command::CopyAsHtml => {
                self.copy_as_html();
                return;
            }
            Command::Inline(style) => {
                let (text, sel) = self.text_and_selection();
                self.apply_format(crate::format::toggle_inline(&text, sel, style));
                return;
            }
            Command::Block(block) => {
                let (text, sel) = self.text_and_selection();
                self.apply_format(crate::format::set_block(&text, sel, block));
                return;
            }
            Command::InsertLink => {
                let (text, sel) = self.text_and_selection();
                self.apply_format(crate::format::insert_link(&text, sel));
                return;
            }
            Command::InsertCodeBlock => {
                let (text, sel) = self.text_and_selection();
                self.apply_format(crate::format::insert_code_block(&text, sel));
                return;
            }
            Command::InsertImage => {
                self.start_insert_image();
                return;
            }
            Command::SelectAll => self.doc_engine.editor.select_all(),
            Command::Open => {
                self.start_open();
                return;
            }
            Command::Save => {
                self.save_tab(self.active_tab);
            }
            Command::SaveAs => {
                self.start_save_as(self.active_tab, None);
                return;
            }
            Command::ExportPdf => {
                self.start_export(self.active_tab);
                return;
            }
            Command::Reload => {
                let id = self.tabs[self.active_tab].id;
                if self.doc_engine.editor.file_path().is_none() {
                    return;
                }
                if self.doc_engine.editor.is_dirty() && self.ui.is_some() {
                    self.modal = Some(PendingModal::ConfirmReload { id });
                    self.redraw();
                } else {
                    self.reload_tab(id);
                }
                return;
            }
            Command::CloseTab => {
                self.close_tab(self.active_tab, event_loop);
                return;
            }
            Command::SwitchTab(i) => self.switch_tab(i),
            Command::CloseTabAt(i) => {
                self.close_tab(i, event_loop);
                return;
            }
            Command::CloseOtherTabs(keep) => {
                if let Some(keep_id) = self.tabs.get(keep).map(|t| t.id) {
                    self.switch_tab(keep);
                    let ids = self.tabs.iter().map(|t| t.id).filter(|&id| id != keep_id).collect();
                    self.request_close(CloseRequest { ids, quit: false }, event_loop);
                }
                return;
            }
            Command::CloseAllTabs | Command::Quit => {
                let ids = self.tabs.iter().map(|t| t.id).collect();
                let quit = cmd == Command::Quit;
                self.request_close(CloseRequest { ids, quit }, event_loop);
                return;
            }
            Command::CopyPath(i) => {
                if let Some(path) = self.tab_editor(i).file_path().map(Path::to_path_buf)
                    && let Some(cb) = self.clipboard.as_mut()
                    && let Err(e) = cb.set_text(path.display().to_string())
                {
                    eprintln!("[markdone] failed to copy path: {e}");
                }
                return;
            }
            Command::RevealInFileManager(i) => {
                if let Some(path) = self.tab_editor(i).file_path() {
                    crate::platform::reveal_in_file_manager(path);
                }
                return;
            }
            Command::MoveTab { from, to } => {
                self.move_tab(from, to);
            }
            Command::Undo | Command::Redo => {
                if cmd == Command::Undo {
                    self.doc_engine.editor.undo();
                } else {
                    self.doc_engine.editor.redo();
                }
                self.doc_engine.editor.find_rescan();
                let (w, vh, scale) = state.viewport();
                apply_edit_effects(
                    &mut self.doc_engine,
                    &mut self.hovered,
                    &self.runtime,
                    &self.proxy,
                    None,
                    &state.window,
                    w,
                    scale,
                    vh,
                );
            }
            Command::Find | Command::Replace => {
                self.doc_engine.editor.open_find(cmd == Command::Replace);
                state.find_bar_h = find_bar_height(&self.doc_engine.editor, state.scale);
                let (w, vh, scale) = state.viewport();
                apply_edit_effects(
                    &mut self.doc_engine,
                    &mut self.hovered,
                    &self.runtime,
                    &self.proxy,
                    None,
                    &state.window,
                    w,
                    scale,
                    vh,
                );
            }
            Command::ToggleOutline => {
                self.doc_engine.editor.toggle_outline();
                let (w, vh, scale) = state.viewport();
                self.doc_engine.rebuild_preserving_scroll(w, scale, vh);
            }
            Command::NextTab | Command::PrevTab => {
                let n = self.tabs.len();
                if n > 1 {
                    let step = if cmd == Command::NextTab { 1 } else { n - 1 };
                    self.switch_tab((self.active_tab + step) % n);
                }
            }
        }
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }

    /// Post-edit async side effects: validate freshly-detected GitHub refs and start
    /// loading any newly-appeared standalone images. Both are idempotent (cache-guarded).
    fn spawn_validations(&self) {
        let visible = self.state.as_ref().and_then(|s| {
            let (_, vh, _) = s.viewport();
            self.doc_engine.doc.as_ref().map(|d| {
                let (a, b) = d.visible_range(vh);
                a..b
            })
        });
        spawn_ref_validations(&self.doc_engine.editor, visible, &self.runtime, &self.proxy);
        self.sync_images();
    }

    /// Kick off loads for every standalone image in the current layout that isn't cached
    /// yet. Idempotent (already-cached URLs are skipped), so it's safe to call after any
    /// rebuild/scroll.
    fn sync_images(&self) {
        sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
    }
}

/// Decoded-pixel budget for file/remote images per document (bytes). Images in the
/// materialized band are always kept; older ones beyond this are evicted and reloaded on
/// demand. ~10 downscaled 1024×728 screenshots.
const IMAGE_MEMORY_BUDGET: usize = 32 * 1024 * 1024;

/// Touchpad scroll distance (device px) that makes one Ctrl+wheel zoom step.
const WHEEL_ZOOM_PX_PER_STEP: f32 = 50.0;

/// Pin glibc's malloc thresholds so large blocks always go through `mmap` and return
/// to the OS when freed. By default glibc *raises* both thresholds after the first large
/// free (up to 32 MB mmap / 64 MB trim), after which image-sized buffers (decode
/// temporaries, decoded pixels) land in the per-thread arenas of the decoder pool and
/// are never given back: measured ~136 MB free-but-resident after one pass over a
/// document with 29 screenshots, which `malloc_trim` did not release. Setting the
/// values explicitly disables the dynamic adjustment.
fn configure_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        const M_TRIM_THRESHOLD: std::ffi::c_int = -1;
        const M_MMAP_THRESHOLD: std::ffi::c_int = -3;
        unsafe extern "C" {
            fn mallopt(param: std::ffi::c_int, value: std::ffi::c_int) -> std::ffi::c_int;
        }
        // SAFETY: called once at startup, before any other thread exists.
        unsafe {
            mallopt(M_MMAP_THRESHOLD, 256 * 1024);
            mallopt(M_TRIM_THRESHOLD, 1024 * 1024);
        }
    }
}

/// Bytes the allocator currently has handed out (arena + mmapped chunks).
fn malloc_bytes_in_use() -> usize {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        #[repr(C)]
        struct Mallinfo2 {
            arena: usize,
            ordblks: usize,
            smblks: usize,
            hblks: usize,
            hblkhd: usize,
            usmblks: usize,
            fsmblks: usize,
            uordblks: usize,
            fordblks: usize,
            keepcost: usize,
        }
        unsafe extern "C" {
            fn mallinfo2() -> Mallinfo2;
        }
        // SAFETY: mallinfo2 only reads allocator bookkeeping.
        let m = unsafe { mallinfo2() };
        m.uordblks + m.hblkhd
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    0
}

/// This process's PSS from /proc/self/smaps_rollup (profiling only).
fn self_pss() -> String {
    std::fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Pss:"))
                .and_then(|l| l.split_whitespace().nth(1).map(str::to_string))
        })
        .map_or_else(
            || "?".to_string(),
            |kb| format!("{:.1} MB", kb.parse::<f64>().unwrap_or(0.0) / 1024.0),
        )
}

/// glibc allocator totals (all arenas) for profiling: bytes in use vs. free-but-held.
fn allocator_stats() -> String {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        #[repr(C)]
        #[derive(Default)]
        struct Mallinfo2 {
            arena: usize,
            ordblks: usize,
            smblks: usize,
            hblks: usize,
            hblkhd: usize,
            usmblks: usize,
            fsmblks: usize,
            uordblks: usize,
            fordblks: usize,
            keepcost: usize,
        }
        unsafe extern "C" {
            fn mallinfo2() -> Mallinfo2;
        }
        // SAFETY: mallinfo2 only reads allocator bookkeeping.
        let m = unsafe { mallinfo2() };
        let mb = |b: usize| b as f64 / 1048576.0;
        format!(
            "malloc: in use {:.1} MB (+ mmapped {:.1} MB), free-held {:.1} MB, arena total {:.1} MB",
            mb(m.uordblks),
            mb(m.hblkhd),
            mb(m.fordblks),
            mb(m.arena)
        )
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    String::new()
}

/// Widest useful raster for images at this window scale: the body never exceeds the
/// configured text width (or, at full width, a large screen's width).
fn sync_image_raster_width(scale: f32, max_content_width: Option<f32>) {
    // Full width: images may be shown wider; cap the decode at a large screen's width.
    const FULL_WIDTH_RASTER: f32 = 1920.0;
    let width = max_content_width.unwrap_or(FULL_WIDTH_RASTER);
    crate::image_cache::set_max_raster_width((width * scale).ceil() as u32);
}

/// Free-function form of `App::sync_images` that borrows only the doc-engine (+ runtime
/// / proxy) — usable from `window_event` where `self.state` is already borrowed mutably.
fn sync_image_loads(
    doc_engine: &DocEngine,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
) {
    let Some(doc) = doc_engine.doc.as_ref() else {
        return;
    };
    // Mermaid diagrams render off-thread into the same image cache; kick off any new ones
    // (does nothing for sources already loading/loaded/failed).
    #[cfg(feature = "mermaid")]
    {
        let sources = doc.mermaid_sources();
        if !sources.is_empty() {
            let proxy = proxy.clone();
            mermaid::spawn_mermaid_renders(sources, &doc_engine.images, move || {
                let _ = proxy.send_event(WritEvent::ImageLoaded);
            });
        }
    }
    // Math renders (block + inline) share the same off-thread → image-cache path.
    #[cfg(feature = "math")]
    {
        let jobs = doc.math_sources();
        if !jobs.is_empty() {
            let proxy = proxy.clone();
            math::spawn_math_renders(jobs, &doc_engine.images, move || {
                let _ = proxy.send_event(WritEvent::ImageLoaded);
            });
        }
    }
    let urls = doc.image_urls();
    // Drop decoded pixels of images far from the viewport beyond the memory budget
    // (their size is kept, so layout is unaffected); `urls` is the materialized band.
    doc_engine.images.trim(urls, IMAGE_MEMORY_BUDGET);
    if urls.is_empty() {
        return;
    }
    let dir = doc_engine
        .editor
        .file_path()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    spawn_image_loads(
        dir,
        urls,
        &doc_engine.images,
        runtime,
        RedrawOnImageLoad(proxy.clone()),
    );
}

/// The async side effects every buffer edit needs, in one place so an edit entry point
/// can't silently skip one: rebuild+re-detect, spawn validation for newly-visible refs,
/// and load newly-appeared images. Callers still own the surrounding UI (clearing the
/// hover, autocomplete sync, and `request_redraw`) since those vary per site.
fn after_edit(
    doc_engine: &mut DocEngine,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
    w: f32,
    scale: f32,
    editor_h: f32,
) {
    doc_engine.refresh(w, scale, editor_h);
    let visible = doc_engine.doc.as_ref().map(|d| {
        let (a, b) = d.visible_range(editor_h);
        a..b
    });
    spawn_ref_validations(&doc_engine.editor, visible, runtime, proxy);
    sync_image_loads(doc_engine, runtime, proxy);
}

/// The full flow every edit site must run: drop the stale hover target, apply the async
/// side effects ([`after_edit`]), optionally refresh the autocomplete popup (pass `ac`
/// for keyboard edits, `None` for mouse/reload), then request a redraw. Defined once so a
/// new edit site can't silently skip a step. Takes the doc-engine's sibling fields
/// individually so it composes while `state` is borrowed at the call site.
#[allow(clippy::too_many_arguments)]
fn apply_edit_effects(
    doc_engine: &mut DocEngine,
    hovered: &mut Option<HoverTarget>,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
    ac: Option<(&Arc<AtomicU64>, &AcSlot)>,
    window: &Window,
    w: f32,
    scale: f32,
    editor_h: f32,
) {
    // A buffer edit while the find bar is open leaves the highlighted match set stale
    // (fall-through typing, undo/redo, replace). Rescan so highlights track the document;
    // it no-ops when (version, query, case, regex) is unchanged. Runs before `after_edit`
    // so the cursor reveal below lands on the refreshed active match.
    if doc_engine.editor.find_state().is_some() {
        doc_engine.editor.find_rescan();
    }
    *hovered = None;
    after_edit(doc_engine, runtime, proxy, w, scale, editor_h);
    if let Some((ac_gen, ac_slot)) = ac {
        sync_autocomplete(&mut doc_engine.editor, runtime, proxy, ac_gen, ac_slot);
    }
    window.request_redraw();
}

/// For each detected GitHub ref (from refs + naked URLs) with no cache entry yet,
/// mark it pending and spawn a tokio task to validate it. Results land in the shared
/// `GitHubValidationCache`; a `GithubUpdated` wakeup triggers a redraw.
fn spawn_ref_validations(
    editor: &Editor,
    visible: Option<Range<usize>>,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
) {
    let Some(client) = editor.github_client() else {
        return;
    };
    let cache = editor.github_validation_cache();

    // No layout yet → validate all; otherwise only the refs on visible lines.
    let refs = match visible {
        Some(r) => editor.detected_refs_in_lines(r),
        None => editor.detected_refs(),
    };
    for reference in refs {
        if cache.contains(&reference) {
            continue; // already pending/valid/invalid
        }
        cache.mark_pending(reference.clone());
        let client = client.clone();
        let cache = cache.clone();
        let proxy = proxy.clone();
        runtime.spawn(async move {
            validate_ref_into_cache(&client, &cache, reference).await;
            let _ = proxy.send_event(WritEvent::GithubUpdated);
        });
    }
}

/// Validate one ref and record the outcome in the shared cache. Shared by the async
/// GUI path and the blocking snapshot path so the mapping lives in one place.
async fn validate_ref_into_cache(
    client: &GitHubClient,
    cache: &GitHubValidationCache,
    reference: GitHubRef,
) {
    match client.validate_ref(&reference).await {
        ValidationResult::ValidWithData(d) => cache.set_valid(reference, Some(d)),
        ValidationResult::ValidNoData => cache.set_valid(reference, None),
        ValidationResult::Invalid => cache.set_invalid(reference),
    }
}

/// Translate a key press into an editor edit/move. Returns true if the editor
/// changed (so the caller rebuilds + reveals the cursor).
/// Lines the cursor jumps per PageUp/PageDown (fixed; viewport-aware is a refinement).
const PAGE_LINES: usize = 20;

/// Extra lines above/below the viewport that ref/URL detection covers — generous so it
/// spans every line the layout materializes and draws (viewport + measure overscan),
/// keeping scrolled-in refs colored while bounding the per-keystroke scan.
const DETECT_OVERSCAN_LINES: usize = 200;

/// Max gap between presses (and max pointer travel, in physical px) still counted as a
/// repeated click for double/triple-click selection.
const MULTI_CLICK_MS: u128 = 400;
const MULTI_CLICK_DIST: f32 = 6.0;

// Drag-select autoscroll tuning. Follows the de-facto pattern (VS Code, CodeMirror 6,
// AppKit, GTK): a hot zone at the viewport edge, speed that eases in across the zone then
// grows with how far the pointer is past the edge, capped so a flick can't teleport — all
// integrated against real elapsed time (frame-rate independent) like VS Code's
// dragScrolling. Values are logical px; the caller multiplies by elapsed seconds.
/// Depth of the top/bottom hot zone; speed eases 0→`EDGE_SPEED` across it.
const DRAG_SCROLL_ZONE: f32 = 24.0;
/// Speed right at the viewport edge (~6 lines/s) — the calm baseline.
const DRAG_SCROLL_EDGE_SPEED: f32 = 140.0;
/// Extra px/s per px the pointer is *past* the edge (e.g. dragged outside the window).
const DRAG_SCROLL_GAIN: f32 = 10.0;
/// Hard cap so pushing far past the edge stays fast-but-controllable (~36 lines/s).
const DRAG_SCROLL_MAX_SPEED: f32 = 800.0;

/// Auto-scroll velocity (physical px/second) when a drag pointer sits at height `y` in a
/// `[0, editor_h]` viewport: 0 in the middle; eases in across the hot zone; grows
/// linearly once past the edge; clamped to `MAX_SPEED`. Negative = scroll up. The caller
/// multiplies by real elapsed seconds so speed is independent of tick/frame rate.
fn drag_edge_velocity(y: f32, editor_h: f32, scale: f32) -> f32 {
    let zone = DRAG_SCROLL_ZONE * scale;
    let edge = DRAG_SCROLL_EDGE_SPEED * scale;
    let cap = DRAG_SCROLL_MAX_SPEED * scale;
    // `depth` = physical px past the inner edge of the zone (== zone at the viewport edge,
    // > zone once outside the window). Sign picks scroll direction.
    let (depth, dir) = if y < zone {
        (zone - y, -1.0)
    } else if y > editor_h - zone {
        (y - (editor_h - zone), 1.0)
    } else {
        return 0.0;
    };
    let speed = edge * (depth / zone).min(1.0) + DRAG_SCROLL_GAIN * (depth - zone).max(0.0);
    dir * speed.min(cap)
}

/// One drag step: scroll by `dy` (if any), then extend the selection to whatever sits
/// under the viewport-clamped pointer (so dragging into the margin still grows it).
/// Takes `&mut DocEngine` (not `&mut self`) so it composes while `state` is borrowed.
fn drag_extend_step(
    doc_engine: &mut DocEngine,
    mouse: (f32, f32),
    dy: f32,
    w: f32,
    scale: f32,
    editor_h: f32,
) {
    if dy != 0.0
        && let Some(doc) = doc_engine.doc.as_mut()
    {
        doc.scroll_by(dy, editor_h);
    }
    let hy = mouse.1.clamp(0.0, editor_h);
    if let Some(off) = doc_engine
        .doc
        .as_ref()
        .and_then(|d| d.hit_test(mouse.0, hy))
    {
        doc_engine.editor.drag(off);
    }
    doc_engine.refresh(w, scale, editor_h);
}

fn clipboard_copy_or_cut(
    editor: &mut Editor,
    cut: bool,
    write: impl FnOnce(String) -> Result<()>,
) -> Result<bool> {
    let Some(selection) = editor.selected_text() else {
        return Ok(false);
    };
    // A failed clipboard write must leave the selection and undo history intact.
    write(selection)?;
    if cut {
        editor.backspace();
    }
    Ok(cut)
}

fn apply_key(
    editor: &mut Editor,
    modifiers: ModifiersState,
    event: &winit::event::KeyEvent,
) -> bool {
    let shift = modifiers.shift_key();
    let alt = modifiers.alt_key();
    let ctrl = modifiers.control_key() || modifiers.super_key();
    match &event.logical_key {
        Key::Named(NamedKey::Enter) => {
            if shift && alt {
                editor.shift_alt_enter();
            } else if shift {
                editor.shift_enter();
            } else {
                editor.enter();
            }
            true
        }
        Key::Named(NamedKey::Backspace) if ctrl => {
            editor.delete_word(false);
            true
        }
        Key::Named(NamedKey::Delete) if ctrl => {
            editor.delete_word(true);
            true
        }
        Key::Named(NamedKey::Backspace) => {
            editor.backspace();
            true
        }
        Key::Named(NamedKey::Delete) => {
            editor.delete_forward();
            true
        }
        Key::Named(NamedKey::Tab) => {
            // In a code block Tab indents (4 spaces) instead of cycling list nesting.
            if !shift && editor.cursor_in_code_block() {
                editor.insert_str("    ");
            } else if shift {
                editor.shift_tab();
            } else {
                editor.tab();
            }
            true
        }
        Key::Named(NamedKey::Space) => {
            // Smart space: try_insert_space suppresses the space at a line's
            // start / blockquote-content start (returns false) — no fallback insert.
            editor.try_insert_space();
            true
        }
        Key::Named(NamedKey::ArrowLeft) if ctrl => {
            editor.move_word(false, shift);
            true
        }
        Key::Named(NamedKey::ArrowRight) if ctrl => {
            editor.move_word(true, shift);
            true
        }
        Key::Named(NamedKey::ArrowLeft) => {
            editor.move_in_direction(Direction::Left, shift);
            true
        }
        Key::Named(NamedKey::ArrowRight) => {
            editor.move_in_direction(Direction::Right, shift);
            true
        }
        Key::Named(NamedKey::ArrowUp) => {
            editor.move_in_direction(Direction::Up, shift);
            true
        }
        Key::Named(NamedKey::ArrowDown) => {
            editor.move_in_direction(Direction::Down, shift);
            true
        }
        // Home/End: line boundary, or document boundary with Ctrl/Super; Shift extends.
        Key::Named(NamedKey::Home) => {
            let dir = if ctrl {
                Direction::DocStart
            } else {
                Direction::LineStart
            };
            editor.move_in_direction(dir, shift);
            true
        }
        Key::Named(NamedKey::End) => {
            let dir = if ctrl {
                Direction::DocEnd
            } else {
                Direction::LineEnd
            };
            editor.move_in_direction(dir, shift);
            true
        }
        // Page up/down: a fixed page of lines (viewport-aware sizing is a later refinement).
        Key::Named(NamedKey::PageUp) => {
            for _ in 0..PAGE_LINES {
                editor.move_in_direction(Direction::Up, shift);
            }
            true
        }
        Key::Named(NamedKey::PageDown) => {
            for _ in 0..PAGE_LINES {
                editor.move_in_direction(Direction::Down, shift);
            }
            true
        }
        _ => {
            // Command shortcuts (Ctrl or Super held). Any other Ctrl+key is swallowed
            // so it doesn't type a character.
            if ctrl {
                if let Key::Character(c) = &event.logical_key {
                    let c = c.as_str();
                    if c.eq_ignore_ascii_case("s") {
                        if let Err(e) = editor.save() {
                            eprintln!("[markdone] save failed: {e}");
                        }
                        return true; // redraw so the title's dirty marker clears
                    }
                    if c.eq_ignore_ascii_case("z") {
                        // Ctrl+Shift+Z = redo, Ctrl+Z = undo.
                        if shift {
                            editor.redo();
                        } else {
                            editor.undo();
                        }
                        return true;
                    }
                    if c.eq_ignore_ascii_case("y") {
                        editor.redo();
                        return true;
                    }
                    if c.eq_ignore_ascii_case("a") {
                        editor.select_all();
                        return true;
                    }
                    if c.eq_ignore_ascii_case("r") {
                        // Force GitHub refs to re-validate (bust a stale/invalid cache);
                        // returning true runs after_edit, which re-spawns validation.
                        editor.revalidate_github_refs();
                        return true;
                    }
                }
                return false;
            }
            // Printable text (respects shift for capitals/symbols).
            if let Some(text) = &event.text
                && !text.is_empty()
                && !text.chars().any(|c| c.is_control())
            {
                editor.insert_str(text);
                // Auto-complete markdown structure just typed: `> `→blockquote space,
                // ` ``` `/`~~~`→closing fence.
                match text.as_str() {
                    ">" => editor.maybe_complete_blockquote_marker(),
                    "`" | "~" => editor.maybe_complete_code_fence(),
                    _ => {}
                }
                return true;
            }
            false
        }
    }
}

impl DocEngine {
    /// Rebuild after an edit or cursor move (marker reveal is cursor-dependent),
    /// preserving scroll then revealing the cursor. Re-detects GitHub refs / naked
    /// URLs first so coloring + validation spawning see the current buffer.
    ///
    /// A method on `DocEngine` (not `App`) so it borrows only the doc-engine fields,
    /// leaving the caller's `&self.state` (GPU surface) borrow disjoint and intact.
    /// Line window to run GitHub-ref / URL detection over: the visible range widened by
    /// overscan (covering the lines the rebuild materializes + draws) and always
    /// including the cursor line. Whole buffer until the first layout exists. Scoping it
    /// keeps per-keystroke detection bounded instead of O(total lines) on large docs.
    fn detection_range(&self, editor_h: f32) -> Range<usize> {
        let n = self.editor.state.buffer.line_count();
        let cursor_line = self.editor.line_of(self.editor.cursor_position());
        match &self.doc {
            Some(d) => {
                let (first, last) = d.visible_range(editor_h);
                let lo = first.saturating_sub(DETECT_OVERSCAN_LINES).min(cursor_line);
                let hi = (last + DETECT_OVERSCAN_LINES).max(cursor_line + 1).min(n);
                lo..hi
            }
            None => 0..n,
        }
    }

    /// Rebuild at the current doc's scroll anchor and restore that scroll position
    /// (clamped). The shared core of `refresh` and `rebuild_preserving_scroll`; callers
    /// run detection and any cursor reveal around it.
    fn relayout_at_anchor(&mut self, device_width: f32, scale: f32, editor_h: f32) -> DocLayout {
        let (anchor_line, anchor_off) = self
            .doc
            .as_ref()
            .map(|d| d.scroll_anchor())
            .unwrap_or((0, 0.0));
        let mut new_doc = self.rebuild(device_width, scale, anchor_line, editor_h);
        new_doc.scroll_y = new_doc.anchor_scroll_y(anchor_line, anchor_off);
        new_doc.clamp_scroll(editor_h);
        new_doc
    }

    fn refresh(&mut self, device_width: f32, scale: f32, editor_h: f32) {
        // Auto-unfold if a cursor move (arrows, typing, find fall-through) entered a
        // folded region, before we lay out around the new position.
        self.editor.reveal_cursor();
        self.editor
            .refresh_detection(self.detection_range(editor_h));
        self.doc = Some(self.relayout_at_anchor(device_width, scale, editor_h));
        self.scroll_to_cursor(device_width, scale, editor_h);
    }

    fn scroll_to_cursor(&mut self, device_width: f32, scale: f32, editor_h: f32) {
        if let Some(doc) = self.doc.as_mut() {
            doc.scroll_to(self.editor.cursor_position(), editor_h);
        }
        // A cursor jump (Ctrl+End / PageDown) can reveal lines outside the band built
        // around the old anchor; rebuild once around the new position so it's laid out.
        if self
            .doc
            .as_ref()
            .is_some_and(|d| d.needs_remeasure(editor_h))
        {
            self.doc = Some(self.relayout_at_anchor(device_width, scale, editor_h));
        }
    }

    /// Rebuild preserving the current scroll (no cursor reveal): the freshly-validated-
    /// refs recolor and wheel-remeasure paths, where the viewport must not jump. Re-runs
    /// detection over the (possibly scrolled) viewport — gated, so a same-window recolor
    /// wakeup doesn't rescan, but scrolling into new lines detects their refs.
    fn rebuild_preserving_scroll(&mut self, device_width: f32, scale: f32, editor_h: f32) {
        self.editor
            .refresh_detection(self.detection_range(editor_h));
        self.doc = Some(self.relayout_at_anchor(device_width, scale, editor_h));
    }

    /// Lay out the document at `device_width`, materializing a band around `anchor_line`
    /// (the top visible line) that covers `viewport_h` + overscan; the head and tail are
    /// height-estimated. `anchor_line = 0` + `viewport_h = f32::INFINITY` lays out
    /// everything. Borrows disjoint doc-engine fields so the caller's surface borrow stays
    /// intact.
    fn rebuild(
        &mut self,
        device_width: f32,
        scale: f32,
        anchor_line: usize,
        viewport_h: f32,
    ) -> DocLayout {
        let t0 = Instant::now();
        let layout = self.rebuild_inner(device_width, scale, anchor_line, viewport_h);
        profile::record_rebuild(t0);
        layout
    }

    fn rebuild_inner(
        &mut self,
        device_width: f32,
        scale: f32,
        anchor_line: usize,
        viewport_h: f32,
    ) -> DocLayout {
        let cursor_offset = self.editor.cursor_position();
        let version = self.editor.state.buffer.version();
        // Clone the diff before borrowing the buffer mutably for the snapshot.
        let diff = if self.git_diff {
            self.editor.diff_state().cloned()
        } else {
            None
        };
        let snapshot = self.editor.state.buffer.render_snapshot();
        // The snapshot is owned, so the mutable borrow above is released — now gather
        // the GitHub autolink data (immutable borrows) to color validated refs.
        let github = GithubRenderData {
            refs_by_line: self.editor.github_refs_by_line(),
            urls_by_line: self.editor.naked_urls_by_line(),
            cache: self.editor.github_validation_cache(),
            context: self.editor.github_context(),
        };
        // The outline panel (when open) claims a right strip, so the doc region is
        // `[0, device_width - outline_w]`. `device_width` stays the full surface width;
        // the inset is applied here so every rebuild call site keeps passing it whole.
        let outline_w = outline_width(&self.editor, scale);
        let params = LayoutParams {
            content_x0: 0.0,
            content_w: (device_width - outline_w).max(1.0),
            scale,
            pad_x: if self.line_numbers {
                PADDING + line_number_gutter(self.font_size, snapshot.line_count())
            } else {
                PADDING
            },
            pad_top: PADDING,
            pad_bottom: PADDING * 2.0,
            base_font_size: self.font_size,
            line_height: LINE_HEIGHT,
            fg: peniko_color(self.theme.foreground),
            max_content_width: self.max_content_width,
        };
        let preedit = self.preedit.as_ref().map(|p| PreeditView {
            text: &p.text,
            cursor: p.cursor,
        });
        let folds = self.editor.hidden_line_ranges();
        let selection = self.editor.selection_range().unwrap_or_else(|| {
            let c = self.editor.cursor_position();
            c..c
        });
        DocLayout::build(
            &mut self.text_engine,
            &mut self.line_cache,
            &mut self.render_cache,
            &mut self.height_cache,
            &mut self.table_cache,
            version,
            &snapshot,
            &self.theme,
            diff.as_ref(),
            Some(&github),
            &self.images,
            cursor_offset,
            &params,
            preedit.as_ref(),
            anchor_line,
            viewport_h,
            &folds,
            &selection,
            self.editor.math_spans_by_line(),
        )
    }
}

/// Re-evaluate the autocomplete popup against the cursor and, if a fetch is needed,
/// spawn a debounced (150ms) tokio task that fetches suggestions into the shared slot.
fn sync_autocomplete(
    editor: &mut Editor,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
    fetch_gen: &Arc<AtomicU64>,
    slot: &AcSlot,
) {
    if !editor.update_autocomplete_from_cursor() {
        return;
    }
    let Some((trigger, prefix)) = editor.begin_autocomplete_fetch() else {
        return;
    };
    let Some(client) = editor.github_client().cloned() else {
        return;
    };
    let Some(context) = editor.github_context().cloned() else {
        return;
    };

    let my_gen = fetch_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let fetch_gen = fetch_gen.clone();
    let proxy = proxy.clone();
    let slot = slot.clone();
    runtime.spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if fetch_gen.load(Ordering::SeqCst) != my_gen {
            return; // a newer keystroke superseded this fetch
        }
        let fetched = fetch_suggestions(&client, &context, trigger, prefix).await;
        *slot.lock().unwrap() = Some(fetched);
        let _ = proxy.send_event(WritEvent::GithubUpdated);
    });
}

/// Fetch issue/user autocomplete suggestions for `trigger`. Shared by the async GUI
/// path (debounced) and the blocking snapshot path.
async fn fetch_suggestions(
    client: &GitHubClient,
    context: &GitHubContext,
    trigger: AutocompleteTrigger,
    prefix: String,
) -> FetchedSuggestions {
    match trigger {
        AutocompleteTrigger::Issue => {
            let issues = client
                .issues_matching_prefix(&context.owner, &context.repo, &prefix, 8)
                .await;
            FetchedSuggestions::Issues { prefix, issues }
        }
        AutocompleteTrigger::User => {
            let users = client
                .users_matching_prefix(&context.owner, &context.repo, &prefix, 8)
                .await;
            FetchedSuggestions::Users { prefix, users }
        }
    }
}

/// Turn fetched GitHub data into suggestion rows (and prime the validation cache with
/// issue data so hovers are instant), then install them if the popup still matches.
fn apply_fetched(editor: &mut Editor, fetched: FetchedSuggestions) {
    match fetched {
        FetchedSuggestions::Issues { prefix, issues } => {
            let context = editor.github_context().cloned();
            let cache = editor.github_validation_cache().clone();
            let mut suggestions = Vec::with_capacity(issues.len());
            for issue in issues {
                if let Some(ctx) = &context {
                    cache.set_valid(
                        GitHubRef::Issue {
                            owner: ctx.owner.clone(),
                            repo: ctx.repo.clone(),
                            number: issue.number,
                        },
                        Some(ValidatedRefData::Issue(issue.clone())),
                    );
                }
                suggestions.push(AutocompleteSuggestion::IssueOrPr {
                    number: issue.number,
                    symbol: issue.symbol().to_string(),
                    status: issue.status(),
                    title: issue.title,
                });
            }
            editor.apply_autocomplete_suggestions(AutocompleteTrigger::Issue, &prefix, suggestions);
        }
        FetchedSuggestions::Users { prefix, users } => {
            let suggestions = users
                .into_iter()
                .map(|u| AutocompleteSuggestion::User {
                    login: u.login,
                    name: u.name,
                })
                .collect();
            editor.apply_autocomplete_suggestions(AutocompleteTrigger::User, &prefix, suggestions);
        }
    }
}

fn rect_contains(rect: &ScreenRect, pos: (f32, f32)) -> bool {
    let (px, py) = (pos.0 as f64, pos.1 as f64);
    let &(x0, y0, x1, y1) = rect;
    px >= x0 && px <= x1 && py >= y0 && py <= y1
}

/// Index of the autocomplete row whose rect contains `pos` (physical px), if any.
fn ac_row_at(rects: &[ScreenRect], pos: (f32, f32)) -> Option<usize> {
    rects.iter().position(|r| rect_contains(r, pos))
}

/// The OS cursor for whatever's under the pointer: a hand (`Pointer`) when Ctrl/Cmd is
/// held over a Ctrl-clickable link (same hit-test → `link_at` path as the click); the
/// arrow (`Default`) over a clickable gutter chevron / autocomplete row and the chrome
/// around the body (outline panel, find bar, status bar); otherwise the I-beam (`Text`)
/// over the editable document body. The modifier is checked first so the (snapshot-
/// building) `link_at` lookup only runs during an actual Ctrl-hover.
/// Scrollbar geometry for the live surface (None when the document fits the viewport).
fn scrollbar_geometry(
    doc_engine: &DocEngine,
    state: &ActiveSurface,
) -> Option<scrollbar::Geometry> {
    let doc = doc_engine.doc.as_ref()?;
    let width = state.surface.config.width as f32;
    let height = state.doc_height();
    let (content_top, editor_h) = chrome_metrics(state.scale, height, state.find_bar_h);
    let doc_w = (width - outline_width(&doc_engine.editor, state.scale)).max(0.0);
    scrollbar::geometry(doc, doc_w, content_top, editor_h, state.scale)
}

/// Move the scrollbar thumb so its top sits at `thumb_top` (device px): set the scroll
/// offset, then lay out any height-estimated lines that came into view.
fn scrollbar_scroll_to(
    doc_engine: &mut DocEngine,
    state: &ActiveSurface,
    geo: &scrollbar::Geometry,
    thumb_top: f32,
) -> bool {
    let (w, vh, scale) = state.viewport();
    let remeasure = match doc_engine.doc.as_mut() {
        Some(doc) => {
            let target = scrollbar::scroll_for_thumb_top(doc, geo, vh, thumb_top);
            if (target - doc.scroll_y).abs() < 0.5 {
                return false;
            }
            scroll_trace::log(
                &format!("thumb top={thumb_top:.1} target={target:.1} before"),
                Some(doc),
                w,
                vh,
                scale,
            );
            doc.scroll_y = target;
            doc.clamp_scroll(vh);
            doc.needs_remeasure(vh)
        }
        None => return false,
    };
    scroll_trace::log("thumb scrolled", doc_engine.doc.as_ref(), w, vh, scale);
    if remeasure {
        doc_engine.rebuild_preserving_scroll(w, scale, vh);
        scroll_trace::log("thumb rebuilt", doc_engine.doc.as_ref(), w, vh, scale);
    }
    true
}

fn pointer_cursor(
    doc_engine: &mut DocEngine,
    state: &ActiveSurface,
    modifiers: ModifiersState,
    mouse_pos: (f32, f32),
    fold_chevrons: &[(usize, ScreenRect)],
    ac_rows: &[ScreenRect],
) -> CursorIcon {
    if (modifiers.control_key() || modifiers.super_key())
        && doc_engine
            .doc
            .as_ref()
            .and_then(|d| d.hit_test(mouse_pos.0, mouse_pos.1))
            .is_some_and(|off| doc_engine.editor.link_at(off).is_some())
    {
        return CursorIcon::Pointer;
    }
    if fold_chevrons
        .iter()
        .any(|(_, r)| rect_contains(r, mouse_pos))
        || ac_rows.iter().any(|r| rect_contains(r, mouse_pos))
    {
        return CursorIcon::Default;
    }
    if scrollbar_geometry(doc_engine, state).is_some_and(|g| g.hit(mouse_pos)) {
        return CursorIcon::Default;
    }
    // The editable body is the editor content minus the right-docked outline panel; the
    // find bar and status bar sit below `editor_h`. I-beam inside, arrow over the chrome.
    let width = state.surface.config.width as f32;
    let height = state.doc_height();
    let outline_w = outline_width(&doc_engine.editor, state.scale);
    let (_, editor_h) = chrome_metrics(state.scale, height, state.find_bar_h);
    let body: ScreenRect = (0.0, 0.0, (width - outline_w) as f64, editor_h as f64);
    if rect_contains(&body, mouse_pos) {
        CursorIcon::Text
    } else {
        CursorIcon::Default
    }
}

/// Recompute the pointer cursor and apply it, but only call `set_cursor` on a real change.
#[allow(clippy::too_many_arguments)]
fn update_pointer_cursor(
    doc_engine: &mut DocEngine,
    state: &ActiveSurface,
    modifiers: ModifiersState,
    mouse_pos: (f32, f32),
    fold_chevrons: &[(usize, ScreenRect)],
    ac_rows: &[ScreenRect],
    current: &mut CursorIcon,
) {
    let icon = pointer_cursor(
        doc_engine,
        state,
        modifiers,
        mouse_pos,
        fold_chevrons,
        ac_rows,
    );
    if icon != *current {
        *current = icon;
        state.window.set_cursor(icon);
    }
}

/// Paint the clipped document body (diff backgrounds, quote gutters, rules, images,
/// selection, glyphs, caret) plus the bottom status bar into `scene`. Overlays
/// (hover popover / autocomplete) differ per caller and are drawn separately. Shared by
/// the live redraw and the headless snapshot so the golden frame can't drift from the
/// real one. Takes the doc-engine's fields individually so callers can pass a `doc`
/// borrowed from the same engine alongside `&mut` access to its text engine.
#[allow(clippy::too_many_arguments)]
fn paint_document(
    scene: &mut Scene,
    engine: &mut TextEngine,
    theme: &EditorTheme,
    editor: &Editor,
    doc: &DocLayout,
    content_top: f32,
    editor_h: f32,
    find_h: f32,
    width: f32,
    height: f32,
    scale: f32,
    gutter_hover: Option<usize>,
    // Line numbers: the document font size (numbers are drawn smaller), or `None`.
    line_numbers: Option<f32>,
) -> (Option<FindButtonRects>, Vec<(usize, ScreenRect)>) {
    // The outline panel (when open) reserves a right strip; clip the document body to the
    // reduced region so glyphs/backgrounds never draw under the panel.
    let outline_w = outline_width(editor, scale);
    let doc_w = (width - outline_w).max(0.0);
    let clip = Rect::new(
        0.0,
        content_top as f64,
        doc_w as f64,
        (content_top + editor_h) as f64,
    );
    scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
    // Draw order (all before glyphs): code block panels, diff row/word bg, quote
    // gutters, rules, images, then selection.
    doc.draw_code_blocks(engine, scene, editor_h);
    doc.draw_added_backgrounds(scene, editor_h);
    doc.draw_blockquote_gutters(scene, editor_h);
    doc.draw_horizontal_rules(scene, editor_h);
    doc.draw_images(engine, scene, editor_h);
    doc.draw_tables(engine, scene, editor_h);
    // Faint yellow tint under every visible find match EXCEPT the active one, which is
    // drawn last (below) in a distinct warm orange so the current match reads clearly.
    if let Some(find) = editor.find_state()
        && !find.matches.is_empty()
    {
        let tint = peniko_color_alpha(theme.yellow, 0.16);
        let (first, last) = doc.visible_range(editor_h);
        for (i, m) in find.matches.iter().enumerate() {
            if find.active == Some(i) {
                continue;
            }
            if doc.line_of(m.end) < first || doc.line_of(m.start) >= last {
                continue;
            }
            for (x0, y0, x1, y1) in doc.selection_rects(m.clone()) {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    tint,
                    None,
                    &Rect::new(x0, y0, x1, y1),
                );
            }
        }
    }
    if let Some(sel) = editor.selection_range() {
        let color = peniko_color(theme.selection);
        for (x0, y0, x1, y1) in doc.selection_rects(sel) {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                color,
                None,
                &Rect::new(x0, y0, x1, y1),
            );
        }
    }
    // The active find match, drawn last (over the selection) in a distinct warm orange
    // with an outline, so the current match stands out from the faint-yellow others.
    if let Some(find) = editor.find_state()
        && let Some(active) = find.active
        && let Some(m) = find.matches.get(active)
    {
        let fill = peniko_color_alpha(theme.orange, 0.34);
        let border = peniko_color_alpha(theme.orange, 0.95);
        let stroke = Stroke::new(1.5 * scale as f64);
        for (x0, y0, x1, y1) in doc.selection_rects(m.clone()) {
            let r = Rect::new(x0, y0, x1, y1);
            scene.fill(Fill::NonZero, Affine::IDENTITY, fill, None, &r);
            scene.stroke(&stroke, Affine::IDENTITY, border, None, &r);
        }
    }
    doc.draw(engine, scene, editor_h);
    let cw = CARET_WIDTH * scale;
    let caret = doc
        .preedit_caret_rect(cw)
        .or_else(|| doc.caret_rect(editor.cursor_position(), cw));
    if let Some((x0, y0, x1, y1)) = caret {
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            peniko_color(theme.foreground),
            None,
            &Rect::new(x0, y0, x1.max(x0 + 1.0), y1),
        );
    }
    let fold_rects = draw_fold_chevrons(scene, theme, editor, doc, editor_h, gutter_hover, scale);
    if let Some(font_size) = line_numbers {
        draw_line_numbers(scene, engine, theme, editor, doc, editor_h, font_size, scale);
    }
    scene.pop_layer();

    // Chrome (bottom, above the OS-native title bar): the find bar sits in the gap
    // between the document and the status bar, so the status bar starts below it.
    let bar_top = (content_top + editor_h) as f64;
    let status_top = bar_top + find_h as f64;
    let btn_rects = if find_h > 0.0
        && let Some(find) = editor.find_state()
    {
        draw_find_bar(
            engine,
            scene,
            theme,
            find,
            &BarRect {
                x0: 0.0,
                y0: bar_top,
                x1: width as f64,
                y1: bar_top + find_h as f64,
            },
            scale,
        )
    } else {
        None
    };
    let info = build_status_info(editor, doc, editor_h);
    draw_status_bar(
        engine,
        scene,
        theme,
        &BarRect {
            x0: 0.0,
            y0: status_top,
            x1: width as f64,
            y1: height as f64,
        },
        &info,
        scale,
    );
    (btn_rects, fold_rects)
}

/// Draw the source line numbers in the left margin, right-aligned just left of the fold
/// chevrons: one per visible buffer line, at the top of its first row. Lines with no
/// height of their own (folded away, or collapsed inside a rendered mermaid/math block)
/// are skipped — they all sit at the next line's y, so their numbers would pile up
/// there. The cursor's line is drawn brighter.
#[allow(clippy::too_many_arguments)]
fn draw_line_numbers(
    scene: &mut Scene,
    engine: &mut TextEngine,
    theme: &EditorTheme,
    editor: &Editor,
    doc: &DocLayout,
    editor_h: f32,
    font_size: f32,
    scale: f32,
) {
    let (first, last) = doc.visible_range(editor_h);
    let hidden = editor.hidden_line_ranges();
    let cursor_line = editor.line_of(editor.cursor_position());
    let size = line_number_size(font_size);
    let right = doc.body_left() - 22.0 * scale;
    // Vertically, center the number on the document's first text row.
    let row_h = font_size * LINE_HEIGHT * scale;
    for line in first..last {
        if hidden.iter().any(|r| r.contains(&line)) || doc.line_is_collapsed(line) {
            continue;
        }
        let Some(top) = doc.line_top_screen(line) else {
            continue;
        };
        let color = if line == cursor_line {
            peniko_color(theme.foreground)
        } else {
            peniko_color(theme.comment)
        };
        let label = (line + 1).to_string();
        let layout = engine.build_line(&label, scale, size, LINE_HEIGHT, color, None, &[]);
        let first_row = row_h.min(doc.line_text_height(line).max(layout.height()));
        let y = top + (first_row - layout.height()) / 2.0;
        engine.draw_line(scene, &layout, (right - layout.width(), y));
    }
}

/// Heading depth for a `Ctrl+Shift+<n>` fold-to-level shortcut, from the physical digit
/// key (1..6), or `None` for any other key.
fn fold_level_key(key: PhysicalKey) -> Option<u8> {
    match key {
        PhysicalKey::Code(KeyCode::Digit1) => Some(1),
        PhysicalKey::Code(KeyCode::Digit2) => Some(2),
        PhysicalKey::Code(KeyCode::Digit3) => Some(3),
        PhysicalKey::Code(KeyCode::Digit4) => Some(4),
        PhysicalKey::Code(KeyCode::Digit5) => Some(5),
        PhysicalKey::Code(KeyCode::Digit6) => Some(6),
        _ => None,
    }
}

/// Draw fold chevrons in the left gutter beside every visible foldable heading, and
/// return their hit rects keyed by heading byte offset. A folded heading shows a
/// persistent ▸; an expanded heading shows ▾ only while its gutter is hovered.
fn draw_fold_chevrons(
    scene: &mut Scene,
    theme: &EditorTheme,
    editor: &Editor,
    doc: &DocLayout,
    editor_h: f32,
    hover: Option<usize>,
    scale: f32,
) -> Vec<(usize, ScreenRect)> {
    let (first, last) = doc.visible_range(editor_h);
    // Anchors collapsed inside a folded ancestor are zero-height but still fall inside the
    // contiguous visible-index span — exclude them so no stray chevron paints at the
    // ancestor's y.
    let hidden = editor.hidden_line_ranges();
    let visible_foldable = |line: usize| -> bool {
        line >= first && line < last && !hidden.iter().any(|r| r.contains(&line))
    };
    // Every visible, non-collapsed, foldable anchor — headings, list items, and callouts
    // alike; the draw + hit-rect geometry below is identical for all kinds.
    let anchors: Vec<(usize, usize)> = editor
        .fold_anchors()
        .into_iter()
        .filter(|a| visible_foldable(a.line))
        .map(|a| (a.line, a.byte_offset))
        .collect();

    let body_left = doc.body_left();
    let s = 9.0 * scale; // chevron glyph size (device px)
    let mut rects: Vec<(usize, ScreenRect)> = Vec::new();
    for (line, byte_offset) in anchors {
        let Some(top) = doc.line_top_screen(line) else {
            continue;
        };
        // The fold set holds both heading and list-item offsets, so this reports either.
        let folded = editor.is_heading_folded(byte_offset);
        // Full-height gutter hit target for easy clicking; the glyph is centered in it.
        let row_h = doc.line_text_height(line);
        let hit: ScreenRect = (
            (body_left - 20.0 * scale) as f64,
            top as f64,
            (body_left - 2.0 * scale) as f64,
            (top + row_h) as f64,
        );
        rects.push((byte_offset, hit));
        // Expanded chevrons only appear on hover; folded ones are always shown.
        if !folded && hover != Some(byte_offset) {
            continue;
        }
        let cx = body_left - 13.0 * scale;
        let cy = top + row_h / 2.0;
        let color = if hover == Some(byte_offset) {
            peniko_color(theme.foreground)
        } else {
            peniko_color(theme.comment)
        };
        let mut tri = BezPath::new();
        if folded {
            // ▸ pointing right.
            tri.move_to(Point::new((cx - s * 0.3) as f64, (cy - s * 0.5) as f64));
            tri.line_to(Point::new((cx - s * 0.3) as f64, (cy + s * 0.5) as f64));
            tri.line_to(Point::new((cx + s * 0.45) as f64, cy as f64));
        } else {
            // ▾ pointing down.
            tri.move_to(Point::new((cx - s * 0.5) as f64, (cy - s * 0.3) as f64));
            tri.line_to(Point::new((cx + s * 0.5) as f64, (cy - s * 0.3) as f64));
            tri.line_to(Point::new(cx as f64, (cy + s * 0.45) as f64));
        }
        tri.close_path();
        scene.fill(Fill::NonZero, Affine::IDENTITY, color, None, &tri);
    }
    rects
}

impl ApplicationHandler<WritEvent> for App {
    /// A tokio task finished (validation/suggestion). Results are already in the
    /// shared caches; rebuild the doc (ref colors may have changed) and redraw.
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: WritEvent) {
        if let WritEvent::SaveAsPicked { id, path, then } = event {
            self.file_dialog_open = false;
            // Cancelled: a close that was waiting for this save is abandoned too.
            if let Some(path) = path {
                self.finish_save_as(id, path, then, event_loop);
            }
            return;
        }
        if let WritEvent::OpenPicked(paths) = event {
            self.file_dialog_open = false;
            self.open_paths(paths);
            return;
        }
        if let WritEvent::ExportPicked { id, path } = event {
            self.file_dialog_open = false;
            if let Some(path) = path {
                self.run_export(id, path);
            }
            return;
        }
        if let WritEvent::ExportDone(result) = event {
            self.export_running = None;
            match result {
                Ok(path) => {
                    self.modal = Some(PendingModal::Exported { path });
                    self.redraw();
                }
                Err(e) => self.show_message("Export failed", e),
            }
            return;
        }
        if let WritEvent::ImagePicked { id, path } = event {
            self.file_dialog_open = false;
            if let Some(path) = path
                && self.tabs[self.active_tab].id == id
            {
                let doc_dir = self.active_doc_dir();
                let (_, sel) = self.text_and_selection();
                self.apply_format(crate::format::insert_image(sel, &path, doc_dir.as_deref()));
            }
            return;
        }
        if let WritEvent::FilesDropped(paths) = event {
            self.files_dropped(paths);
            return;
        }
        if let WritEvent::InstanceOpen(paths) = event {
            self.open_paths(paths);
            if let Some(state) = self.state.as_ref() {
                state.window.set_minimized(false);
                state.window.focus_window();
            }
            return;
        }
        if let WritEvent::GitBaseChanged(id) = event {
            if id != self.tabs[self.active_tab].id {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                    tab.base_stale = true;
                }
                return;
            }
            let Some(path) = self.doc_engine.editor.file_path().map(|p| p.to_path_buf()) else {
                return;
            };
            let generation = self.reloads.begin_base();
            let proxy = self.proxy.clone();
            self.runtime.spawn_blocking(move || {
                let _ = proxy.send_event(WritEvent::GitBaseReloaded(
                    generation,
                    head_blob_text(&path),
                ));
            });
            return;
        }
        if let WritEvent::GitBaseReloaded(generation, base_text) = event {
            if self
                .reloads
                .apply_base(&mut self.doc_engine.editor, generation, base_text)
            {
                self.pending_rebuild = true;
                if let Some(state) = self.state.as_ref() {
                    state.window.request_redraw();
                }
            }
            return;
        }
        if let WritEvent::FileChanged(id) = event {
            if id != self.tabs[self.active_tab].id {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                    tab.file_stale = true;
                }
                return;
            }
            let Some(path) = self.doc_engine.editor.file_path().map(|p| p.to_path_buf()) else {
                return;
            };
            let ticket = self.reloads.begin_file(&self.doc_engine.editor);
            let last_mtime = self.doc_engine.editor.last_save_mtime();
            let proxy = self.proxy.clone();
            self.runtime.spawn_blocking(move || {
                if let Some(data) = Editor::read_reload(&path, last_mtime, false) {
                    let _ = proxy.send_event(WritEvent::FileReloaded(ticket, data));
                }
            });
            return;
        }
        // The off-thread read finished: apply it (cheap parse/diff) and treat it like an
        // edit (refresh detection / revalidate refs / reload images).
        if let WritEvent::FileReloaded(ticket, data) = event {
            if !self.reloads.is_latest(ticket) {
                return; // a newer read is on its way
            }
            // Unsaved changes and a different disk version: never overwrite silently.
            // Without unsaved changes, ask only if the preferences say so.
            let editor = &self.doc_engine.editor;
            let ask = editor.is_dirty() || self.prefs.external_change == ExternalChange::Ask;
            if self.ui.is_some() && ask && !editor.state.buffer.content_eq(&data.0) {
                // A newer disk version replaces the one still waiting for an answer.
                self.tabs[self.active_tab].conflict = Some(DiskConflict { ticket, data });
                self.redraw();
                return;
            }
            if self
                .reloads
                .apply_file(&mut self.doc_engine.editor, ticket, data)
            {
                self.doc_engine.editor.note_disk_state();
                self.after_reload();
            }
            return;
        }
        // GithubUpdated may carry fetched autocomplete suggestions built on the worker;
        // install them before rebuilding. ImageLoaded only changed the shared image
        // cache. Both then rebuild-preserving-scroll (validated refs recolor / a loaded
        // image reflows its line's height) and redraw.
        if matches!(event, WritEvent::GithubUpdated) {
            let fetched = self.ac_slot.lock().unwrap().take();
            if let Some(fetched) = fetched {
                apply_fetched(&mut self.doc_engine.editor, fetched);
            }
        }
        if profile::enabled() && matches!(event, WritEvent::ImageLoaded) {
            eprintln!(
                "[markdone-mem] image/render loaded (tab {}) | {}",
                self.active_tab,
                allocator_stats()
            );
        }
        // Defer the relayout to the next redraw so a burst of completions (many refs /
        // images finishing in the same frame) collapses into a single rebuild.
        if let Some(state) = self.state.as_ref() {
            self.pending_rebuild = true;
            state.window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        // Native decorations (shadow, rounded corners, resize, snap — all free), but
        // request a dark title bar to match the editor instead of a bright one over
        // dark content. On Wayland CSD this also honors WINIT_WAYLAND_CSD_THEME.
        let attrs = Window::default_attributes()
            .with_title(self.title.clone())
            .with_theme(Some(if self.doc_engine.theme.is_light() {
                Theme::Light
            } else {
                Theme::Dark
            }));
        // Application id (Wayland app_id / X11 WM_CLASS): matches `markdone.desktop`,
        // so the desktop shows the right icon and groups the windows.
        #[cfg(all(unix, not(target_os = "macos")))]
        let attrs = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            WindowAttributesExtWayland::with_name(attrs, "markdone", "markdone")
        };
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        window.set_ime_allowed(true);
        self.start_wayland_dnd(&window);
        let size = window.inner_size();

        let surface = pollster::block_on(self.context.create_surface(
            window.clone(),
            size.width.max(1),
            size.height.max(1),
            wgpu::PresentMode::AutoVsync,
        ))
        .expect("failed to create surface");

        let dev = &self.context.devices[surface.dev_id];
        self.renderer = Some(
            Renderer::new(
                &dev.device,
                RendererOptions {
                    use_cpu: false,
                    antialiasing_support: vello::AaSupport::area_only(),
                    num_init_threads: None,
                    pipeline_cache: None,
                },
            )
            .expect("failed to create Vello renderer"),
        );

        let scale = window.scale_factor() as f32;
        sync_image_raster_width(scale, self.doc_engine.max_content_width);
        // Find is closed at startup, so no find-bar inset yet.
        let menu_bar = std::env::var_os("MARKDONE_NO_UI").is_none();
        let top_inset = if menu_bar {
            UiLayer::top_inset(scale)
        } else {
            0.0
        };
        let doc_h = (size.height as f32 - top_inset).max(1.0);
        let (_, editor_h) = chrome_metrics(scale, doc_h, 0.0);
        let doc = self
            .doc_engine
            .rebuild(size.width as f32, scale, 0, editor_h);
        self.doc_engine.doc = Some(doc);
        // The loop parks on Wait, so the first frame must not depend on input,
        // a resize, or an asynchronous resource finishing.
        window.request_redraw();
        self.ui = menu_bar.then(|| {
            let ui_font = self.doc_engine.text_engine.system_ui_font();
            let fallbacks = self.doc_engine.text_engine.ui_fallback_fonts();
            UiLayer::new(
                &window,
                &dev.device,
                surface.config.format,
                &self.doc_engine.theme,
                ui_font,
                fallbacks,
            )
        });
        self.state = Some(ActiveSurface {
            surface,
            window,
            scale,
            find_bar_h: 0.0,
            menu_bar,
        });
        // Validate refs already present in the loaded file, and start loading images
        // (spawn_validations already calls sync_images).
        self.spawn_validations();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let _event_timer = match &event {
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                profile::EventTimer::new("key")
            }
            WindowEvent::Ime(winit::event::Ime::Commit(_)) => {
                profile::EventTimer::new("ime-commit")
            }
            WindowEvent::Ime(winit::event::Ime::Preedit(..)) => {
                profile::EventTimer::new("ime-preedit")
            }
            _ => None,
        };
        let dialog_view = if matches!(event, WindowEvent::RedrawRequested) {
            self.dialog_view()
        } else {
            None
        };
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if let Some(ui) = self.ui.as_mut() {
            let menu_was_open = ui.menu_open();
            // A selection or scrollbar drag keeps following the pointer (and ends on
            // release) even over the bars at the top.
            let doc_drag = self.selection_drag.is_some() || self.scrollbar_drag.is_some();
            if ui.on_window_event(&state.window, &event)
                && !(doc_drag
                    && matches!(
                        event,
                        WindowEvent::CursorMoved { .. } | WindowEvent::MouseInput { .. }
                    ))
            {
                return;
            }
            // A dialog or the Preferences own all input until closed; a disk conflict owns
            // the document (keys included) until it is answered.
            if (self.modal.is_some() || self.prefs_open)
                && matches!(
                    event,
                    WindowEvent::KeyboardInput { .. }
                        | WindowEvent::MouseInput { .. }
                        | WindowEvent::MouseWheel { .. }
                        | WindowEvent::CursorMoved { .. }
                        | WindowEvent::Ime(_)
                )
            {
                state.window.request_redraw();
                return;
            }
            if self.tabs[self.active_tab].conflict.is_some()
                && matches!(event, WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_))
            {
                state.window.request_redraw();
                return;
            }
            // A click outside an open menu only closes the menu. (Only the press: the
            // release must still end a drag that began before.)
            if menu_was_open
                && matches!(
                    event,
                    WindowEvent::MouseInput {
                        state: ElementState::Pressed,
                        ..
                    } | WindowEvent::MouseWheel { .. }
                )
            {
                return;
            }
        }
        match event {
            WindowEvent::CloseRequested => self.run_command(Command::Quit, event_loop),
            WindowEvent::Focused(true) => self.check_active_on_disk(),
            WindowEvent::DroppedFile(path) => self.files_dropped(vec![path]),
            WindowEvent::Focused(false) => {
                self.selection_drag = None;
                self.drag_scroll_dy = 0.0;
                self.last_drag_tick = None;
            }
            WindowEvent::Occluded(false) => state.window.request_redraw(),
            WindowEvent::Resized(size) => {
                self.context.resize_surface(
                    &mut state.surface,
                    size.width.max(1),
                    size.height.max(1),
                );
                // A width change re-wraps, so re-run detection + relayout at the anchor —
                // the same preserve-scroll path used for recolor/remeasure rebuilds.
                let (w, editor_h, scale) = state.viewport();
                self.doc_engine
                    .rebuild_preserving_scroll(w, scale, editor_h);
                state.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.scale = scale_factor as f32;
                sync_image_raster_width(state.scale, self.doc_engine.max_content_width);
                // Text metrics and the find bar are scale-dependent, so a DPI change
                // (e.g. dragging the window to another monitor) must recompute the bar
                // height and relayout at the new scale, not just redraw stale geometry.
                sync_find_chrome(&mut self.doc_engine, state);
                state.window.request_redraw();
            }
            // Ctrl/Cmd + wheel zooms the document text (like a browser) instead of scrolling:
            // wheel up = larger. Notches count one step each; touchpad pixel deltas are
            // accumulated into steps so one gesture doesn't jump across many sizes.
            WindowEvent::MouseWheel { delta, .. }
                if self.modifiers.control_key() || self.modifiers.super_key() =>
            {
                self.wheel_zoom_acc += match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / WHEEL_ZOOM_PX_PER_STEP,
                };
                let steps = self.wheel_zoom_acc.trunc();
                if steps != 0.0 {
                    self.wheel_zoom_acc -= steps;
                    self.zoom(Some(steps));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y * WHEEL_LINE_STEP * state.scale,
                    MouseScrollDelta::PixelDelta(p) => -p.y as f32,
                };
                // Coalesced: accumulated here, applied once per frame in RedrawRequested
                // (a fast wheel/touchpad can deliver several events per frame, and each
                // one used to trigger its own relayout).
                self.wheel_pending_dy += dy;
                self.hovered = None; // stored anchor goes stale on scroll
                if self.doc_engine.doc.is_some() {
                    state.window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                // Pressing/releasing Ctrl while stationary over a link flips the pointer.
                update_pointer_cursor(
                    &mut self.doc_engine,
                    state,
                    self.modifiers,
                    self.mouse_pos,
                    &self.fold_chevron_rects,
                    &self.ac_row_rects,
                    &mut self.cursor_icon,
                );
            }
            WindowEvent::Ime(winit::event::Ime::Enabled) => {
                self.doc_engine.preedit = None;
            }
            // In-progress composition: splice it into the caret line (render-only, no
            // buffer mutation) and move the OS candidate popup to the composition caret.
            WindowEvent::Ime(winit::event::Ime::Preedit(text, cursor)) => {
                // While the find bar owns focus, never splice composition into the document
                // behind it. When the bar is open but unfocused, composition targets the
                // document as usual.
                if self
                    .doc_engine
                    .editor
                    .find_state()
                    .is_some_and(|f| f.focused)
                {
                    state.window.request_redraw();
                    return;
                }
                // Some IMEs (GNOME/IBus on Wayland) spam empty preedit events at idle. An
                // empty preedit with nothing to clear is a no-op — skip it, or every frame
                // would trigger a full relayout (murder on an expensive doc, e.g. mermaid).
                if text.is_empty() && self.doc_engine.preedit.is_none() {
                    return;
                }
                self.doc_engine.preedit = (!text.is_empty()).then_some(Preedit { text, cursor });
                let (w, vh, _) = state.viewport();
                self.doc_engine
                    .rebuild_preserving_scroll(w, state.scale, vh);
                if let Some(doc) = self.doc_engine.doc.as_ref() {
                    let cw = CARET_WIDTH * state.scale;
                    let rect = doc
                        .preedit_caret_rect(cw)
                        .or_else(|| doc.caret_rect(self.doc_engine.editor.cursor_position(), cw));
                    if let Some((x0, y0, x1, y1)) = rect {
                        state.window.set_ime_cursor_area(
                            PhysicalPosition::new(x0, y0 + state.top_inset() as f64),
                            PhysicalSize::new((x1 - x0).max(1.0), (y1 - y0).max(1.0)),
                        );
                    }
                }
                state.window.request_redraw();
            }
            // Commit: clear the composition, then insert the finalized text.
            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                // While the find bar owns focus, route committed IME text (CJK/dead-key/
                // accent) to the focused field instead of the document.
                if self
                    .doc_engine
                    .editor
                    .find_state()
                    .is_some_and(|f| f.focused)
                {
                    let selection_before = self.doc_engine.editor.state.selection;
                    let mut search_edited = false;
                    if !text.is_empty() {
                        let to_replace = self
                            .doc_engine
                            .editor
                            .find_state()
                            .is_some_and(|f| f.focus == FieldFocus::Replace);
                        if let Some(find) = self.doc_engine.editor.find_state_mut() {
                            if to_replace {
                                find.replace.insert(&text);
                            } else {
                                find.search.insert(&text);
                            }
                        }
                        if !to_replace {
                            search_edited = true;
                            self.doc_engine.editor.find_rescan();
                        }
                    }
                    if search_edited || self.doc_engine.editor.state.selection != selection_before {
                        let (w, vh, scale) = state.viewport();
                        apply_edit_effects(
                            &mut self.doc_engine,
                            &mut self.hovered,
                            &self.runtime,
                            &self.proxy,
                            None,
                            &state.window,
                            w,
                            scale,
                            vh,
                        );
                    }
                    state.window.request_redraw();
                    return;
                }
                let had_preedit = self.doc_engine.preedit.take().is_some();
                let (w, vh, _) = state.viewport();
                if !text.is_empty() {
                    self.doc_engine.editor.insert_str(&text);
                    apply_edit_effects(
                        &mut self.doc_engine,
                        &mut self.hovered,
                        &self.runtime,
                        &self.proxy,
                        None,
                        &state.window,
                        w,
                        state.scale,
                        vh,
                    );
                } else if had_preedit {
                    // Composition cancelled (empty commit): drop the spliced preedit.
                    self.doc_engine
                        .rebuild_preserving_scroll(w, state.scale, vh);
                    state.window.request_redraw();
                }
            }
            WindowEvent::Ime(winit::event::Ime::Disabled) => {
                if self.doc_engine.preedit.take().is_some() {
                    let (w, vh, _) = state.viewport();
                    self.doc_engine
                        .rebuild_preserving_scroll(w, state.scale, vh);
                    state.window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                // Document coordinates: origin at the top of the area below the menu bar.
                self.mouse_pos = (position.x as f32, position.y as f32 - state.top_inset());
                if self.ui.as_mut().is_some_and(UiLayer::take_cursor_released) {
                    // egui drew its own cursor over the menu: re-apply the editor's one.
                    self.cursor_icon = CursorIcon::Progress;
                }
                // Scrollbar thumb drag owns the pointer until release.
                if self.scrollbar_drag.is_some() {
                    // Coalesced: the thumb follows the pointer once per frame.
                    self.scrollbar_pending = true;
                    state.window.request_redraw();
                    return;
                }
                let sb_hover = scrollbar_geometry(&self.doc_engine, state)
                    .is_some_and(|g| g.hit(self.mouse_pos));
                if sb_hover != self.scrollbar_hover {
                    self.scrollbar_hover = sb_hover;
                    state.window.request_redraw();
                }
                // Fold-gutter hover: reveal the expanded (▾) chevron under the pointer.
                let new_gutter_hover = self
                    .fold_chevron_rects
                    .iter()
                    .find(|(_, r)| rect_contains(r, self.mouse_pos))
                    .map(|(off, _)| *off);
                if new_gutter_hover != self.gutter_hover {
                    self.gutter_hover = new_gutter_hover;
                    state.window.request_redraw();
                }
                if let Some(drag) = self.selection_drag.as_mut() {
                    if drag.update(self.mouse_pos) {
                        let (_, vh, _) = state.viewport();
                        self.drag_scroll_dy = drag_edge_velocity(self.mouse_pos.1, vh, state.scale);
                        // Coalesce pointer motion into one selection update per frame.
                        state.window.request_redraw();
                    }
                } else if self.doc_engine.editor.autocomplete().is_some() {
                    // Autocomplete popup open: the highlighted row follows the pointer.
                    if let Some(row) = ac_row_at(&self.ac_row_rects, self.mouse_pos) {
                        self.doc_engine.editor.autocomplete_select(row);
                        state.window.request_redraw();
                    }
                } else {
                    // Not dragging: update the hovered GitHub ref (popover source).
                    let new = self.doc_engine.doc.as_ref().and_then(|d| {
                        find_hover_target(
                            &self.doc_engine.editor,
                            d,
                            self.mouse_pos.0,
                            self.mouse_pos.1,
                        )
                    });
                    let changed = self.hovered.as_ref().map(|h| &h.reference)
                        != new.as_ref().map(|h| &h.reference);
                    self.hovered = new;
                    if changed {
                        state.window.request_redraw();
                    }
                }
                // I-beam over the body, hand on Ctrl-hover over a link, arrow over chrome
                // and clickable gutter/popup widgets (skip while drag-selecting).
                if self.selection_drag.is_none() {
                    update_pointer_cursor(
                        &mut self.doc_engine,
                        state,
                        self.modifiers,
                        self.mouse_pos,
                        &self.fold_chevron_rects,
                        &self.ac_row_rects,
                        &mut self.cursor_icon,
                    );
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                let (w, vh, scale) = state.viewport();
                if self.ui.is_none() || self.mouse_pos.1 < 0.0 || self.mouse_pos.1 >= vh {
                    return;
                }
                // Outside the selection, the right click first moves the caret there.
                if let Some(off) = self
                    .doc_engine
                    .doc
                    .as_ref()
                    .and_then(|d| d.hit_test(self.mouse_pos.0, self.mouse_pos.1))
                    && !self
                        .doc_engine
                        .editor
                        .selection_range()
                        .is_some_and(|r| r.start <= off && off <= r.end)
                {
                    self.doc_engine.editor.set_cursor(off);
                    self.doc_engine.rebuild_preserving_scroll(w, scale, vh);
                }
                self.context_menu_pending = true;
                state.window.request_redraw();
            }
            // The menu opens on release, where the pointer is then (as egui's own context
            // menus): opened on press, the release would read as a click outside it.
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Right,
                ..
            } => {
                if std::mem::take(&mut self.context_menu_pending) {
                    let top = state.top_inset();
                    self.context_menu = Some(egui::pos2(
                        self.mouse_pos.0 / state.scale,
                        (self.mouse_pos.1 + top) / state.scale,
                    ));
                    self.context_menu_open = true;
                    state.window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                self.selection_drag = None;
                self.drag_scroll_dy = 0.0;
                self.last_drag_tick = None;
                let (w, vh, _) = state.viewport();

                // Find-bar focus routing. The bar is docked at the bottom, so the document
                // occupies y < vh, the find strip [vh, vh + find_bar_h), the status bar
                // below that. A click in the strip focuses the bar (and picks the field by
                // row); a click in the document unfocuses it (keys then type into the
                // buffer) and falls through to normal caret placement.
                let in_find_bar = if self.doc_engine.editor.find_state().is_some() {
                    let bar_top = vh;
                    let bar_bottom = vh + state.find_bar_h;
                    let click_y = self.mouse_pos.1;
                    if click_y < bar_top {
                        if let Some(find) = self.doc_engine.editor.find_state_mut() {
                            find.focused = false;
                        }
                        false
                    } else if click_y < bar_bottom {
                        // Coarse which-row hit test: in Replace mode the lower half is the
                        // replace field, otherwise it's the search field (precise field
                        // rects aren't threaded back here).
                        let field = if self
                            .doc_engine
                            .editor
                            .find_state()
                            .is_some_and(|f| f.mode == FindMode::Replace)
                            && click_y >= bar_top + state.find_bar_h / 2.0
                        {
                            FieldFocus::Replace
                        } else {
                            FieldFocus::Search
                        };
                        if let Some(find) = self.doc_engine.editor.find_state_mut() {
                            find.focused = true;
                            find.focus = field;
                        }
                        true
                    } else {
                        false // status bar: leave focus unchanged
                    }
                } else {
                    false
                };

                // Find-bar Replace/All button click (only present in Replace mode).
                if let Some(rects) = self.find_btn_rects.as_ref() {
                    let hit_all = rect_contains(&rects.all, self.mouse_pos);
                    let hit_replace = rect_contains(&rects.replace, self.mouse_pos);
                    if hit_all || hit_replace {
                        if hit_all {
                            self.doc_engine.editor.find_replace_all();
                        } else {
                            self.doc_engine.editor.find_replace_current();
                        }
                        apply_edit_effects(
                            &mut self.doc_engine,
                            &mut self.hovered,
                            &self.runtime,
                            &self.proxy,
                            None,
                            &state.window,
                            w,
                            state.scale,
                            vh,
                        );
                        return;
                    }
                }

                // A click inside the find strip that missed the buttons just (re)focuses
                // the bar — redraw the focus ring and don't fall through to doc caret
                // placement (the doc's hit-test would otherwise snap the caret to the
                // nearest line for a click below the content).
                if in_find_bar {
                    state.window.request_redraw();
                    return;
                }

                // Autocomplete row click: accept that suggestion, don't move the caret.
                if self.doc_engine.editor.autocomplete().is_some()
                    && let Some(row) = ac_row_at(&self.ac_row_rects, self.mouse_pos)
                {
                    self.doc_engine.editor.autocomplete_select(row);
                    if self.doc_engine.editor.accept_autocomplete_suggestion() {
                        apply_edit_effects(
                            &mut self.doc_engine,
                            &mut self.hovered,
                            &self.runtime,
                            &self.proxy,
                            None,
                            &state.window,
                            w,
                            state.scale,
                            vh,
                        );
                    }
                    return;
                }

                // Scrollbar click: on the thumb, grab it where it was pressed; on the track,
                // jump so the thumb centers on the pointer, then keep dragging from there.
                if let Some(geo) = scrollbar_geometry(&self.doc_engine, state)
                    && geo.hit(self.mouse_pos)
                {
                    let y = self.mouse_pos.1;
                    let grab = if geo.on_thumb(y) {
                        y - geo.thumb_y0
                    } else {
                        let grab = geo.thumb_h() / 2.0;
                        if scrollbar_scroll_to(&mut self.doc_engine, state, &geo, y - grab) {
                            sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                        }
                        grab
                    };
                    self.scrollbar_drag = Some(grab);
                    self.hovered = None;
                    state.window.request_redraw();
                    return;
                }

                // Fold-gutter chevron click: toggle that heading's fold, don't place the
                // caret. Checked before the document hit-test since the chevron sits in
                // the left margin, outside the text body.
                if let Some(&(off, _)) = self
                    .fold_chevron_rects
                    .iter()
                    .find(|(_, r)| rect_contains(r, self.mouse_pos))
                {
                    // Modifier-clicks escalate scope on the same two axes for both kinds:
                    // Ctrl = breadth (all at this heading level / list depth), Shift = depth
                    // (recursive: this item + everything nested), Ctrl+Shift = breadth AND
                    // depth, plain = just this one.
                    let ctrl = self.modifiers.control_key() || self.modifiers.super_key();
                    let shift = self.modifiers.shift_key();
                    self.doc_engine.editor.apply_fold_gesture(off, ctrl, shift);
                    self.doc_engine
                        .rebuild_preserving_scroll(w, state.scale, vh);
                    state.window.request_redraw();
                    return;
                }

                if self.mouse_pos.1 >= vh {
                    return;
                }

                if let Some(off) = self
                    .doc_engine
                    .doc
                    .as_ref()
                    .and_then(|d| d.hit_test(self.mouse_pos.0, self.mouse_pos.1))
                {
                    // Ctrl/Cmd-click on a link (markdown link, naked URL, or GitHub ref)
                    // opens it in the browser instead of placing the caret.
                    if (self.modifiers.control_key() || self.modifiers.super_key())
                        && let Some(url) = self.doc_engine.editor.link_at(off)
                    {
                        self.follow_link(&url);
                        return;
                    }
                    // Clicking a checkbox toggles it (leaving the caret where it was)
                    // rather than placing the caret in the box.
                    if let Some(line) = self.doc_engine.editor.checkbox_at(off) {
                        self.doc_engine.editor.toggle_checkbox(line);
                        // A checkbox flip is a real buffer edit — run the full effect
                        // pipeline (find-rescan, ref/image sync) rather than a bare refresh
                        // that would leave find highlights and detection stale.
                        apply_edit_effects(
                            &mut self.doc_engine,
                            &mut self.hovered,
                            &self.runtime,
                            &self.proxy,
                            None,
                            &state.window,
                            w,
                            state.scale,
                            vh,
                        );
                        return;
                    }
                    // A click on an image block hit-tests to the line start (the image
                    // has no text). Note the line first; after the click reveals its raw
                    // markdown, re-place the caret at the clicked x on the revealed row
                    // (its short row no longer sits under the original click y).
                    let image_line = self.doc_engine.doc.as_ref().and_then(|d| {
                        let line = self.doc_engine.editor.line_of(off);
                        d.is_image_line(line).then_some(line)
                    });
                    // Advance the click-count state machine (winit gives no click count):
                    // within the time+distance threshold it cycles 1→2→3→1 (double = word,
                    // triple = line select), else restarts at 1. Inlined (not a &mut self
                    // method) so it uses disjoint fields while `state` stays borrowed.
                    let now = std::time::Instant::now();
                    let repeat = self.last_click.is_some_and(|(t, p)| {
                        now.duration_since(t).as_millis() <= MULTI_CLICK_MS
                            && (p.0 - self.mouse_pos.0).hypot(p.1 - self.mouse_pos.1)
                                <= MULTI_CLICK_DIST
                    });
                    self.click_count = if repeat {
                        (self.click_count % 3) + 1
                    } else {
                        1
                    };
                    self.last_click = Some((now, self.mouse_pos));
                    self.doc_engine
                        .editor
                        .click(off, self.modifiers.shift_key(), self.click_count);
                    self.doc_engine.refresh(w, state.scale, vh);
                    if let Some(line) = image_line
                        && let Some(off2) = self
                            .doc_engine
                            .doc
                            .as_ref()
                            .and_then(|d| d.offset_in_line_at_x(line, self.mouse_pos.0))
                    {
                        self.doc_engine.editor.set_cursor(off2);
                    }
                    self.selection_drag = Some(SelectionDrag::new(self.mouse_pos, state.scale));
                    // Moving the caret off an image line materializes it as an image
                    // block, whose URL now needs loading — so kick off fetches here too.
                    sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                    // Clicking into/out of a ref opens/closes the popup.
                    sync_autocomplete(
                        &mut self.doc_engine.editor,
                        &self.runtime,
                        &self.proxy,
                        &self.ac_gen,
                        &self.ac_slot,
                    );
                    state.window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                // A quick drag can release before its queued redraw. Flush only real
                // pending motion, then discard the gesture so it cannot affect a later click.
                if self.selection_drag.take().is_some_and(|drag| drag.pending) {
                    let (w, vh, scale) = state.viewport();
                    drag_extend_step(&mut self.doc_engine, self.mouse_pos, 0.0, w, scale, vh);
                    state.window.request_redraw();
                }
                self.drag_scroll_dy = 0.0;
                self.last_drag_tick = None;
                if self.scrollbar_drag.take().is_some() {
                    state.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                // Application commands: Ctrl+N new, Ctrl+O open, Ctrl+S save, Ctrl+Shift+S
                // save as, Ctrl+W close tab, Ctrl+Q quit (the last two ask about unsaved
                // changes).
                if event.logical_key == Key::Named(NamedKey::F5) {
                    self.run_command(Command::Reload, event_loop);
                    return;
                }
                // Formatting and clipboard shortcuts (not while typing in the find bar).
                if self.doc_engine.editor.find_state().is_none()
                    && let Some(format_cmd) = crate::commands::format_shortcut(
                        &event.logical_key,
                        event.physical_key,
                        self.modifiers.control_key() || self.modifiers.super_key(),
                        self.modifiers.shift_key(),
                        self.modifiers.alt_key(),
                    )
                {
                    self.run_command(format_cmd, event_loop);
                    return;
                }
                let cmd = self.modifiers.control_key() || self.modifiers.super_key();
                if cmd && let Key::Character(c) = &event.logical_key {
                    let shift = self.modifiers.shift_key();
                    let app_cmd = match c.as_str().to_ascii_lowercase().as_str() {
                        "n" if !shift => Some(Command::New),
                        "o" if !shift => Some(Command::Open),
                        "s" if shift => Some(Command::SaveAs),
                        "e" if !shift => Some(Command::ExportPdf),
                        "s" => Some(Command::Save),
                        "w" => Some(Command::CloseTab),
                        "q" => Some(Command::Quit),
                        "," => Some(Command::Preferences),
                        "+" | "=" => Some(Command::ZoomIn),
                        "-" => Some(Command::ZoomOut),
                        "0" => Some(Command::ResetZoom),
                        _ => None,
                    };
                    if let Some(app_cmd) = app_cmd {
                        self.run_command(app_cmd, event_loop);
                        return;
                    }
                }
                // Tab switching: Ctrl+Tab / Ctrl+Shift+Tab, Ctrl+PageDown / Ctrl+PageUp.
                if self.modifiers.control_key() && self.tabs.len() > 1 {
                    let n = self.tabs.len();
                    let step = match &event.logical_key {
                        Key::Named(NamedKey::Tab) if self.modifiers.shift_key() => Some(n - 1),
                        Key::Named(NamedKey::Tab) | Key::Named(NamedKey::PageDown) => Some(1),
                        Key::Named(NamedKey::PageUp) => Some(n - 1),
                        _ => None,
                    };
                    if let Some(step) = step {
                        self.switch_tab((self.active_tab + step) % n);
                        return;
                    }
                }
                let (w, vh, _) = state.viewport();

                // Find / Replace bar (Ctrl+F / Ctrl+H). Opening or closing changes the
                // editor height, so re-sync the find-bar inset and reflow the document.
                // While open, this intercept sits ABOVE clipboard/autocomplete/apply_key
                // and captures EVERY keystroke, so nothing leaks into the document.
                let open_find = cmd
                    && matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("f"));
                let open_replace = cmd
                    && matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("h"));
                if open_find || open_replace {
                    self.doc_engine.editor.open_find(open_replace);
                    state.find_bar_h = find_bar_height(&self.doc_engine.editor, state.scale);
                    let (fw, feh, scale) = state.viewport();
                    apply_edit_effects(
                        &mut self.doc_engine,
                        &mut self.hovered,
                        &self.runtime,
                        &self.proxy,
                        None,
                        &state.window,
                        fw,
                        scale,
                        feh,
                    );
                    return;
                }

                // Outline panel (Ctrl+Shift+O): the panel claims a right strip and the
                // document reflows into the remaining width (a pure inset — the window /
                // surface size is untouched, so no framebuffer scaling).
                let shift = self.modifiers.shift_key();
                if cmd
                    && shift
                    && matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("o"))
                {
                    self.doc_engine.editor.toggle_outline();
                    let (ow, oeh, oscale) = state.viewport();
                    self.doc_engine.rebuild_preserving_scroll(ow, oscale, oeh);
                    state.window.request_redraw();
                    return;
                }

                // Heading folding: Ctrl/Cmd+Shift+[ / ] fold / unfold the current section;
                // Ctrl/Cmd+Alt+[ / ] fold / unfold all headings; Ctrl/Cmd+Shift+1..6 fold
                // to that heading depth (Sublime's chord-free "fold by level"). Matched on
                // physical keys so Shift's `{`/`}`/`!` remaps don't matter.
                let alt = self.modifiers.alt_key();
                if cmd && (shift || alt) {
                    let bracket = match event.physical_key {
                        PhysicalKey::Code(KeyCode::BracketLeft) => Some(true),
                        PhysicalKey::Code(KeyCode::BracketRight) => Some(false),
                        _ => None,
                    };
                    let level = shift.then(|| fold_level_key(event.physical_key)).flatten();
                    let acted = if let Some(left) = bracket {
                        match (shift, left) {
                            (true, true) => self.doc_engine.editor.fold_at_cursor(),
                            (true, false) => self.doc_engine.editor.unfold_at_cursor(),
                            (false, true) => self.doc_engine.editor.fold_all_headings(),
                            (false, false) => self.doc_engine.editor.unfold_all(),
                        }
                        true
                    } else if let Some(level) = level {
                        self.doc_engine.editor.fold_to_level(level);
                        true
                    } else {
                        false
                    };
                    if acted {
                        let (fw, feh, fscale) = state.viewport();
                        self.doc_engine.rebuild_preserving_scroll(fw, fscale, feh);
                        self.doc_engine.scroll_to_cursor(fw, fscale, feh);
                        sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                        state.window.request_redraw();
                        return;
                    }
                }

                // Escape closes the find bar regardless of which pane holds focus (so it
                // works even after clicking into the document unfocuses the bar).
                if self.doc_engine.editor.find_state().is_some()
                    && matches!(&event.logical_key, Key::Named(NamedKey::Escape))
                {
                    self.doc_engine.editor.close_find();
                    sync_find_chrome(&mut self.doc_engine, state);
                    state.window.request_redraw();
                    return;
                }

                // The find intercept swallows keys ONLY while the bar owns focus. When it's
                // open but unfocused (the document was clicked), keys fall through to the
                // normal document handling below so typing/arrows/undo edit the buffer.
                if self
                    .doc_engine
                    .editor
                    .find_state()
                    .is_some_and(|f| f.focused)
                {
                    let alt = self.modifiers.alt_key();
                    let shift = self.modifiers.shift_key();

                    // Undo/redo always act on the DOCUMENT, even with the find field
                    // focused, so a replace can be undone without clicking away. The buffer
                    // changed, so `apply_edit_effects` rescans + reveals the active match.
                    let is_z = matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("z"));
                    let is_y = matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("y"));
                    if cmd && (is_z || is_y) {
                        if is_y || (is_z && shift) {
                            self.doc_engine.editor.redo();
                        } else {
                            self.doc_engine.editor.undo();
                        }
                        self.doc_engine.editor.find_rescan();
                        apply_edit_effects(
                            &mut self.doc_engine,
                            &mut self.hovered,
                            &self.runtime,
                            &self.proxy,
                            None,
                            &state.window,
                            w,
                            state.scale,
                            vh,
                        );
                        return;
                    }
                    let selection_before = self.doc_engine.editor.state.selection;
                    let mut reveal_find_match = false;
                    match &event.logical_key {
                        Key::Named(NamedKey::Enter) => {
                            let replace_mode = self
                                .doc_engine
                                .editor
                                .find_state()
                                .is_some_and(|f| f.mode == FindMode::Replace);
                            let focus_replace = self
                                .doc_engine
                                .editor
                                .find_state()
                                .is_some_and(|f| f.focus == FieldFocus::Replace);
                            if replace_mode && (cmd || (!shift && focus_replace)) {
                                // Ctrl/Cmd+Enter replaces every match; a plain Enter in the
                                // replace field replaces the active match and advances. Both
                                // mutate the buffer, so run the full post-edit flow.
                                if cmd {
                                    self.doc_engine.editor.find_replace_all();
                                } else {
                                    self.doc_engine.editor.find_replace_current();
                                }
                                apply_edit_effects(
                                    &mut self.doc_engine,
                                    &mut self.hovered,
                                    &self.runtime,
                                    &self.proxy,
                                    None,
                                    &state.window,
                                    w,
                                    state.scale,
                                    vh,
                                );
                                return;
                            } else {
                                reveal_find_match = if shift {
                                    self.doc_engine.editor.find_prev()
                                } else {
                                    self.doc_engine.editor.find_next()
                                }
                                .is_some();
                            }
                        }
                        Key::Named(NamedKey::Tab) => self.doc_engine.editor.find_toggle_field(),
                        Key::Character(c) if alt && c.as_str().eq_ignore_ascii_case("r") => {
                            self.doc_engine.editor.find_toggle_regex();
                            reveal_find_match = true;
                        }
                        Key::Character(c) if alt && c.as_str().eq_ignore_ascii_case("c") => {
                            self.doc_engine.editor.find_toggle_case();
                            reveal_find_match = true;
                        }
                        _ => {
                            let ctrl_v = cmd
                                && matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("v"));
                            let to_replace = self
                                .doc_engine
                                .editor
                                .find_state()
                                .is_some_and(|f| f.focus == FieldFocus::Replace);
                            let query_before = self
                                .doc_engine
                                .editor
                                .find_state()
                                .map(|find| find.search.text().to_owned());
                            if ctrl_v {
                                // Read the clipboard first, then insert — keeps the
                                // clipboard borrow disjoint from the editor's.
                                let text = self
                                    .clipboard
                                    .as_mut()
                                    .and_then(|cb| cb.get_text().ok())
                                    .filter(|t| !t.is_empty());
                                if let Some(text) = text
                                    && let Some(find) = self.doc_engine.editor.find_state_mut()
                                {
                                    if to_replace {
                                        find.replace.insert(&text);
                                    } else {
                                        find.search.insert(&text);
                                    }
                                }
                            } else if let Some(find) = self.doc_engine.editor.find_state_mut() {
                                let field = if to_replace {
                                    &mut find.replace
                                } else {
                                    &mut find.search
                                };
                                text_input::apply_key(field, &event, self.modifiers);
                            }
                            // A search-field edit changes the match set: rescan, then
                            // bring the (new) active match into view.
                            if !to_replace {
                                self.doc_engine.editor.find_rescan();
                                reveal_find_match =
                                    self.doc_engine.editor.find_state().is_some_and(|find| {
                                        query_before.as_deref() != Some(find.search.text())
                                    });
                            }
                        }
                    }
                    // Search can jump outside the materialized band or into hidden
                    // Markdown. Scrolling the old layout alone draws empty placeholders.
                    if reveal_find_match
                        || self.doc_engine.editor.state.selection != selection_before
                    {
                        apply_edit_effects(
                            &mut self.doc_engine,
                            &mut self.hovered,
                            &self.runtime,
                            &self.proxy,
                            None,
                            &state.window,
                            w,
                            state.scale,
                            vh,
                        );
                    }
                    // Swallow unconditionally: no key reaches the buffer while open.
                    state.window.request_redraw();
                    return;
                }

                // Clipboard: Ctrl/Super + C copy, X cut, V paste.
                if cmd && let Key::Character(c) = &event.logical_key {
                    match c.as_str().to_ascii_lowercase().as_str() {
                        "c" | "x" if self.doc_engine.editor.selected_text().is_some() => {
                            let copied = clipboard_copy_or_cut(
                                &mut self.doc_engine.editor,
                                c.as_str().eq_ignore_ascii_case("x"),
                                |selection| {
                                    let clipboard = self
                                        .clipboard
                                        .as_mut()
                                        .ok_or_else(|| anyhow!("clipboard unavailable"))?;
                                    clipboard.set_text(selection)?;
                                    Ok(())
                                },
                            );
                            if let Err(error) = &copied {
                                eprintln!("[markdone] failed to copy selection: {error}");
                            }
                            if matches!(copied, Ok(true)) {
                                apply_edit_effects(
                                    &mut self.doc_engine,
                                    &mut self.hovered,
                                    &self.runtime,
                                    &self.proxy,
                                    None,
                                    &state.window,
                                    w,
                                    state.scale,
                                    vh,
                                );
                            }
                            return;
                        }
                        "v" => {
                            if let Some(cb) = self.clipboard.as_mut()
                                && let Ok(text) = cb.get_text()
                                && !text.is_empty()
                            {
                                self.doc_engine.editor.paste(&text);
                                apply_edit_effects(
                                    &mut self.doc_engine,
                                    &mut self.hovered,
                                    &self.runtime,
                                    &self.proxy,
                                    Some((&self.ac_gen, &self.ac_slot)),
                                    &state.window,
                                    w,
                                    state.scale,
                                    vh,
                                );
                            }
                            return;
                        }
                        _ => {}
                    }
                }

                // When the autocomplete popup is open, route navigation keys to it.
                // `ac_has` is Some(has_suggestions) iff the popup is open.
                let ac_has = self
                    .doc_engine
                    .editor
                    .autocomplete()
                    .map(|ac| !ac.suggestions.is_empty());
                if let Some(has) = ac_has {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.doc_engine.editor.close_autocomplete();
                            state.window.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::ArrowUp) if has => {
                            self.doc_engine.editor.autocomplete_move(false);
                            state.window.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::ArrowDown) if has => {
                            self.doc_engine.editor.autocomplete_move(true);
                            state.window.request_redraw();
                            return;
                        }
                        // `has` guarantees suggestions exist, so accept() always
                        // succeeds here (it mutates as part of the guard).
                        Key::Named(NamedKey::Enter | NamedKey::Tab)
                            if has && self.doc_engine.editor.accept_autocomplete_suggestion() =>
                        {
                            apply_edit_effects(
                                &mut self.doc_engine,
                                &mut self.hovered,
                                &self.runtime,
                                &self.proxy,
                                None,
                                &state.window,
                                w,
                                state.scale,
                                vh,
                            );
                            return;
                        }
                        _ => {}
                    }
                }

                // Up/Down move by visual row (a wrapped paragraph's rows, not its source
                // line), keeping the column across moves.
                if let Key::Named(key @ (NamedKey::ArrowUp | NamedKey::ArrowDown)) =
                    &event.logical_key
                    && !self.modifiers.control_key()
                    && !self.modifiers.alt_key()
                    && let Some(target) = visual_vertical_target(
                        &self.doc_engine,
                        &mut self.visual_goal,
                        *key == NamedKey::ArrowDown,
                        state.scale,
                    )
                {
                    let extend = self.modifiers.shift_key();
                    self.doc_engine.editor.move_to(target, extend);
                    self.visual_goal = self.visual_goal.map(|(x, _)| (x, target));
                    apply_edit_effects(
                        &mut self.doc_engine,
                        &mut self.hovered,
                        &self.runtime,
                        &self.proxy,
                        None,
                        &state.window,
                        w,
                        state.scale,
                        vh,
                    );
                    return;
                }
                if apply_key(&mut self.doc_engine.editor, self.modifiers, &event) {
                    apply_edit_effects(
                        &mut self.doc_engine,
                        &mut self.hovered,
                        &self.runtime,
                        &self.proxy,
                        Some((&self.ac_gen, &self.ac_slot)),
                        &state.window,
                        w,
                        state.scale,
                        vh,
                    );
                }
            }
            WindowEvent::RedrawRequested => {
                let t_frame = Instant::now();
                // Acquire before building the scene so an occluded window only
                // probes the surface. A skipped first frame must wake itself up.
                let surface_texture = match state.surface.surface.get_current_texture() {
                    CurrentSurfaceTexture::Success(t) | CurrentSurfaceTexture::Suboptimal(t) => t,
                    CurrentSurfaceTexture::Occluded | CurrentSurfaceTexture::Timeout => {
                        self.frame_retry.schedule(Instant::now());
                        return;
                    }
                    other => {
                        eprintln!("[markdone] skip frame: {:?}", other);
                        return;
                    }
                };

                let t_acquired = Instant::now();

                if self
                    .selection_drag
                    .as_mut()
                    .is_some_and(SelectionDrag::take_pending)
                {
                    let (w, vh, scale) = state.viewport();
                    drag_extend_step(&mut self.doc_engine, self.mouse_pos, 0.0, w, scale, vh);
                }

                // Coalesced scroll input: apply this frame's accumulated wheel delta and
                // the latest scrollbar-drag position, with at most one relayout each.
                if let Some(step) = profile::autoscroll_step()
                    && !self.autoscroll_done
                    && let Some(doc) = self.doc_engine.doc.as_ref()
                {
                    let (_, vh, _) = state.viewport();
                    if doc.scroll_y >= doc.max_scroll(vh) - 0.5 {
                        self.autoscroll_done = true;
                        let (held, _) = self.doc_engine.images.trim(doc.image_urls(), usize::MAX);
                        eprintln!(
                            "[markdone-autoscroll] tab {} reached bottom | PSS {} | image pixels held {:.1} MB | {}",
                            self.active_tab,
                            self_pss(),
                            held as f64 / 1048576.0,
                            allocator_stats()
                        );
                        // MARKDONE_AUTOTABS: scroll every tab in turn, then return to the first.
                        if std::env::var_os("MARKDONE_AUTOTABS").is_some() {
                            if self.active_tab + 1 < self.tabs.len() {
                                self.pending_tab_switch = Some(self.active_tab + 1);
                                self.autoscroll_done = false;
                            } else if !self.autotabs_returning {
                                self.autotabs_returning = true;
                                self.pending_tab_switch = Some(0);
                            }
                        }
                    } else {
                        self.wheel_pending_dy += step;
                        state.window.request_redraw();
                    }
                }
                let wheel_dy = std::mem::take(&mut self.wheel_pending_dy);
                if wheel_dy != 0.0 {
                    let (w, vh, scale) = state.viewport();
                    scroll_trace::log(
                        &format!("wheel dy={wheel_dy:.1} before"),
                        self.doc_engine.doc.as_ref(),
                        w,
                        vh,
                        scale,
                    );
                    let remeasure = if let Some(doc) = self.doc_engine.doc.as_mut() {
                        doc.scroll_by(wheel_dy, vh);
                        doc.needs_remeasure(vh)
                    } else {
                        false
                    };
                    scroll_trace::log("wheel scrolled", self.doc_engine.doc.as_ref(), w, vh, scale);
                    // Wheel-scrolled into height-estimated lines → rebuild to lay them out.
                    if remeasure {
                        self.doc_engine.rebuild_preserving_scroll(w, scale, vh);
                        scroll_trace::log(
                            "wheel rebuilt",
                            self.doc_engine.doc.as_ref(),
                            w,
                            vh,
                            scale,
                        );
                        // Image lines scrolled into the materialized range need loading.
                        sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                    }
                    // Visible refs change with scroll_y alone (no rebuild needed); the
                    // cache dedups, so validating once per frame is cheap and safe.
                    spawn_ref_validations(
                        &self.doc_engine.editor,
                        self.doc_engine.doc.as_ref().map(|d| {
                            let (a, b) = d.visible_range(vh);
                            a..b
                        }),
                        &self.runtime,
                        &self.proxy,
                    );
                }
                if std::mem::take(&mut self.scrollbar_pending)
                    && let Some(grab) = self.scrollbar_drag
                    && let Some(geo) = scrollbar_geometry(&self.doc_engine, state)
                    && scrollbar_scroll_to(
                        &mut self.doc_engine,
                        state,
                        &geo,
                        self.mouse_pos.1 - grab,
                    )
                {
                    sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                }
                let t_input = Instant::now();
                self.scene.reset();

                // Keep the native title bar's text current (filename + dirty marker).
                let desired = window_title(&self.doc_engine.editor);
                if desired != self.title {
                    self.title = desired;
                    state.window.set_title(&self.title);
                }

                let width = state.surface.config.width as f32;
                let height = state.doc_height();
                let (content_top, editor_h) = chrome_metrics(state.scale, height, state.find_bar_h);

                // Apply any coalesced async-completion rebuild once, before drawing.
                if self.pending_rebuild {
                    scroll_trace::log(
                        "deferred before",
                        self.doc_engine.doc.as_ref(),
                        width,
                        editor_h,
                        state.scale,
                    );
                    self.doc_engine
                        .rebuild_preserving_scroll(width, state.scale, editor_h);
                    scroll_trace::log(
                        "deferred rebuilt",
                        self.doc_engine.doc.as_ref(),
                        width,
                        editor_h,
                        state.scale,
                    );
                    sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                    self.pending_rebuild = false;
                }

                if let Some(doc) = self.doc_engine.doc.as_ref() {
                    let (btn_rects, fold_rects) = paint_document(
                        &mut self.scene,
                        &mut self.doc_engine.text_engine,
                        &self.doc_engine.theme,
                        &self.doc_engine.editor,
                        doc,
                        content_top,
                        editor_h,
                        state.find_bar_h,
                        width,
                        height,
                        state.scale,
                        self.gutter_hover,
                        self.doc_engine
                            .line_numbers
                            .then_some(self.doc_engine.font_size),
                    );
                    self.find_btn_rects = btn_rects;
                    self.fold_chevron_rects = fold_rects;

                    if let Some(geo) = scrollbar::geometry(
                        doc,
                        (width - outline_width(&self.doc_engine.editor, state.scale)).max(0.0),
                        content_top,
                        editor_h,
                        state.scale,
                    ) {
                        scrollbar::draw(
                            &mut self.scene,
                            &self.doc_engine.theme,
                            &geo,
                            self.scrollbar_hover || self.scrollbar_drag.is_some(),
                            state.scale,
                        );
                    }

                    // Overlays on top of everything (unclipped): the autocomplete
                    // dropdown takes priority over the hover popover.
                    let ac_open = self
                        .doc_engine
                        .editor
                        .autocomplete()
                        .is_some_and(|ac| !ac.suggestions.is_empty());
                    if ac_open {
                        if let Some(caret) = doc.caret_rect(
                            self.doc_engine.editor.cursor_position(),
                            CARET_WIDTH * state.scale,
                        ) {
                            let ac = self.doc_engine.editor.autocomplete().expect("open");
                            self.ac_row_rects = draw_autocomplete(
                                &mut self.doc_engine.text_engine,
                                &mut self.scene,
                                &self.doc_engine.theme,
                                ac,
                                caret,
                                width,
                                height,
                                state.scale,
                            );
                        }
                    } else {
                        self.ac_row_rects.clear();
                        if let Some(target) = self.hovered.as_ref() {
                            draw_hover_popover(
                                &mut self.doc_engine.text_engine,
                                &mut self.scene,
                                &self.doc_engine.theme,
                                &self.doc_engine.editor,
                                target,
                                width,
                                height,
                                state.scale,
                            );
                        }
                    }
                    // The find bar itself is drawn inside `paint_document` as a bottom
                    // chrome strip, so the document reflows above it.
                }

                // egui chrome frame (menu bar). Picked commands run after present.
                let menu_state = MenuState {
                    outline_open: self.doc_engine.editor.outline_open(),
                    line_numbers: self.prefs.line_numbers,
                    git_diff: self.prefs.git_diff,
                    has_file: self.doc_engine.editor.file_path().is_some(),
                    has_selection: self.doc_engine.editor.selection_range().is_some(),
                    tab_count: self.tabs.len(),
                    recent: self.session.recent.iter().map(|p| recent_label(p)).collect(),
                };
                let tab_infos: Vec<TabInfo> = self
                    .tabs
                    .iter()
                    .map(|t| {
                        let editor = t
                            .state
                            .as_ref()
                            .map_or(&self.doc_engine.editor, |s| &s.editor);
                        let path = editor.file_path().map(Path::to_path_buf);
                        TabInfo {
                            name: doc_name(editor),
                            path,
                            dirty: editor.is_dirty(),
                        }
                    })
                    .collect();
                let active_tab = self.active_tab;
                let mut picked = None;
                let mut answer = None;
                let mut conflict_answer = None;
                let conflict = self.tabs[active_tab].conflict.as_ref().map(|_| {
                    let editor = &self.doc_engine.editor;
                    (doc_name(editor), editor.is_dirty())
                });
                let modal_shown = dialog_view.is_some() || self.prefs_open || conflict.is_some();
                let top_points = UiLayer::top_inset(state.scale) / state.scale;
                // Table of contents (egui side panel over the strip the layout reserves).
                let toc_w_dev = outline_width(&self.doc_engine.editor, state.scale);
                let toc = (toc_w_dev > 0.0).then(|| {
                    let buffer = &self.doc_engine.editor.state.buffer;
                    let headings = buffer.headings();
                    let cursor_line = buffer.byte_to_line(self.doc_engine.editor.cursor_position());
                    let current = current_heading_index(headings, cursor_line);
                    let entries: Vec<TocEntry> = headings
                        .iter()
                        .map(|h| TocEntry {
                            level: h.level,
                            text: h.text.clone(),
                            folded: self.doc_engine.editor.is_heading_folded(h.byte_offset),
                        })
                        .collect();
                    let s = state.scale;
                    let rect = egui::Rect::from_min_max(
                        egui::pos2((width - toc_w_dev) / s, top_points + content_top / s),
                        egui::pos2(width / s, top_points + (content_top + editor_h) / s),
                    );
                    (entries, current, rect)
                });
                let toc_reveal = toc.as_ref().is_some_and(|t| t.1 != self.toc_current);
                self.toc_current = toc.as_ref().and_then(|t| t.1);
                let mut toc_response = None;
                if let Some(ui) = self.ui.as_mut() {
                    ui.set_side_panel(toc.as_ref().map(|t| {
                        let mut r = t.2;
                        r.min.x -= 1.0;
                        r
                    }));
                }
                let context_menu = self.context_menu;
                let context_menu_open = std::mem::take(&mut self.context_menu_open);
                let has_selection = menu_state.has_selection;
                let exporting = self.export_running.clone();
                let status_points = STATUS_BAR_H;
                let mut prefs_edit = self.prefs_open.then(|| self.prefs.clone());
                let mut prefs_changed = false;
                let mut prefs_open = self.prefs_open;
                let theme_names = &self.theme_names;
                let config_path = self.config_path.as_deref();
                let ui_frame = self.ui.as_mut().map(|ui| {
                    ui.run(&state.window, modal_shown, |ui| {
                        let menu = menu_bar(ui, &menu_state);
                        let tab = tab_strip(ui, &tab_infos, active_tab);
                        picked = menu.or(tab);
                        if let Some(name) = &exporting {
                            export_indicator(ui.ctx(), name, status_points);
                        }
                        if let Some(pos) = context_menu
                            && let Some(cmd) = crate::ui::menu::context_menu(
                                ui.ctx(),
                                pos,
                                context_menu_open,
                                has_selection,
                            )
                        {
                            picked = Some(cmd);
                        }
                        if let Some((entries, current, rect)) = &toc {
                            toc_response =
                                Some(toc::toc_panel(ui, *rect, entries, *current, toc_reveal));
                        }
                        if let Some(prefs) = prefs_edit.as_mut() {
                            prefs_changed = preferences::show(
                                ui.ctx(),
                                &mut prefs_open,
                                prefs,
                                theme_names,
                                config_path,
                            );
                        }
                        if let Some((name, unsaved)) = &conflict {
                            let mut rect = ui.ctx().content_rect();
                            rect.min.y += top_points;
                            conflict_answer = dialogs::disk_conflict(ui.ctx(), rect, name, *unsaved);
                        }
                        if let Some(view) = &dialog_view {
                            answer = dialogs::show(ui.ctx(), view);
                        }
                    })
                });

                // The document scene sits below the menu bar.
                self.frame_scene.reset();
                self.frame_scene.append(
                    &self.scene,
                    Some(Affine::translate((0.0, state.top_inset() as f64))),
                );

                let dev = &self.context.devices[state.surface.dev_id];
                let params = RenderParams {
                    base_color: peniko_color(self.doc_engine.theme.background),
                    width: state.surface.config.width,
                    height: state.surface.config.height,
                    antialiasing_method: AaConfig::Area,
                };

                // Vello has no render_to_surface: render into the intermediate
                // STORAGE texture, then blit that into the swapchain frame.
                self.renderer
                    .as_mut()
                    .expect("renderer missing")
                    .render_to_texture(
                        &dev.device,
                        &dev.queue,
                        &self.frame_scene,
                        &state.surface.target_view,
                        &params,
                    )
                    .expect("render_to_texture failed");
                let t_rendered = Instant::now();

                let mut encoder =
                    dev.device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("surface_blit"),
                        });
                let surface_view = surface_texture
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                state.surface.blitter.copy(
                    &dev.device,
                    &mut encoder,
                    &state.surface.target_view,
                    &surface_view,
                );
                if let (Some(ui), Some(frame)) = (self.ui.as_mut(), ui_frame) {
                    ui.paint(
                        &dev.device,
                        &dev.queue,
                        &mut encoder,
                        &surface_view,
                        [state.surface.config.width, state.surface.config.height],
                        frame,
                    );
                }
                dev.queue.submit([encoder.finish()]);
                state.window.pre_present_notify();
                surface_texture.present();
                self.frame_retry = FrameRetry::default();
                self.prefs_open = prefs_open;
                if let Some(r) = toc_response {
                    if let Some(i) = r.clicked {
                        picked = Some(Command::GoToHeading(i));
                    }
                    if (r.width - toc_width()).abs() >= 1.0 {
                        set_toc_width(r.width);
                        let (w, vh, scale) = state.viewport();
                        self.doc_engine.rebuild_preserving_scroll(w, scale, vh);
                        state.window.request_redraw();
                    }
                    if !r.resizing {
                        self.toc_width = toc_width();
                    }
                }
                if prefs_changed && let Some(prefs) = prefs_edit {
                    self.apply_preferences(prefs);
                }
                if let Some(answer) = conflict_answer {
                    self.answer_conflict(answer);
                }
                if let Some(answer) = answer {
                    self.answer_dialog(answer, event_loop);
                } else if let Some(cmd) = picked {
                    self.run_command(cmd, event_loop);
                }
                if profile::enabled() {
                    let t_end = Instant::now();
                    let (n, us) = profile::take_rebuilds();
                    let ms = |a: Instant, b: Instant| b.duration_since(a).as_secs_f64() * 1e3;
                    if let Some(lat) = profile::take_input_latency() {
                        eprintln!("[markdone-latency] input → present {lat:7.2} ms");
                    }
                    eprintln!(
                        "[markdone-profile] acquire {:6.2} ms | rebuilds {n} = {:6.2} ms | input {:6.2} ms | scene+gpu {:6.2} ms | present {:6.2} ms | frame-tot {:6.2} ms",
                        ms(t_frame, t_acquired),
                        us as f64 / 1e3,
                        ms(t_acquired, t_input),
                        ms(t_input, t_rendered),
                        ms(t_rendered, t_end),
                        ms(t_frame, t_end),
                    );
                }
            }
            _ => {}
        }
    }

    /// Park the loop until the next real event. External file edits arrive as
    /// `WritEvent::FileChanged` (forwarded from the watcher thread), so there's no
    /// need to poll except to retry a skipped frame or auto-scroll a selection while
    /// the pointer is held still in an edge hot zone.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Before winit disconnects (and frees) the display the listener reads.
        #[cfg(all(unix, not(target_os = "macos")))]
        if let Some(mut listener) = self.dnd.take() {
            listener.stop();
        }
        self.persist_session();
        if self.prefs_save_at.take().is_some() {
            self.save_preferences();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.persist_session();
        if self.prefs_save_at.is_some_and(|t| t <= Instant::now()) {
            self.prefs_save_at = None;
            self.save_preferences();
        }
        // MARKDONE_DROPTEST: attribution baseline — once the first frame is up, report memory
        // with all documents loaded, then drop the parked ones and report again.
        if std::env::var_os("MARKDONE_DROPTEST").is_some()
            && !self.autotabs_returning
            && self.doc_engine.doc.is_some()
            && self.tabs.len() > 1
        {
            self.autotabs_returning = true;
            eprintln!(
                "[markdone-droptest] all docs loaded | PSS {} | {}",
                self_pss(),
                allocator_stats()
            );
            for i in (1..self.tabs.len()).rev() {
                let before = malloc_bytes_in_use();
                drop(self.tabs[i].state.take());
                eprintln!(
                    "[markdone-droptest] dropped parked tab {i}: freed {:.1} MB",
                    (before as f64 - malloc_bytes_in_use() as f64) / 1048576.0
                );
            }
            eprintln!(
                "[markdone-droptest] only tab 0 left | PSS {} | {}",
                self_pss(),
                allocator_stats()
            );
        }
        if let Some(i) = self.pending_tab_switch.take() {
            self.switch_tab(i);
            if self.autotabs_returning {
                let parked: Vec<String> = self
                    .tabs
                    .iter()
                    .enumerate()
                    .filter_map(|(i, t)| {
                        let (files, renders) = t.state.as_ref()?.images.memory_stats();
                        Some(format!(
                            "{i}:{:.1}+{:.1}",
                            files as f64 / 1048576.0,
                            renders as f64 / 1048576.0
                        ))
                    })
                    .collect();
                eprintln!(
                    "[markdone-autotabs] back on tab 0 | PSS {} | {} | parked tabs image MB (files+renders) {}",
                    self_pss(),
                    allocator_stats(),
                    parked.join(" ")
                );
                let rounds: u32 = std::env::var("MARKDONE_AUTOTABS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1);
                if self.autotabs_round < rounds {
                    // Next round: every tab from the top again.
                    self.autotabs_round += 1;
                    self.autotabs_returning = false;
                    self.autoscroll_done = false;
                    for tab in &mut self.tabs {
                        tab.anchor = (0, 0.0);
                    }
                    if let Some(doc) = self.doc_engine.doc.as_mut() {
                        doc.scroll_y = 0.0;
                    }
                    self.pending_rebuild = true;
                    if let Some(state) = self.state.as_ref() {
                        state.window.request_redraw();
                    }
                    eprintln!("[markdone-autotabs] starting round {}", self.autotabs_round);
                    return;
                }
                // Attribution: drop each parked document in turn and report what it freed.
                // Whatever survives dropping them all is held by shared state.
                for i in (1..self.tabs.len()).rev() {
                    let before = malloc_bytes_in_use();
                    drop(self.tabs[i].state.take());
                    let after = malloc_bytes_in_use();
                    eprintln!(
                        "[markdone-autotabs] dropped parked tab {i}: freed {:.1} MB",
                        (before as f64 - after as f64) / 1048576.0
                    );
                }
                eprintln!(
                    "[markdone-autotabs] only tab 0 left | PSS {} | {}",
                    self_pss(),
                    allocator_stats()
                );
            }
        }
        let now = Instant::now();
        let ui_due = self.ui.as_mut().is_some_and(|ui| ui.take_due(now));
        if (self.frame_retry.take_due(now) || ui_due)
            && let Some(state) = self.state.as_ref()
        {
            state.window.request_redraw();
        }
        let drag_deadline = if self.selection_drag.as_ref().is_some_and(|drag| drag.active)
            && self.drag_scroll_dy != 0.0
        {
            // Integrate velocity against real elapsed time: a first tick (no prior
            // timestamp) scrolls nothing and just starts the clock, so the amount can't
            // spike regardless of how the loop woke.
            let dt = self
                .last_drag_tick
                .map_or(0.0, |t| now.duration_since(t).as_secs_f32());
            self.last_drag_tick = Some(now);
            let dy = self.drag_scroll_dy * dt;
            if let Some((w, editor_h, scale, window)) = self.state.as_ref().map(|s| {
                let (w, editor_h, scale) = s.viewport();
                (w, editor_h, scale, s.window.clone())
            }) {
                drag_extend_step(&mut self.doc_engine, self.mouse_pos, dy, w, scale, editor_h);
                window.request_redraw();
            }
            Some(now + Duration::from_millis(16))
        } else {
            self.last_drag_tick = None;
            None
        };
        let ui_deadline = self.ui.as_ref().and_then(UiLayer::repaint_deadline);
        let other = [drag_deadline, ui_deadline, self.prefs_save_at]
            .into_iter()
            .flatten()
            .min();
        event_loop.set_control_flow(self.frame_retry.control_flow(other));
    }
}

/// Boot the shell: install the TLS provider, stand up a tokio runtime for HTTP/
/// GitHub work (kept alive for the process), and run the winit event loop.
pub fn run() -> Result<()> {
    configure_allocator();
    // Default winit's Wayland client-side decoration (sctk-adwaita) to a dark title
    // bar to match the editor. Set before any threads spawn; user can override.
    if std::env::var_os("WINIT_WAYLAND_CSD_THEME").is_none() {
        // SAFETY: called at process start, before the tokio runtime spawns any threads.
        unsafe { std::env::set_var("WINIT_WAYLAND_CSD_THEME", "dark") };
    }
    let _ = rustls::crypto::ring::default_provider().install_default();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = runtime.enter();

    // Headless golden-image path: render one frame to a PNG and exit. Used to
    // verify the render offscreen (no compositor screenshot needed).
    if let Ok(path) = std::env::var("MARKDONE_SHELL_SNAPSHOT") {
        let env_f32 = |k: &str, d: f32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let w = env_f32("MARKDONE_SHELL_W", 1000.0) as u32;
        let h = env_f32("MARKDONE_SHELL_H", 400.0) as u32;
        let scroll = env_f32("MARKDONE_SHELL_SCROLL", 0.0);
        return snapshot(&path, w, h, scroll);
    }

    // A second launch hands its files to the running instance and exits here.
    let Some(startup) = startup_from_cli() else {
        return Ok(());
    };
    let event_loop = EventLoop::<WritEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(startup, proxy, runtime.handle().clone());
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// What the application starts with.
struct Startup {
    editors: Vec<Editor>,
    /// Index of the active document.
    active: usize,
    /// Files given on the command line that do not exist (opened as new documents).
    missing: Vec<PathBuf>,
    config: ResolvedConfig,
    session: Session,
    /// Where the session is saved; `None` = not saved.
    session_path: Option<PathBuf>,
    /// The config file the preferences are saved to.
    config_path: Option<PathBuf>,
    github: GithubSetup,
    /// Where later launches hand over their files (single instance).
    instance: Option<interprocess::local_socket::prelude::LocalSocketListener>,
}

/// Build the startup state from the command line. Files given there open one tab each
/// (watched for external edits). Without files the previous session is restored, or an
/// empty document opens. `--demo` opens the showcase document (no session).
fn startup_from_cli() -> Option<Startup> {
    use clap::Parser;

    let config = Config::parse();
    let app_config = config::load(config.config.as_deref());
    let config_path = config.config.clone().or_else(config::default_config_path);
    let github = GithubSetup {
        enabled: app_config.prefs.github,
        repo: config.github_repo.clone(),
        token: config.github_token.clone(),
    };
    // `--demo`: showcase doc with a synthetic HEAD diff (no file needed).
    if config.demo {
        return Some(Startup {
            editors: vec![demo_editor()],
            active: 0,
            missing: Vec::new(),
            config: app_config,
            session: Session::default(),
            session_path: None,
            config_path,
            github,
            instance: None,
        });
    }
    let session_path = Session::default_path();
    let mut session = session_path.as_deref().map(Session::load).unwrap_or_default();
    let paths: Vec<PathBuf> = config
        .file
        .iter()
        .chain(&config.files)
        .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone()))
        .collect();
    // Single instance: hand the files to a running Markdone, before loading anything.
    let instance = if config.new_instance {
        None
    } else {
        match single_instance::claim(&paths) {
            Instance::Forwarded => return None,
            Instance::Primary(listener) => Some(listener),
            Instance::Standalone => None,
        }
    };
    if paths.is_empty() {
        let (restore, active) = session.restorable();
        let mut editors: Vec<Editor> = restore
            .iter()
            .map(|p| open_document(p, &github, config.autosave))
            .collect();
        if editors.is_empty() {
            editors.push(Editor::new(""));
        }
        return Some(Startup {
            editors,
            active,
            missing: Vec::new(),
            config: app_config,
            session,
            session_path,
            config_path,
            github,
            instance,
        });
    }
    let missing = paths.iter().filter(|p| !p.exists()).cloned().collect();
    for path in paths.iter().filter(|p| p.exists()) {
        session.touch_recent(path);
    }
    let editors = paths
        .iter()
        .map(|p| open_document(p, &github, config.autosave))
        .collect();
    Some(Startup {
        editors,
        active: 0,
        missing,
        config: app_config,
        session,
        session_path,
        config_path,
        github,
        instance,
    })
}

/// Label of a recent file in the menu: the file name, and its folder shortened (home as
/// `~`, long paths cut to their last two folders).
fn recent_label(path: &Path) -> (String, String) {
    let name = path
        .file_name()
        .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let dir = path.parent().unwrap_or(Path::new(""));
    let home = dirs::home_dir();
    let dir_text = match home.as_deref().and_then(|h| dir.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    };
    if dir_text.chars().count() <= 40 {
        return (name, dir_text);
    }
    let tail: Vec<_> = dir.components().rev().take(2).collect();
    let tail: PathBuf = tail.into_iter().rev().collect();
    (name, format!("…/{}", tail.display()))
}

/// The startup notice for files given on the command line that do not exist.
fn missing_files_notice(missing: &[PathBuf]) -> Option<PendingModal> {
    if missing.is_empty() {
        return None;
    }
    let list: Vec<String> = missing.iter().map(|p| format!("•  {}", p.display())).collect();
    let (title, intro) = if missing.len() == 1 {
        ("File not found", "This file does not exist")
    } else {
        ("Files not found", "These files do not exist")
    };
    Some(PendingModal::Message {
        title: title.to_string(),
        text: format!(
            "{intro}; it opens as an empty document and is created when you save:\n\n{}",
            list.join("\n")
        ),
    })
}

/// GitHub lookups for documents opened during this run (Preferences → Privacy).
#[derive(Clone, Default)]
struct GithubSetup {
    enabled: bool,
    /// `--github-repo`: the repository, instead of detecting it from `.git/config`.
    repo: Option<String>,
    /// `--github-token` / `GITHUB_TOKEN`.
    token: Option<String>,
}

/// Open a document: load it, set up GitHub lookups if enabled, detect links and watch
/// the file for external edits.
fn open_document(path: &Path, github: &GithubSetup, autosave: bool) -> Editor {
    let mut editor = Editor::open(path);
    editor.set_autosave(autosave);
    if github.enabled {
        // GitHub context for ref detection: explicit --github-repo, else auto-detect.
        let context = github
            .repo
            .as_deref()
            .and_then(parse_github_repo_string)
            .or_else(|| detect_github_context(path));
        if let Some(ctx) = context {
            editor.set_github_context(ctx);
        }
        if let Some(token) = github.token.clone() {
            editor.set_github_client(GitHubClient::new(token));
        }
    }
    // Populate github_refs_by_line / naked_urls_by_line so the first frame can color
    // (and the shell can spawn validation for) any refs already in the file.
    editor.refresh_detection(0..usize::MAX);
    // A missing file is reported by a dialog; watching fails when its folder is missing too.
    if let Err(e) = editor.watch_file()
        && path.exists()
    {
        eprintln!("[markdone] file watch failed for {}: {e}", path.display());
    }
    editor
}

/// Synchronously validate every detected GitHub ref (blocking on the current tokio
/// runtime). Snapshot-only helper so a single headless frame shows final ref colors.
fn validate_all_blocking(editor: &mut Editor) {
    let Some(client) = editor.github_client().cloned() else {
        return;
    };
    let cache = editor.github_validation_cache().clone();
    let refs = editor.detected_refs();
    tokio::runtime::Handle::current().block_on(async {
        for r in refs {
            validate_ref_into_cache(&client, &cache, r).await;
        }
    });
}

/// Synchronously fetch + install autocomplete suggestions for the open popup.
/// Snapshot-only helper (the GUI fetches asynchronously with a debounce).
fn fetch_autocomplete_blocking(editor: &mut Editor) {
    let Some((trigger, prefix)) = editor.begin_autocomplete_fetch() else {
        return;
    };
    let Some(client) = editor.github_client().cloned() else {
        return;
    };
    let Some(context) = editor.github_context().cloned() else {
        return;
    };
    let fetched = tokio::runtime::Handle::current()
        .block_on(fetch_suggestions(&client, &context, trigger, prefix));
    apply_fetched(editor, fetched);
}

/// Render a single frame of the document to an offscreen texture and write it to
/// `path` as a PNG. Independent of any surface/window, so it runs headlessly and
/// doubles as a golden-image harness for later phases.
pub fn snapshot(path: &str, width: u32, height: u32, scroll_y: f32) -> Result<()> {
    // MARKDONE_SHELL_FILE opens a real file (with live HEAD diff); else the demo doc.
    let mut editor = match std::env::var("MARKDONE_SHELL_FILE") {
        Ok(p) => Editor::open(std::path::Path::new(&p)),
        Err(_) => Editor::new(DEMO_DOC),
    };
    // Place the caret mid-document so the snapshot exercises caret geometry.
    let env_usize = |k: &str| std::env::var(k).ok().and_then(|v| v.parse().ok());
    editor.set_cursor(env_usize("MARKDONE_SHELL_CURSOR").unwrap_or(0));
    // Optional selection A..B (via click+drag) to exercise selection rendering.
    if let (Some(a), Some(b)) = (env_usize("MARKDONE_SHELL_SEL_A"), env_usize("MARKDONE_SHELL_SEL_B")) {
        editor.click(a, false, 1);
        editor.drag(b);
    }
    // Optional diff: set a HEAD base that differs from the current doc so additions
    // (green) + a word-level change render. Exercises the inline-diff path.
    if std::env::var("MARKDONE_SHELL_DIFF").is_ok() {
        editor.set_head_base(DEMO_BASE);
    }
    // MARKDONE_SHELL_OUTLINE reserves the right outline strip so the golden frame shows the
    // reflowed document beside the placeholder panel.
    if std::env::var("MARKDONE_SHELL_OUTLINE").is_ok() {
        editor.set_outline_open(true);
    }
    // Optional GitHub golden-image check: wire a client from GITHUB_TOKEN +
    // MARKDONE_SHELL_GITHUB_REPO, then *synchronously* validate every ref so the single
    // frame shows final ref colors (the GUI does this asynchronously).
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && std::env::var("MARKDONE_SHELL_GITHUB").is_ok()
    {
        if let Some(ctx) = std::env::var("MARKDONE_SHELL_GITHUB_REPO")
            .ok()
            .and_then(|s| parse_github_repo_string(&s))
        {
            editor.set_github_context(ctx);
        }
        editor.set_github_client(GitHubClient::new(token));
        editor.refresh_detection(0..usize::MAX);
        validate_all_blocking(&mut editor);
        // MARKDONE_SHELL_AUTOCOMPLETE opens the popup at the caret (MARKDONE_SHELL_CURSOR).
        if std::env::var("MARKDONE_SHELL_AUTOCOMPLETE").is_ok() {
            editor.update_autocomplete_from_cursor();
            if editor.autocomplete().is_some() {
                fetch_autocomplete_blocking(&mut editor);
            }
        }
    }
    // Headless snapshots never open the find bar, so no find-bar inset.
    let (content_top, editor_h) = chrome_metrics(1.0, height as f32, 0.0);
    // Reuse the shared rebuild path (caches + Parley/Vello engine) headlessly.
    // Snapshots stay on the built-in defaults (no user config) so golden frames are
    // stable across machines.
    let mut de = DocEngine {
        text_engine: TextEngine::new(),
        line_cache: LineCache::new(),
        render_cache: RenderCache::new(),
        height_cache: HeightCache::new(),
        table_cache: TableCache::new(),
        theme: EditorTheme::dracula(),
        font_size: FONT_SIZE,
        max_content_width: Some(MAX_CONTENT_WIDTH),
        line_numbers: false,
        git_diff: true,
        editor,
        doc: None,
        images: ImageCache::new(),
        preedit: None,
    };
    // Run viewport detection (naked URLs, GitHub refs, inline `$…$` math) over the whole
    // document so the golden frame matches the live render, which detects before layout.
    de.editor.refresh_detection(0..usize::MAX);
    // Headless: lay out the whole document (anchor 0, infinite viewport) so any scroll
    // position renders correctly for the golden frame.
    let mut doc = de.rebuild(width as f32, 1.0, 0, f32::INFINITY);
    // Synchronously decode local standalone images so they appear in the single frame
    // (remote images stay a placeholder headlessly). Then rebuild so their heights land.
    let img_dir = de
        .editor
        .file_path()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    load_local_images_blocking(img_dir.as_deref(), doc.image_urls(), &de.images);
    #[cfg(feature = "mermaid")]
    mermaid::render_mermaid_blocking(doc.mermaid_sources(), &de.images);
    #[cfg(feature = "math")]
    math::render_math_blocking(doc.math_sources(), &de.images);
    doc = de.rebuild(width as f32, 1.0, 0, f32::INFINITY);
    doc.scroll_by(scroll_y, editor_h);
    let mut scene = Scene::new();
    paint_document(
        &mut scene,
        &mut de.text_engine,
        &de.theme,
        &de.editor,
        &doc,
        content_top,
        editor_h,
        0.0,
        width as f32,
        height as f32,
        1.0,
        None,
        None,
    );

    // Optional hover-popover golden image: MARKDONE_SHELL_HOVER=<byte offset> anchors the
    // popover on the ref containing that offset.
    if let Some(off) = std::env::var("MARKDONE_SHELL_HOVER")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        && let Some(t) = hover_target_at_offset(&de.editor, &doc, off)
    {
        draw_hover_popover(
            &mut de.text_engine,
            &mut scene,
            &de.theme,
            &de.editor,
            &t,
            width as f32,
            height as f32,
            1.0,
        );
    }

    // Optional autocomplete golden image (see MARKDONE_SHELL_AUTOCOMPLETE above).
    if let Some(ac) = de
        .editor
        .autocomplete()
        .filter(|ac| !ac.suggestions.is_empty())
        && let Some(caret) = doc.caret_rect(de.editor.cursor_position(), 2.0)
    {
        draw_autocomplete(
            &mut de.text_engine,
            &mut scene,
            &de.theme,
            ac,
            caret,
            width as f32,
            height as f32,
            1.0,
        );
    }

    rasterize_scene_to_png(
        &scene,
        width,
        height,
        peniko_color(de.theme.background),
        path,
    )?;
    eprintln!("[markdone] wrote snapshot: {path} ({width}x{height})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_cut_failure_preserves_document_selection_and_undo() {
        let mut editor = Editor::new("hello world");
        editor.insert_str("prefix ");
        editor.click(8, false, 2);
        let text = editor.text();
        let selection = editor.state.selection;
        let version = editor.state.buffer.version();
        let undo_head = editor.state.buffer.undo_head();
        let mut writes = 0;

        let result = clipboard_copy_or_cut(&mut editor, true, |text| {
            writes += 1;
            assert_eq!(text, "hello");
            Err(anyhow!("clipboard unavailable"))
        });

        assert!(result.is_err());
        assert_eq!(writes, 1);
        assert_eq!(editor.text(), text);
        assert_eq!(editor.state.selection, selection);
        assert_eq!(editor.state.buffer.version(), version);
        assert_eq!(editor.state.buffer.undo_head(), undo_head);
        editor.undo();
        assert_eq!(editor.text(), "hello world");
        assert!(!editor.can_undo());
    }

    #[test]
    fn clipboard_cut_success_copies_and_deletes_once() {
        let mut editor = Editor::new("hello world");
        editor.click(2, false, 2);
        let mut writes = 0;
        let changed = clipboard_copy_or_cut(&mut editor, true, |text| {
            writes += 1;
            assert_eq!(text, "hello");
            Ok(())
        })
        .unwrap();

        assert!(changed);
        assert_eq!(writes, 1);
        assert_eq!(editor.text(), " world");
        assert_eq!(editor.cursor_position(), 0);
        assert!(editor.selection_range().is_none());
        editor.undo();
        assert_eq!(editor.text(), "hello world");
        assert!(!editor.can_undo());
    }

    #[test]
    fn clipboard_copy_preserves_selection_and_document() {
        let mut editor = Editor::new("hello world");
        editor.click(2, false, 2);
        let selection = editor.state.selection;
        let changed = clipboard_copy_or_cut(&mut editor, false, |text| {
            assert_eq!(text, "hello");
            Ok(())
        })
        .unwrap();

        assert!(!changed);
        assert_eq!(editor.text(), "hello world");
        assert_eq!(editor.state.selection, selection);
        assert!(!editor.can_undo());
    }

    fn test_doc_engine(text: &str) -> DocEngine {
        DocEngine {
            text_engine: TextEngine::new(),
            line_cache: LineCache::new(),
            render_cache: RenderCache::new(),
            height_cache: HeightCache::new(),
            table_cache: TableCache::new(),
            theme: EditorTheme::dracula(),
            font_size: FONT_SIZE,
            max_content_width: Some(MAX_CONTENT_WIDTH),
            line_numbers: false,
            git_diff: true,
            editor: Editor::new(text),
            doc: None,
            images: ImageCache::new(),
            preedit: None,
        }
    }

    #[test]
    fn git_base_reload_preserves_unsaved_text_selection_and_undo() {
        let mut editor = Editor::new("original\n");
        editor.insert_str("local ");
        let text = editor.text();
        let selection = editor.state.selection;
        let version = editor.state.buffer.version();
        let mut reloads = ReloadState::default();
        let generation = reloads.begin_base();
        assert!(reloads.apply_base(&mut editor, generation, Some("committed\n".into())));
        assert_eq!(editor.text(), text);
        assert_eq!(editor.state.selection, selection);
        assert_eq!(editor.state.buffer.version(), version);
        assert!(editor.is_dirty());
        assert!(editor.can_undo());
        assert!(editor.diff_state().is_some());
        let generation = reloads.begin_base();
        assert!(reloads.apply_base(&mut editor, generation, None));
        assert!(editor.diff_state().is_none());
        assert_eq!(editor.text(), text);
        editor.undo();
        assert_eq!(editor.text(), "original\n");
    }

    #[test]
    fn stale_file_reads_cannot_overwrite_newer_reads_or_local_edits() {
        let mut editor = Editor::new("original");
        let mut reloads = ReloadState::default();
        let older = reloads.begin_file(&editor);
        let newer = reloads.begin_file(&editor);
        assert!(!reloads.apply_file(&mut editor, older, ("stale".into(), None)));
        assert_eq!(editor.text(), "original");
        assert!(reloads.apply_file(&mut editor, newer, ("newer".into(), None)));
        assert!(!reloads.apply_file(&mut editor, older, ("stale".into(), None)));
        assert_eq!(editor.text(), "newer");

        let pending = reloads.begin_file(&editor);
        editor.insert_str("local ");
        assert!(!reloads.apply_file(&mut editor, pending, ("newer".into(), None)));
        assert_eq!(editor.text(), "local newer");
        assert!(editor.is_dirty());
        editor.undo();
        assert_eq!(editor.text(), "newer");
    }

    #[test]
    fn file_read_cannot_overwrite_an_intervening_save() {
        let path = std::env::temp_dir().join(format!("writ-reload-save-{}.md", std::process::id()));
        let mut editor = Editor::new("original");
        editor.set_file_path(path.clone());
        editor.save().unwrap();
        editor.insert_str("local ");
        let mut reloads = ReloadState::default();
        let pending = reloads.begin_file(&editor);
        let version = editor.state.buffer.version();
        editor.save().unwrap();
        assert_eq!(editor.state.buffer.version(), version);
        assert!(!reloads.apply_file(&mut editor, pending, ("original".into(), None)));
        assert_eq!(editor.text(), "local original");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), editor.text());
        assert!(!editor.is_dirty());
        assert!(editor.can_undo());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn file_reload_cannot_restore_an_outdated_git_base() {
        let mut editor = Editor::new("old");
        let mut reloads = ReloadState::default();
        let file = reloads.begin_file(&editor);
        let old_base = reloads.begin_base();
        let new_base = reloads.begin_base();
        assert!(reloads.apply_base(&mut editor, new_base, Some("new".into())));
        assert!(!reloads.apply_base(&mut editor, old_base, Some("old".into())));
        assert!(reloads.apply_file(&mut editor, file, ("new".into(), Some("old".into()))));
        assert_eq!(editor.text(), "new");
        assert!(editor.diff_state().is_none());
    }

    #[test]
    fn find_jumps_materialize_distant_matches() {
        let padding = "ordinary line\n".repeat(500);
        let text = format!("{padding}quartz first\n{padding}quartz second\n");
        let mut engine = test_doc_engine(&text);
        let (w, vh, scale) = (800.0, 500.0, 1.0);
        engine.editor.open_find(false);
        engine.refresh(w, scale, vh);
        engine
            .editor
            .find_state_mut()
            .unwrap()
            .search
            .insert("quartz");
        engine.editor.find_rescan();

        let cursor = engine.editor.cursor_position();
        let doc = engine.doc.as_mut().unwrap();
        doc.scroll_to(cursor, vh);
        assert!(
            doc.needs_remeasure(vh),
            "scroll alone leaves placeholders visible"
        );

        for step in 0..3 {
            match step {
                1 => {
                    engine.editor.find_next();
                }
                2 => {
                    engine.editor.find_prev();
                }
                _ => {}
            }
            engine.refresh(w, scale, vh);
            let doc = engine.doc.as_ref().unwrap();
            assert!(!doc.needs_remeasure(vh));
            let selection = engine.editor.selection_range().unwrap();
            assert!(
                doc.selection_rects(selection)
                    .iter()
                    .any(|rect| rect.3 > 0.0 && rect.1 < f64::from(vh))
            );
        }
        assert_eq!(engine.editor.text(), text);
    }

    #[test]
    fn selection_drag_requires_pointer_travel_at_each_scale() {
        for scale in [1.0, 1.5, 2.0] {
            let origin = (100.0, 200.0);
            let mut drag = SelectionDrag::new(origin, scale);
            for dx in [0.0, 1.0, 2.0, 3.0, 4.0, 0.0] {
                assert!(!drag.update((origin.0 + dx * scale, origin.1)));
                assert!(!drag.active);
                assert!(!drag.take_pending());
            }
            assert!(drag.update((origin.0 + 5.0 * scale, origin.1)));
            assert!(drag.active);
            assert!(drag.take_pending());
            assert!(!drag.take_pending());

            // Once dragging, returning inside the threshold still updates selection.
            assert!(drag.update(origin));
            assert!(drag.take_pending());
            assert!(!drag.update(origin));
            assert!(!drag.take_pending());
        }
    }

    #[test]
    fn table_reveal_does_not_turn_click_jitter_into_selection() {
        let text = "Intro\n\n| Name | Value |\n| --- | --- |\n| Alpha | One |\n| Beta | Two |\n";
        let mut engine = test_doc_engine(text);
        let (w, vh, scale) = (800.0, 600.0, 1.0);
        engine.refresh(w, scale, vh);
        let doc = engine.doc.as_ref().unwrap();
        let position = (
            doc.body_left() + 80.0,
            doc.line_top_screen(4).unwrap() + 2.0,
        );
        let clicked = doc.hit_test(position.0, position.1).unwrap();
        assert_eq!(engine.editor.line_of(clicked), 4);
        engine.editor.click(clicked, false, 1);
        engine.refresh(w, scale, vh);
        assert_ne!(
            engine
                .doc
                .as_ref()
                .unwrap()
                .hit_test(position.0, position.1),
            Some(clicked),
            "revealing raw table text should move a different offset under the pointer"
        );

        let mut drag = SelectionDrag::new(position, scale);
        for dx in [0.0, 1.0, -1.0, 2.0, 0.0] {
            let jitter = (position.0 + dx, position.1);
            if drag.update(jitter) && drag.take_pending() {
                drag_extend_step(&mut engine, jitter, 0.0, w, scale, vh);
            }
        }
        assert_eq!(engine.editor.cursor_position(), clicked);
        assert!(engine.editor.selection_range().is_none());
        assert!(
            !drag.pending,
            "a plain release must not hit-test the changed layout"
        );

        let moved = (position.0 + 60.0, position.1);
        assert!(drag.update(moved));
        assert!(
            drag.pending,
            "a release before redraw must retain the final drag update"
        );
        drag_extend_step(&mut engine, moved, 0.0, w, scale, vh);
        assert!(engine.editor.selection_range().is_some());
        assert_eq!(engine.editor.state.selection.anchor, clicked);
    }

    #[test]
    fn skipped_frames_retry_without_input_and_back_off() {
        let mut retry = FrameRetry::default();
        let mut now = Instant::now();
        assert_eq!(retry.control_flow(None), ControlFlow::Wait);

        for delay_ms in [100, 200, 400, 800, 1000, 1000] {
            retry.schedule(now);
            let deadline = now + Duration::from_millis(delay_ms);
            assert_eq!(retry.control_flow(None), ControlFlow::WaitUntil(deadline));
            assert!(!retry.take_due(deadline - Duration::from_millis(1)));
            assert!(retry.take_due(deadline));
            assert!(!retry.take_due(deadline));
            now = deadline;
        }
    }

    #[test]
    fn frame_retry_preserves_the_earliest_event_loop_deadline() {
        let mut retry = FrameRetry::default();
        let now = Instant::now();
        let drag_deadline = now + Duration::from_millis(16);
        assert_eq!(
            retry.control_flow(Some(drag_deadline)),
            ControlFlow::WaitUntil(drag_deadline)
        );
        retry.schedule(now);
        assert_eq!(
            retry.control_flow(Some(drag_deadline)),
            ControlFlow::WaitUntil(drag_deadline)
        );
        assert_eq!(
            retry.control_flow(Some(now + Duration::from_secs(1))),
            ControlFlow::WaitUntil(now + Duration::from_millis(100))
        );
    }

    /// The line-number margin grows with the number of digits of the last line.
    #[test]
    fn line_number_gutter_fits_the_widest_number() {
        let one = line_number_gutter(18.0, 9);
        let three = line_number_gutter(18.0, 999);
        let four = line_number_gutter(18.0, 1000);
        assert!(one < three && three < four);
        assert_eq!(line_number_gutter(18.0, 0), one);
        // Each digit is about 0.62 em of the numbers' size.
        assert!((four - three - line_number_size(18.0) * 0.62).abs() < 1e-3);
    }

    #[test]
    fn missing_files_notice_lists_each_path() {
        assert!(missing_files_notice(&[]).is_none());
        let one = missing_files_notice(&[PathBuf::from("a/x.md")]);
        let Some(PendingModal::Message { title, text }) = one else {
            panic!("expected a message");
        };
        assert_eq!(title, "File not found");
        assert!(text.contains("a/x.md"));
        let two = missing_files_notice(&[PathBuf::from("x.md"), PathBuf::from("y.md")]);
        let Some(PendingModal::Message { title, text }) = two else {
            panic!("expected a message");
        };
        assert_eq!(title, "Files not found");
        assert!(text.contains("x.md") && text.contains("y.md"));
    }

    #[test]
    fn drag_edge_velocity_zones() {
        let h = 400.0;
        // Middle of the viewport: no auto-scroll.
        assert_eq!(drag_edge_velocity(200.0, h, 1.0), 0.0);
        // Top zone: negative (scroll up), deeper = faster.
        assert!(drag_edge_velocity(5.0, h, 1.0) < drag_edge_velocity(20.0, h, 1.0));
        assert!(drag_edge_velocity(5.0, h, 1.0) < 0.0);
        // Bottom zone: positive (scroll down).
        assert!(drag_edge_velocity(h - 2.0, h, 1.0) > 0.0);
        // At the exact edge the speed eases to EDGE_SPEED (not the cap).
        assert!((drag_edge_velocity(0.0, h, 1.0) + DRAG_SCROLL_EDGE_SPEED).abs() < 1e-3);
        // Past the edge (outside the window) it grows but never exceeds the cap.
        assert!(drag_edge_velocity(-10.0, h, 1.0).abs() > DRAG_SCROLL_EDGE_SPEED);
        assert!(drag_edge_velocity(-100_000.0, h, 1.0).abs() <= DRAG_SCROLL_MAX_SPEED + 1e-3);
        assert!(drag_edge_velocity(h + 100_000.0, h, 1.0) <= DRAG_SCROLL_MAX_SPEED + 1e-3);
        // Scale widens the zone: 30px from the top is inside the zone at 2x, not at 1x.
        assert!(drag_edge_velocity(30.0, h, 2.0) < 0.0);
        assert_eq!(drag_edge_velocity(30.0, h, 1.0), 0.0);
    }
}
