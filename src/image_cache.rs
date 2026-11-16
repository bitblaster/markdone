//! Shared, cheaply-cloneable cache of decoded images for standalone-image rendering.
//!
//! Mirrors `validation::GitHubValidationCache`: an `Arc<Mutex<HashMap>>` keyed by the raw
//! URL string. Loads (local file reads or remote fetches) run on worker tasks that
//! write their result here and wake the winit loop; the layout/draw path reads it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use resvg::tiny_skia;
use resvg::usvg::{self, fontdb};
use vello::peniko::{Blob, ImageAlphaType, ImageBrush, ImageData, ImageFormat};

/// Supersample factor for SVG rasterization: render at 2× the logical size so the
/// vector art stays crisp on hidpi, then downscale to the logical dest at draw time.
const SVG_SUPERSAMPLE: f32 = 2.0;
/// Cap on the rasterized SVG pixel dimensions. Beyond ~1024px in a dimension, Vello's
/// per-frame image render falls off a fast path on some backends (measured ~2ms at 900px
/// vs ~16ms at 1159px on Asahi/Mesa) — and the whole scene, images included, re-renders
/// every frame, so a large diagram makes every repaint drop frames. 900 keeps large
/// diagrams (mermaid) on the fast path while staying crisp; badges are far below it.
const SVG_MAX_RASTER: f32 = 900.0;

/// Widest raster kept for a decoded bitmap image (device px). Images are displayed at
/// most `MAX_CONTENT_WIDTH × scale` wide, so decoding a 1440-px screenshot at full size
/// only wastes memory (and pushes Vello off its fast image path, see `SVG_MAX_RASTER`).
/// Wider images are downscaled at decode time; their logical display size is unchanged,
/// so layout is identical. Set by the shell from the window scale; 1024 until then.
static MAX_RASTER_W: AtomicU32 = AtomicU32::new(1024);

/// Set the widest raster kept for decoded bitmap images (device px, min 256). Affects
/// images decoded after the call.
pub fn set_max_raster_width(px: u32) {
    MAX_RASTER_W.store(px.max(256), Ordering::Relaxed);
}

/// System-font database, loaded once (the load is expensive) and shared across every
/// SVG parse so badge `<text>` labels resolve to real fonts.
static SVG_FONTS: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

/// A decoded image ready to paint, plus its raster pixel size and logical display
/// size. For raster sources the two match; for SVG the raster size is supersampled
/// while the display size is the SVG's intrinsic logical size (see `decode_svg`).
/// `Arc`-wrapped in the cache so a cache clone stays O(1) (the `Blob` is Arc-backed
/// too, but the `Arc<LoadedImage>` keeps the whole entry a single refcount bump).
pub struct LoadedImage {
    pub brush: ImageBrush,
    pub width: u32,
    pub height: u32,
    pub display_w: f32,
    pub display_h: f32,
    /// For math: logical px from the image top down to the text baseline (so inline math
    /// can sit on the line). `None` for ordinary images (they have no baseline).
    pub baseline: Option<f32>,
}

/// Load state for one image URL.
#[derive(Clone)]
pub enum ImageState {
    /// A load has been spawned but hasn't finished.
    Loading,
    /// Decoded and ready to paint.
    Loaded(Arc<LoadedImage>),
    /// The fetch, decode, or render failed. Carries a short human-readable reason when one
    /// is available (e.g. a mermaid/LaTeX syntax error), shown in the placeholder so the
    /// author sees what went wrong; `None` for a plain image load failure.
    Failed(Option<Arc<str>>),
    /// Was loaded, pixels dropped to save memory (see [`ImageCache::trim`]). Keeps the
    /// logical display size so the layout reserves the exact same height and scrolling
    /// never jumps; it is reloaded when it comes back near the viewport.
    Evicted { display_w: f32, display_h: f32 },
}

/// Longest render-error reason kept for the placeholder — enough for a mermaid/LaTeX parse
/// error without letting a pathological message blow up the box.
const MAX_ERR_LEN: usize = 200;

