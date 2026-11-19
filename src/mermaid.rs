//! Mermaid diagram rendering: detect a ```` ```mermaid ```` fence, render its source to
//! SVG with `merman`, and draw that SVG as *vectors*: the SVG is parsed once (usvg, text
//! converted to outlines with the system fonts) and turned into a Vello scene fragment
//! (`vello_svg`), which the standalone-image draw path appends to the frame scaled to its
//! destination. Crisp at any window scale or zoom, and nothing to re-render when either
//! changes. The diagram is cached under a synthetic `mermaid:<hash>` key so it renders once
//! per distinct source, not per frame; rendering runs off the UI thread.
//!
//! Why merman: the reference is Zettlr 4.9, which embeds Mermaid 12.1. merman is a
//! parity-focused Rust port of Mermaid 12 (dagre via `dugong`, coordinate-identical to
//! dagre-d3-es on real layouts; ELK, Mermaid 12's default flowchart layout; the "neo"
//! look; full Gantt axis formatting; `%%{init}%%` directives). The previous engine
//! (`mermaid-rs-renderer`) was an independent rewrite whose layout could not be tuned into
//! Mermaid's. Known caveat: merman measures text with browser-calibrated metrics, while
//! the outlines come from whatever fonts the system resolves Mermaid's font stack to, so
//! a label can be slightly wider than its box when those fonts differ.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, LazyLock};

use merman::{
    MermaidEngine, OperationControl, RenderOutput, RenderRequest, Renderer,
    SourcePresentationConfig, SvgRequest,
};
use vello_svg::usvg;

use crate::image_cache::{ImageCache, ImagePaint, LoadedImage, decode, svg_fonts};

/// A ```` ```mermaid ```` fence in the document. `block` is the whole fenced range (for
/// the reveal-on-cursor test); `content` is the diagram source; `anchor_line` is the
/// fence's first line, which carries the drawn image (other block lines collapse to 0).
pub struct MermaidBlock {
    pub block: Range<usize>,
    pub content: Range<usize>,
    pub anchor_line: usize,
}

/// Synthetic cache key for a diagram, derived from its source only: the vector scene is
/// resolution-independent, so one render serves every width and window scale.
pub fn key_for(source: &str) -> String {
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    format!("mermaid:{:016x}", h.finish())
}

/// Build a renderer whose site-wide defaults are `config` (a Mermaid config object). The
/// author's own `%%{init: …}%%` directives still take precedence over it.
fn renderer_with(config: serde_json::Value) -> Renderer {
    let site = SourcePresentationConfig::from_value(config);
    Renderer::new().with_presentation_engine(MermaidEngine::new().with_site_config(site))
}

/// Screen renderer: Mermaid's `dark` theme, as Zettlr shows diagrams in its dark mode
/// (labels `#ccc`, subgraphs `#474949`). Mermaid paints no backdrop of its own, so the
/// diagram sits directly on the editor background.
static SCREEN: LazyLock<Renderer> =
    LazyLock::new(|| renderer_with(serde_json::json!({ "theme": "dark" })));

/// Print renderer (PDF export): Mermaid's `default` light theme, for a white page. The
/// geometry is the same as on screen; only colors differ.
static PRINT: LazyLock<Renderer> =
    LazyLock::new(|| renderer_with(serde_json::json!({ "theme": "default" })));

/// Why a render produced no SVG: a message to show the author, or a renderer panic
/// (adversarial input — no message, the placeholder stays generic).
enum RenderFail {
    Message(String),
    Panic,
}

impl RenderFail {
    fn into_message(self) -> String {
        match self {
            RenderFail::Message(msg) => msg,
            RenderFail::Panic => "the diagram renderer failed on this input".to_string(),
        }
    }
}

