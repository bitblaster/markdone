//! Vertical scrollbar for the document viewport.
//!
//! Overlay-style: a thin track drawn in the right padding of the document region (left
//! of the outline panel when that is open), so it never reflows the text. The thumb
//! maps `scroll_y / max_scroll` onto the track. Off-screen line heights are estimates
//! (anchor-forward virtualization), so `content_height` — and with it the thumb size —
//! can shift slightly as lines get laid out; that is inherent to the estimate, not a bug.

use vello::Scene;
use vello::kurbo::{Affine, Rect, RoundedRect};
use vello::peniko::Fill;

use crate::doc_layout::DocLayout;
use crate::editor::EditorTheme;
use crate::text_engine::peniko_color_alpha;

/// Visible track width (logical px).
const TRACK_W: f32 = 10.0;
/// Gap between the track and the right edge of the document region (logical px).
const EDGE_GAP: f32 = 2.0;
/// Width of the pointer hit zone, measured from the region's right edge (logical px).
/// Wider than the track so it is easy to grab.
const HIT_W: f32 = 16.0;
/// Minimum thumb height (logical px), so a very long document stays grabbable.
const MIN_THUMB_H: f32 = 32.0;

/// Scrollbar geometry in device px.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub track_x0: f32,
    pub track_x1: f32,
    pub track_y0: f32,
    pub track_y1: f32,
    pub thumb_y0: f32,
    pub thumb_y1: f32,
    /// Left edge of the hit zone (the zone extends to the region's right edge).
    pub hit_x0: f32,
    pub hit_x1: f32,
}

impl Geometry {
    pub fn thumb_h(&self) -> f32 {
        self.thumb_y1 - self.thumb_y0
    }

    /// Whether `pos` falls inside the pointer hit zone (full track height).
    pub fn hit(&self, pos: (f32, f32)) -> bool {
        pos.0 >= self.hit_x0
            && pos.0 < self.hit_x1
            && pos.1 >= self.track_y0
            && pos.1 < self.track_y1
    }

    /// Whether `y` falls on the thumb.
    pub fn on_thumb(&self, y: f32) -> bool {
        y >= self.thumb_y0 && y < self.thumb_y1
    }
}

/// Geometry for the current scroll state, or `None` when the whole document fits in the
/// viewport (no scrollbar). `doc_w` is the document region's width (surface width minus
/// the outline panel), `content_top`/`editor_h` the viewport's vertical extent.
pub fn geometry(
    doc: &DocLayout,
    doc_w: f32,
    content_top: f32,
    editor_h: f32,
    scale: f32,
) -> Option<Geometry> {
    let content_h = doc.content_height();
    let max_scroll = doc.max_scroll(editor_h);
    if max_scroll <= 0.5 || editor_h <= 1.0 {
        return None;
    }
    let track_x1 = doc_w - EDGE_GAP * scale;
    let track_x0 = track_x1 - TRACK_W * scale;
    let min_h = (MIN_THUMB_H * scale).min(editor_h);
    let thumb_h = (editor_h * editor_h / content_h).clamp(min_h, editor_h);
    let frac = (doc.scroll_y / max_scroll).clamp(0.0, 1.0);
    let thumb_y0 = content_top + frac * (editor_h - thumb_h);
    Some(Geometry {
        track_x0,
        track_x1,
        track_y0: content_top,
        track_y1: content_top + editor_h,
        thumb_y0,
        thumb_y1: thumb_y0 + thumb_h,
        hit_x0: doc_w - HIT_W * scale,
        hit_x1: doc_w,
    })
}

/// The `scroll_y` that puts the thumb's top at `thumb_top` (device px, absolute).
pub fn scroll_for_thumb_top(doc: &DocLayout, geo: &Geometry, editor_h: f32, thumb_top: f32) -> f32 {
    let travel = (geo.track_y1 - geo.track_y0) - geo.thumb_h();
    if travel <= 0.0 {
        return 0.0;
    }
    let frac = ((thumb_top - geo.track_y0) / travel).clamp(0.0, 1.0);
    frac * doc.max_scroll(editor_h)
}

/// Paint the track and thumb. `active` (hovered or dragging) draws a stronger thumb.
pub fn draw(scene: &mut Scene, theme: &EditorTheme, geo: &Geometry, active: bool, scale: f32) {
    let radius = (TRACK_W * scale / 2.0) as f64;
    let track = RoundedRect::new(
        geo.track_x0 as f64,
        geo.track_y0 as f64,
        geo.track_x1 as f64,
        geo.track_y1 as f64,
        radius,
    );
    let track_alpha = if active { 0.10 } else { 0.05 };
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        peniko_color_alpha(theme.foreground, track_alpha),
        None,
        &track,
    );
    let thumb = RoundedRect::from_rect(
        Rect::new(
            geo.track_x0 as f64,
            geo.thumb_y0 as f64,
            geo.track_x1 as f64,
            geo.thumb_y1 as f64,
        ),
        radius,
    );
    let thumb_alpha = if active { 0.55 } else { 0.30 };
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        peniko_color_alpha(theme.foreground, thumb_alpha),
        None,
        &thumb,
    );
}