/// Collapse whitespace/newlines and cap length so a render error fits the placeholder box.
fn truncate_reason(msg: &str) -> String {
    let one_line = msg.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(MAX_ERR_LEN) {
        Some((byte, _)) => format!("{}…", &one_line[..byte]),
        None => one_line,
    }
}

#[derive(Default)]
struct Inner {
    states: HashMap<String, ImageState>,
    /// URLs whose pixels may be dropped under memory pressure: file/remote images, which
    /// can be re-read. Mermaid/math renders are not (re-rendering is the costly part).
    evictable: HashSet<String>,
    /// Loads currently running (an `Evicted` entry keeps its state while reloading).
    in_flight: HashSet<String>,
    /// Last `trim` tick each evictable URL was wanted near the viewport (LRU order).
    last_used: HashMap<String, u64>,
    tick: u64,
}

/// Thread-safe cache of image load states, shared across clones (like the GitHub
/// caches). Cheap to clone: just bumps the inner `Arc`.
#[derive(Clone, Default)]
pub struct ImageCache {
    inner: Arc<Mutex<Inner>>,
}

impl ImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, url: &str) -> Option<ImageState> {
        self.inner.lock().unwrap().states.get(url).cloned()
    }

    /// Presence check without cloning the (image-bearing) state — for the load guard.
    pub fn contains(&self, url: &str) -> bool {
        self.inner.lock().unwrap().states.contains_key(url)
    }

    fn insert(&self, url: &str, state: ImageState) {
        let mut inner = self.inner.lock().unwrap();
        inner.in_flight.remove(url);
        inner.states.insert(url.to_string(), state);
    }

    /// Claim an evictable image (file/remote) for loading: returns `false` when it is
    /// already loaded, loading, or failed. A never-seen URL becomes `Loading`; an
    /// `Evicted` one keeps its state (and so its reserved height) until the reload lands.
    pub fn begin_evictable_load(&self, url: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.in_flight.contains(url) {
            return false;
        }
        match inner.states.get(url) {
            None => {
                inner.states.insert(url.to_string(), ImageState::Loading);
            }
            Some(ImageState::Evicted { .. }) => {}
            Some(_) => return false,
        }
        inner.in_flight.insert(url.to_string());
        inner.evictable.insert(url.to_string());
        true
    }

    /// Bound the decoded-pixel memory of evictable images to `budget_bytes`. URLs in
    /// `keep` (the images in or near the viewport) are marked recently used and never
    /// evicted; beyond the budget, the least recently wanted others drop their pixels
    /// (→ [`ImageState::Evicted`]). Returns the bytes still held by evictable images and
    /// how many images were evicted.
    pub fn trim(&self, keep: &[String], budget_bytes: usize) -> (usize, usize) {
        let mut inner = self.inner.lock().unwrap();
        inner.tick += 1;
        let tick = inner.tick;
        for url in keep {
            if inner.evictable.contains(url) {
                inner.last_used.insert(url.clone(), tick);
            }
        }
        let Inner {
            states,
            evictable,
            last_used,
            ..
        } = &mut *inner;
        let mut held: Vec<(u64, usize, &String)> = Vec::new();
        let mut total = 0usize;
        for url in evictable.iter() {
            if let Some(ImageState::Loaded(img)) = states.get(url) {
                let bytes = img.width as usize * img.height as usize * 4;
                total += bytes;
                if !keep.contains(url) {
                    held.push((last_used.get(url).copied().unwrap_or(0), bytes, url));
                }
            }
        }
        held.sort_unstable_by_key(|&(t, _, _)| t);
        let mut evict = Vec::new();
        for (_, bytes, url) in held {
            if total <= budget_bytes {
                break;
            }
            total -= bytes;
            evict.push(url.clone());
        }
        let evicted = evict.len();
        for url in evict {
            if let Some(ImageState::Loaded(img)) = states.get(&url) {
                let (display_w, display_h) = (img.display_w, img.display_h);
                states.insert(url, ImageState::Evicted { display_w, display_h });
            }
        }
        (total, evicted)
    }

    pub fn mark_loading(&self, url: &str) {
        self.insert(url, ImageState::Loading);
    }

    pub fn set_loaded(&self, url: &str, image: LoadedImage) {
        self.insert(url, ImageState::Loaded(Arc::new(image)));
    }

    pub fn set_failed(&self, url: &str) {
        self.insert(url, ImageState::Failed(None));
    }

    /// Mark failed with a short reason (mermaid/LaTeX syntax error) shown in the placeholder.
    pub fn set_failed_with(&self, url: &str, reason: impl Into<Arc<str>>) {
        self.insert(url, ImageState::Failed(Some(reason.into())));
    }

    /// Store the outcome of an off-thread render: `Ok` → loaded, `Err(Some)` → failed with a
    /// reason shown in the placeholder (collapsed + length-capped), `Err(None)` → generic
    /// failure. Shared by the mermaid and math render paths.
    pub fn store_render(&self, url: &str, result: Result<LoadedImage, Option<String>>) {
        match result {
            Ok(img) => self.set_loaded(url, img),
            Err(Some(reason)) => self.set_failed_with(url, truncate_reason(&reason)),
            Err(None) => self.set_failed(url),
        }
    }
}