/// Render `source` to SVG with `renderer`, through merman's resvg-safe pipeline: HTML
/// labels (`<foreignObject>`, which usvg ignores) become plain `<text>`.
fn render_svg(renderer: &Renderer, source: &str) -> Result<String, RenderFail> {
    let req = SvgRequest {
        pipeline: Some(merman::svg::SvgPipeline::resvg_safe()),
        ..Default::default()
    };
    let out = catch_unwind(AssertUnwindSafe(|| {
        renderer.render(RenderRequest::svg(source, OperationControl::new(), req))
    }));
    match out {
        Ok(Ok(RenderOutput::Svg(Some(svg)))) => Ok(svg.svg().to_string()),
        Ok(Ok(_)) => Err(RenderFail::Message("no mermaid diagram found".to_string())),
        Ok(Err(e)) => Err(RenderFail::Message(e.to_string())),
        Err(_) => Err(RenderFail::Panic),
    }
}

/// Render a diagram to SVG markup for the screen (dark theme, transparent backdrop).
/// `Err` carries the parse/render error message. Exposed for tests and the dev probe.
pub fn render_svg_for_screen(source: &str) -> Result<String, String> {
    render_svg(&SCREEN, source).map_err(RenderFail::into_message)
}

/// Render a diagram to SVG markup for printing (PDF export): Mermaid's light theme, for
/// a white page. `Err` carries the parse/render error message.
pub fn render_svg_for_print(source: &str) -> Result<String, String> {
    render_svg(&PRINT, source).map_err(RenderFail::into_message)
}

/// Turn SVG markup into a vector [`LoadedImage`]: parse it (text → outlines with the
/// shared system-font database) and record its drawing commands into a Vello scene
/// fragment, in the SVG's logical units. Elements `vello_svg` cannot draw (filters,
/// masks, patterns — none in merman's resvg-safe output today) are skipped instead of
/// being flagged with its default red box. `None` if the SVG doesn't parse or is empty.
pub fn svg_to_vector(svg: &str) -> Option<LoadedImage> {
    let tree = text_as_outlines(svg)?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let mut scene = vello::Scene::new();
    vello_svg::append_tree_with(&mut scene, &tree, &mut |_, _| {});
    Some(LoadedImage {
        paint: ImagePaint::Vector(Arc::new(scene)),
        width: w.round() as u32,
        height: h.round() as u32,
        display_w: w,
        display_h: h,
        baseline: None,
    })
}

