//! Headless typing benchmark: open a file, put the caret mid-document, type N chars and
//! time each keystroke as the app does it (no window): the editor core, the link/math
//! detection over the lines in view, and the document layout rebuild.
use markdone::consts::{FONT_SIZE, LINE_HEIGHT, MAX_CONTENT_WIDTH, PADDING};
use markdone::core::Editor;
use markdone::doc_layout::{
    DocLayout, HeightCache, LayoutParams, LineCache, RenderCache, TableCache,
};
use markdone::editor::EditorTheme;
use markdone::image_cache::ImageCache;
use markdone::text_engine::{TextEngine, peniko_color};
use std::time::Instant;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: typebench <file.md> [n]");
    let n: usize = std::env::args().nth(2).map_or(20, |s| s.parse().unwrap());
    let t = Instant::now();
    let mut ed = Editor::open(std::path::Path::new(&path));
    println!(
        "open: {:.1} ms, {} bytes",
        t.elapsed().as_secs_f64() * 1e3,
        ed.len()
    );
    ed.set_cursor(ed.len() / 2);
    // The app scans the lines in view (plus a margin), not the whole document.
    let mid = ed.line_count() / 2;
    let lines = mid.saturating_sub(60)..mid + 60;

    let mut engine = TextEngine::new();
    let (mut lc, mut rc, mut hc, mut tc) = (
        LineCache::new(),
        RenderCache::new(),
        HeightCache::new(),
        TableCache::new(),
    );
    let images = ImageCache::new();
    let theme = EditorTheme::catppuccin_latte();
    let params = LayoutParams {
        content_x0: 0.0,
        content_w: 1400.0,
        scale: 1.0,
        pad_x: PADDING,
        pad_top: PADDING,
        pad_bottom: PADDING * 2.0,
        base_font_size: FONT_SIZE,
        line_height: LINE_HEIGHT,
        fg: peniko_color(theme.foreground),
        max_content_width: Some(MAX_CONTENT_WIDTH),
    };
    let mut layout = |ed: &mut Editor| {
        let cursor = ed.cursor_position();
        let version = ed.state.buffer.version();
        let snapshot = ed.state.buffer.render_snapshot();
        let anchor = ed.line_of(cursor).saturating_sub(20);
        DocLayout::build(
            &mut engine,
            &mut lc,
            &mut rc,
            &mut hc,
            &mut tc,
            version,
            &snapshot,
            &theme,
            None,
            None,
            &images,
            cursor,
            &params,
            None,
            anchor,
            900.0,
            &[],
            &(cursor..cursor),
            ed.math_spans_by_line(),
        )
    };
    layout(&mut ed);

    for i in 0..n {
        let c = (b'a' + (i % 26) as u8) as char;
        let t = Instant::now();
        ed.type_char(c);
        let t_type = t.elapsed();
        let t = Instant::now();
        ed.refresh_detection(lines.clone());
        let t_det = t.elapsed();
        let t = Instant::now();
        let doc = layout(&mut ed);
        let t_layout = t.elapsed();
        drop(doc);
        println!(
            "key {i:2}: type_char {:7.2} ms | refresh_detection {:7.2} ms | layout {:7.2} ms",
            t_type.as_secs_f64() * 1e3,
            t_det.as_secs_f64() * 1e3,
            t_layout.as_secs_f64() * 1e3,
        );
    }
}