/// Decode encoded image bytes into a paintable `LoadedImage`. Tries the raster path
/// (PNG/JPEG/GIF/WebP) first, then falls back to SVG. Returns `None` on any failure.
/// The RGBA8 buffer from the `image` crate is straight (un-premultiplied) alpha,
/// matching `ImageAlphaType::Alpha`.
pub fn decode(bytes: &[u8]) -> Option<LoadedImage> {
    if let Ok(mut img) = image::load_from_memory(bytes) {
        // Logical size is the original's; the raster may be downscaled below it.
        let (display_w, display_h) = (img.width() as f32, img.height() as f32);
        let max_w = MAX_RASTER_W.load(Ordering::Relaxed);
        if img.width() > max_w {
            let h = ((img.height() as u64 * max_w as u64) / img.width() as u64).max(1) as u32;
            img = img.resize_exact(max_w, h, image::imageops::FilterType::Triangle);
        }
        let rgba = img.to_rgba8();
        let width = rgba.width();
        let height = rgba.height();
        let data = ImageData {
            data: Blob::new(Arc::new(rgba.into_raw())),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width,
            height,
        };
        return Some(LoadedImage {
            brush: ImageBrush::new(data),
            width,
            height,
            display_w,
            display_h,
            baseline: None,
        });
    }
    looks_like_svg(bytes).then(|| decode_svg(bytes)).flatten()
}

/// Cheap sniff for SVG: after trimming a UTF-8 BOM and leading whitespace, the bytes
/// should start with `<?xml` or `<svg`, or contain `<svg` within the first ~1 KiB.
/// (Gzipped `.svgz` is out of scope and won't match.)
fn looks_like_svg(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let trimmed = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map_or(&[][..], |i| &bytes[i..]);
    if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<svg") {
        return true;
    }
    let head = &trimmed[..trimmed.len().min(1024)];
    head.windows(4).any(|w| w == b"<svg")
}