/// Parse `svg` into a usvg tree whose text is already converted to outlines (no
/// `Text` nodes left). `vello_svg` 0.11 draws a `Text` node's outlines with the text's
/// absolute transform applied twice (`flattened()` children already carry it), which
/// pushes every label outside its group's clip — so text never reaches it: usvg lays the
/// text out with the system fonts, writes it back as plain paths, and the result is
/// parsed again.
fn text_as_outlines(svg: &str) -> Option<usvg::Tree> {
    let opt = usvg::Options {
        fontdb: svg_fonts(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let outlined = tree.to_string(&usvg::WriteOptions {
        preserve_text: false,
        ..Default::default()
    });
    usvg::Tree::from_str(&outlined, &opt).ok()
}

/// Render mermaid source to a paintable image: source → SVG → vector scene (falling back
/// to a raster decode should the vector conversion ever fail). `Ok(img)` on success;
/// `Err(Some(msg))` on a parse/render error (shown in the placeholder so the author sees
/// what's wrong); `Err(None)` on a panic or an SVG that won't decode at all.
fn render_to_image(source: &str) -> Result<LoadedImage, Option<String>> {
    match render_svg(&SCREEN, source) {
        Ok(svg) => {
            let vector = catch_unwind(AssertUnwindSafe(|| svg_to_vector(&svg))).ok().flatten();
            vector.or_else(|| decode(svg.as_bytes())).ok_or(None)
        }
        Err(RenderFail::Message(msg)) => Err(Some(msg)),
        Err(RenderFail::Panic) => Err(None),
    }
}

/// For each `(key, source)` with no cache entry, mark it loading and render on a worker
/// thread (mermaid layout is CPU-heavy — never on the render thread), then `notify()` so
/// the event-driven shell rebuilds around the now-known diagram size. Mirrors
/// `image_load::spawn_image_loads`, but the "load" is a local CPU render, so a plain OS
/// thread suffices (no async runtime).
pub fn spawn_mermaid_renders(
    sources: &[(String, String)],
    cache: &ImageCache,
    notify: impl Fn() + Send + Clone + 'static,
) {
    for (key, source) in sources {
        // Skips renders already in flight/loaded/failed; redoes evicted ones.
        if !cache.begin_render(key) {
            continue;
        }
        let cache = cache.clone();
        let notify = notify.clone();
        let key = key.clone();
        let source = source.clone();
        std::thread::spawn(move || {
            cache.store_render(&key, render_to_image(&source));
            notify();
        });
    }
}

/// Synchronously render diagrams into the cache for the headless snapshot frame.
pub fn render_mermaid_blocking(sources: &[(String, String)], cache: &ImageCache) {
    for (key, source) in sources {
        if cache.contains(key) {
            continue;
        }
        cache.store_render(key, render_to_image(source));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_cache::ImageState;

    fn render_into_cache(src: &str) -> Option<ImageState> {
        let key = key_for(src);
        let cache = ImageCache::new();
        render_mermaid_blocking(&[(key.clone(), src.to_string())], &cache);
        cache.get(&key)
    }

    #[test]
    fn renders_diagram_into_cache_as_vector() {
        match render_into_cache("flowchart TD\n  A[Start] --> B[End]") {
            Some(ImageState::Loaded(img)) => {
                assert!(
                    matches!(img.paint, ImagePaint::Vector(_)),
                    "diagrams are drawn as vectors, not rasters"
                );
                assert!(img.display_w > 0.0 && img.display_h > 0.0);
                assert_eq!(img.native_size(), (img.display_w, img.display_h));
            }
            _ => panic!("a valid flowchart should render and convert to a vector scene"),
        }
    }

    /// Labels must reach Vello as plain outlines: a `Text` node left in the tree would be
    /// drawn misplaced by `vello_svg` (see `text_as_outlines`) and clipped away.
    #[test]
    fn labels_become_outlines_before_vello() {
        fn has_text(g: &usvg::Group) -> bool {
            g.children().iter().any(|n| match n {
                usvg::Node::Text(_) => true,
                usvg::Node::Group(g) => has_text(g),
                _ => false,
            })
        }
        let svg = render_svg_for_screen("flowchart LR\n  A[Uno] --> B[Due]").unwrap();
        let tree = text_as_outlines(&svg).expect("screen SVG must parse");
        assert!(!has_text(tree.root()), "text must be converted to paths");
        // The outlines are really there: far more path segments than the bare shapes.
        let bare = svg.replace(">Uno<", "><").replace(">Due<", "><");
        let count = |svg: &str| {
            let mut s = vello::Scene::new();
            vello_svg::append_tree(&mut s, &text_as_outlines(svg).unwrap());
            s.encoding().path_data.len()
        };
        assert!(count(&svg) > count(&bare), "labels must add glyph outlines");
    }

    #[test]
    fn bad_source_fails_with_message_without_panicking() {
        match render_into_cache("%%%% definitely not mermaid %%%%") {
            Some(ImageState::Failed(Some(msg))) => assert!(!msg.is_empty()),
            Some(ImageState::Failed(None)) => panic!("expected a message, not a bare failure"),
            Some(ImageState::Loaded(_)) => panic!("garbage source should not render"),
            _ => panic!("the entry must resolve"),
        }
    }

    #[test]
    fn parse_error_surfaces_reason() {
        let src = "flowchart TD\n  A[unclosed --> B";
        match render_into_cache(src) {
            Some(ImageState::Failed(Some(msg))) => assert!(!msg.is_empty()),
            _ => panic!("expected Failed(Some(reason))"),
        }
    }

    #[test]
    fn key_is_stable_per_source() {
        assert_eq!(key_for("graph LR; A-->B"), key_for("graph LR; A-->B"));
        assert_ne!(key_for("graph LR; A-->B"), key_for("graph LR; A-->C"));
    }

    /// The old engine dropped the whole line on an ASCII apostrophe inside a bracketed
    /// label of an edge chain; merman must render the label verbatim.
    #[test]
    fn apostrophe_in_chained_label_renders_verbatim() {
        let src = "flowchart TB\n  C2 --> C4[C4 Parametri dell'esito] --> C5";
        let svg = render_svg_for_screen(src).expect("flowchart should render");
        assert!(svg.contains(">dell'esito<"), "apostrophe label must render as written");
        assert!(!svg.contains("C4[C4"), "raw source text leaked into the diagram");
    }

    /// usvg draws no `<foreignObject>`: the screen SVG must carry its labels as `<text>`.
    #[test]
    fn screen_svg_has_no_foreign_object() {
        let svg = render_svg_for_screen("flowchart LR\n  A[Uno] --> B[Due]").unwrap();
        assert!(!svg.contains("<foreignObject"), "labels must be plain <text>");
        assert!(svg.contains("<text"), "labels must still be present");
    }

    /// No backdrop: the editor background shows through, as in Zettlr.
    #[test]
    fn screen_svg_has_transparent_background() {
        let svg = render_svg_for_screen("flowchart LR\n  A --> B").unwrap();
        assert!(!svg.contains("class=\"background"), "no background rect");
        let root = &svg[..svg.find('>').unwrap()];
        assert!(!root.contains("background"), "root must not paint a background: {root}");
    }

    /// Dark theme on screen (Zettlr's dark mode), light theme on paper.
    #[test]
    fn screen_is_dark_print_is_light() {
        let src = "flowchart LR\n  A --> B";
        let screen = render_svg_for_screen(src).unwrap();
        let print = render_svg_for_print(src).unwrap();
        assert!(screen.contains("#ccc"), "dark theme labels are #ccc");
        assert!(!print.contains("fill:#ccc"), "print must not use the dark label color");
        assert_ne!(screen, print);
    }

    /// Gantt honours `axisFormat` (the old engine ignored it).
    #[test]
    fn gantt_axis_format_is_applied() {
        let src = "gantt\n  dateFormat YYYY-MM-DD\n  axisFormat S%V\n  section S\n  task :a1, 2026-01-05, 30d";
        let svg = render_svg_for_screen(src).expect("gantt should render");
        // 2026-01-05 is a Monday of ISO week 2; a 30-day task spans weeks 2–6.
        assert!(svg.contains(">S02<"), "ISO-week tick labels expected (S%V)");
        assert!(svg.contains(">S04<"));
    }

    /// A `%%{init}%%` directive overrides the site config: the same flowchart laid out with
    /// dagre and with ELK (Mermaid 12's default) must differ.
    #[test]
    fn init_directive_selects_layout() {
        let body = "flowchart TB\n  subgraph X\n    A --> B --> C\n  end\n  subgraph Y\n    D --> E\n  end\n  A --> E\n  B --> D\n";
        let elk = render_svg_for_screen(body).unwrap();
        let dagre =
            render_svg_for_screen(&format!("%%{{init: {{\"layout\": \"dagre\"}}}}%%\n{body}"))
                .unwrap();
        assert_ne!(elk, dagre, "the dagre directive must change the layout");
    }

    #[test]
    fn user_theme_directive_wins_over_site_config() {
        let src = "%%{init: {\"theme\": \"forest\"}}%%\nflowchart LR\n  A --> B";
        let forest = render_svg_for_screen(src).unwrap();
        let dark = render_svg_for_screen("flowchart LR\n  A --> B").unwrap();
        assert_ne!(forest, dark, "the author's theme must override the dark site theme");
    }
}
