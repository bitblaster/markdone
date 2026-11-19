//! Mermaid diagram rendering: detect a ```` ```mermaid ```` fence, render its source to
//! SVG with `merman`, and draw that SVG as *vectors*: the SVG is parsed once (usvg, text
//! converted to outlines with the system fonts) and recorded into a Vello scene fragment
//! (see [`encode_group`]), which the standalone-image draw path appends scaled to its
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
use vello::kurbo::{Affine, Rect};
use vello::peniko::{BlendMode, Fill, Mix};
use vello_svg::usvg;
use vello_svg::util::{to_affine, to_bez_path, to_brush, to_stroke};

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
/// fragment, in the SVG's logical units (see [`encode_group`] for what is drawn and how).
/// `None` if the SVG doesn't parse or is empty.
pub fn svg_to_vector(svg: &str) -> Option<LoadedImage> {
    let opt = usvg::Options {
        fontdb: svg_fonts(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let mut scene = vello::Scene::new();
    encode_group(&mut scene, tree.root(), Affine::IDENTITY);
    Some(LoadedImage {
        paint: ImagePaint::Vector(Arc::new(scene)),
        width: w.round() as u32,
        height: h.round() as u32,
        display_w: w,
        display_h: h,
        baseline: None,
    })
}

/// Record a usvg group into `scene` (each node at `base` × its absolute transform).
///
/// Our own walk instead of `vello_svg::append_tree`, for two reasons:
/// - `vello_svg` wraps *every* group in a clip layer. Mermaid SVGs nest groups deeply, and
///   past a few nested layers Vello spills each covered 16×16 tile to a fixed-size GPU
///   buffer; a tall diagram filling a large window overflowed it, and Vello then silently
///   drops the whole frame (the screen froze while scrolling on a 2560×1274 window).
///   Here a group opens a layer only when it needs one: opacity, a non-normal blend, or a
///   clip/mask (masks are approximated by their bounds, like `vello_svg` does).
/// - `vello_svg` 0.11 applies a text node's transform twice (its `flattened()` outlines
///   already carry it), pushing labels off their place; here they get `base` only.
///
/// Filters are not drawn (none in merman's resvg-safe output), nor are raster images
/// embedded in the SVG; nested SVG images are walked like groups.
fn encode_group(scene: &mut vello::Scene, group: &usvg::Group, base: Affine) {
    for node in group.children() {
        let transform = base * to_affine(&node.abs_transform());
        match node {
            usvg::Node::Group(g) => {
                let alpha = g.opacity().get();
                if alpha <= 0.0 {
                    continue; // fully transparent: nothing to draw (merman emits some)
                }
                let blend = blend_mode(g.blend_mode());
                let needs_layer = alpha < 1.0
                    || blend != Mix::Normal.into()
                    || g.clip_path().is_some()
                    || g.mask().is_some();
                if !needs_layer {
                    encode_group(scene, g, base);
                    continue;
                }
                // A single-path clip is exact; anything else clips to the layer bounds.
                match g.clip_path().and_then(|c| c.root().children().first()) {
                    Some(usvg::Node::Path(clip)) => {
                        scene.push_layer(Fill::NonZero, blend, alpha, transform, &to_bez_path(clip));
                    }
                    _ => {
                        let b = g.layer_bounding_box();
                        let rect = Rect::new(
                            b.x() as f64,
                            b.y() as f64,
                            (b.x() + b.width()) as f64,
                            (b.y() + b.height()) as f64,
                        );
                        scene.push_layer(Fill::NonZero, blend, alpha, transform, &rect);
                    }
                }
                encode_group(scene, g, base);
                scene.pop_layer();
            }
            usvg::Node::Path(path) => encode_path(scene, path, transform),
            usvg::Node::Text(text) => encode_group(scene, text.flattened(), base),
            usvg::Node::Image(img) => {
                if let usvg::ImageKind::SVG(svg) = img.kind()
                    && img.is_visible()
                {
                    encode_group(scene, svg.root(), transform);
                }
            }
        }
    }
}

/// Fill and/or stroke one usvg path, in the SVG's paint order.
fn encode_path(scene: &mut vello::Scene, path: &usvg::Path, transform: Affine) {
    if !path.is_visible() {
        return;
    }
    let shape = to_bez_path(path);
    let fill = |scene: &mut vello::Scene| {
        if let Some(fill) = path.fill()
            && let Some((brush, brush_transform)) = to_brush(fill.paint(), fill.opacity())
        {
            let rule = match fill.rule() {
                usvg::FillRule::NonZero => Fill::NonZero,
                usvg::FillRule::EvenOdd => Fill::EvenOdd,
            };
            scene.fill(rule, transform, &brush, Some(brush_transform), &shape);
        }
    };
    let stroke = |scene: &mut vello::Scene| {
        if let Some(stroke) = path.stroke()
            && let Some((brush, brush_transform)) = to_brush(stroke.paint(), stroke.opacity())
        {
            let style = to_stroke(stroke);
            scene.stroke(&style, transform, &brush, Some(brush_transform), &shape);
        }
    };
    match path.paint_order() {
        usvg::PaintOrder::FillAndStroke => {
            fill(scene);
            stroke(scene);
        }
        usvg::PaintOrder::StrokeAndFill => {
            stroke(scene);
            fill(scene);
        }
    }
}

fn blend_mode(mode: usvg::BlendMode) -> BlendMode {
    match mode {
        usvg::BlendMode::Normal => Mix::Normal,
        usvg::BlendMode::Multiply => Mix::Multiply,
        usvg::BlendMode::Screen => Mix::Screen,
        usvg::BlendMode::Overlay => Mix::Overlay,
        usvg::BlendMode::Darken => Mix::Darken,
        usvg::BlendMode::Lighten => Mix::Lighten,
        usvg::BlendMode::ColorDodge => Mix::ColorDodge,
        usvg::BlendMode::ColorBurn => Mix::ColorBurn,
        usvg::BlendMode::HardLight => Mix::HardLight,
        usvg::BlendMode::SoftLight => Mix::SoftLight,
        usvg::BlendMode::Difference => Mix::Difference,
        usvg::BlendMode::Exclusion => Mix::Exclusion,
        usvg::BlendMode::Hue => Mix::Hue,
        usvg::BlendMode::Saturation => Mix::Saturation,
        usvg::BlendMode::Color => Mix::Color,
        usvg::BlendMode::Luminosity => Mix::Luminosity,
    }
    .into()
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

    fn vector_scene(svg: &str) -> Arc<vello::Scene> {
        match svg_to_vector(svg).expect("SVG must convert").paint {
            ImagePaint::Vector(scene) => scene,
            ImagePaint::Raster(_) => unreachable!(),
        }
    }

    /// Labels are drawn as glyph outlines, placed inside the diagram.
    #[test]
    fn labels_are_drawn_as_outlines_inside_the_diagram() {
        let svg = render_svg_for_screen("flowchart LR\n  A[Uno] --> B[Due]").unwrap();
        let bare = svg.replace(">Uno<", "><").replace(">Due<", "><");
        let with = vector_scene(&svg);
        let without = vector_scene(&bare);
        assert!(
            with.encoding().path_data.len() > without.encoding().path_data.len(),
            "labels must add glyph outlines"
        );
        // Transforms applied once: every transform keeps its translation within the
        // diagram (vello_svg 0.11 doubled the text one, sending labels off-diagram).
        let img = svg_to_vector(&svg).unwrap();
        for t in &with.encoding().transforms {
            let [tx, ty] = t.translation;
            // A few px of slack for markers/arrowheads drawn just outside their anchor.
            let m = 20.0;
            assert!(
                (-m..=img.display_w + m).contains(&tx) && (-m..=img.display_h + m).contains(&ty),
                "transform translates outside the diagram: ({tx}, {ty})"
            );
        }
    }

    /// Plain mermaid groups must not become clip layers: nested layers make Vello spill
    /// every covered tile to a fixed GPU buffer, and a tall diagram in a large window
    /// overflowed it — Vello then drops the whole frame (a frozen screen while scrolling).
    #[test]
    fn plain_groups_open_no_layers() {
        let src = "flowchart TB\n  subgraph X\n    A --> B --> C\n  end\n  subgraph Y\n    D --> E\n  end\n  A --> E";
        let svg = render_svg_for_screen(src).unwrap();
        let ours = vector_scene(&svg).encoding().n_clips;
        // vello_svg's one-layer-per-group encoding of the same SVG, for scale.
        let tree = usvg::Tree::from_str(
            &svg,
            &usvg::Options {
                fontdb: svg_fonts(),
                ..Default::default()
            },
        )
        .unwrap();
        let mut every_group = vello::Scene::new();
        vello_svg::append_tree(&mut every_group, &tree);
        let theirs = every_group.encoding().n_clips;
        // Only genuine clip-path groups remain (a handful); invisible groups are skipped.
        assert!(
            ours * 10 <= theirs,
            "expected far fewer layers than one per group: {ours} vs {theirs}"
        );
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