/// Rasterize an SVG at `SVG_SUPERSAMPLE`× its logical size (capped) so it stays crisp
/// on hidpi. `tiny_skia`'s pixmap is premultiplied RGBA8, hence `AlphaPremultiplied`
/// (unlike the raster path's straight `Alpha`).
fn decode_svg(bytes: &[u8]) -> Option<LoadedImage> {
    let opt = usvg::Options {
        fontdb: SVG_FONTS.clone(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(bytes, &opt).ok()?;
    let size = tree.size();
    let (dw, dh) = (size.width(), size.height());
    if dw <= 0.0 || dh <= 0.0 {
        return None;
    }
    // Clamp the supersample so neither raster dimension exceeds SVG_MAX_RASTER.
    let k = SVG_SUPERSAMPLE.min(SVG_MAX_RASTER / dw.max(dh));
    let rw = ((dw * k).round() as u32).max(1);
    let rh = ((dh * k).round() as u32).max(1);
    let mut pixmap = tiny_skia::Pixmap::new(rw, rh)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(k, k),
        &mut pixmap.as_mut(),
    );
    let data = ImageData {
        data: Blob::new(Arc::new(pixmap.data().to_vec())),
        format: ImageFormat::Rgba8,
        alpha_type: ImageAlphaType::AlphaPremultiplied,
        width: rw,
        height: rh,
    };
    Some(LoadedImage {
        brush: ImageBrush::new(data),
        width: rw,
        height: rh,
        display_w: dw,
        display_h: dh,
        baseline: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat as CrateImageFormat, RgbaImage};
    use std::io::Cursor;

    /// Encode a tiny solid RGBA image to PNG bytes for round-trip decode tests.
    fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), CrateImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn decode_reports_dimensions() {
        let png = tiny_png(7, 3);
        let loaded = decode(&png).expect("tiny PNG should decode");
        assert_eq!((loaded.width, loaded.height), (7, 3));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode(b"not an image").is_none());
    }

    #[test]
    fn decode_rasterizes_svg() {
        let svg = r#"<svg xmlns='http://www.w3.org/2000/svg' width='40' height='20'><rect width='40' height='20' fill='#4c1'/></svg>"#;
        let loaded = decode(svg.as_bytes()).expect("inline SVG should decode");
        assert!((loaded.display_w - 40.0).abs() < 0.5);
        assert!((loaded.display_h - 20.0).abs() < 0.5);
        // 2× supersample → raster is ~80×40.
        assert_eq!((loaded.width, loaded.height), (80, 40));
    }

    #[test]
    fn decode_downscales_wide_images_keeping_display_size() {
        set_max_raster_width(1024);
        let loaded = decode(&tiny_png(2048, 100)).unwrap();
        assert_eq!((loaded.width, loaded.height), (1024, 50));
        assert_eq!((loaded.display_w, loaded.display_h), (2048.0, 100.0));
    }

    #[test]
    fn trim_evicts_least_recently_wanted_and_keeps_size() {
        let cache = ImageCache::new();
        for url in ["a", "b", "c"] {
            assert!(cache.begin_evictable_load(url));
            cache.set_loaded(url, decode(&tiny_png(10, 10)).unwrap()); // 400 bytes each
        }
        // a and b wanted at tick 1, then only b at tick 2.
        cache.trim(&["a".into(), "b".into(), "c".into()], usize::MAX);
        cache.trim(&["b".into()], usize::MAX);
        // Budget for two images: the oldest non-kept (c, never re-wanted... a at tick 1,
        // c at tick 1) goes first; b is kept.
        let (held, evicted) = cache.trim(&["b".into()], 800);
        assert_eq!((held, evicted), (800, 1));
        assert!(matches!(cache.get("b"), Some(ImageState::Loaded(_))));
        let evicted: Vec<_> = ["a", "c"]
            .into_iter()
            .filter(|u| matches!(cache.get(u), Some(ImageState::Evicted { .. })))
            .collect();
        assert_eq!(evicted.len(), 1);
        // An evicted image reports its size and can be reloaded once.
        let url = evicted[0];
        match cache.get(url) {
            Some(ImageState::Evicted { display_w, display_h }) => {
                assert_eq!((display_w, display_h), (10.0, 10.0))
            }
            _ => unreachable!(),
        }
        assert!(cache.begin_evictable_load(url));
        assert!(!cache.begin_evictable_load(url), "second claim while in flight");
        assert!(matches!(cache.get(url), Some(ImageState::Evicted { .. })));
    }

    #[test]
    fn cache_shares_state_across_clones() {
        let cache = ImageCache::new();
        let other = cache.clone();
        assert!(cache.get("x.png").is_none());
        cache.mark_loading("x.png");
        assert!(matches!(other.get("x.png"), Some(ImageState::Loading)));
        let loaded = decode(&tiny_png(2, 2)).unwrap();
        cache.set_loaded("x.png", loaded);
        assert!(matches!(other.get("x.png"), Some(ImageState::Loaded(_))));
    }
}
