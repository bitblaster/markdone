//! Document tab strip: one tab per open document, docked between the find bar and the
//! status bar (a bottom strip like the find bar, so the document's coordinate origin —
//! and with it every hit test — is unchanged).

use vello::Scene;
use vello::kurbo::{Affine, Line, Rect, Stroke};
use vello::peniko::Fill;

use crate::chrome::BarRect;
use crate::consts::UI_LINE_HEIGHT;
use crate::doc_layout::ScreenRect;
use crate::editor::EditorTheme;
use crate::text_engine::{TextEngine, peniko_color, peniko_color_alpha};

/// Tab label font size (logical px).
const FONT_SIZE: f32 = 14.0;
/// Horizontal padding inside a tab (logical px).
const PAD_X: f32 = 12.0;
/// Natural tab width bounds (logical px); tabs shrink below MIN only when crowded.
const MIN_W: f32 = 110.0;
const MAX_W: f32 = 240.0;

/// What a tab shows.
pub struct TabLabel {
    pub name: String,
    pub dirty: bool,
}

/// Paint the strip and return one hit rect per tab (device px), in tab order.
pub fn draw_tab_bar(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    bar: &BarRect,
    tabs: &[TabLabel],
    active: usize,
    hover: Option<usize>,
    scale: f32,
) -> Vec<ScreenRect> {
    let bar_rect = Rect::new(bar.x0, bar.y0, bar.x1, bar.y1);
    scene.fill(Fill::NonZero, Affine::IDENTITY, peniko_color(theme.surface), None, &bar_rect);
    if tabs.is_empty() {
        return Vec::new();
    }
    let s = scale as f64;
    let pad = (PAD_X * scale) as f64;
    // Lay out labels first to size the tabs.
    let layouts: Vec<_> = tabs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let text = if t.dirty { format!("● {}", t.name) } else { t.name.clone() };
            let color = if i == active { theme.foreground } else { theme.comment };
            engine.build_line(&text, scale, FONT_SIZE, UI_LINE_HEIGHT, peniko_color(color), None, &[])
        })
        .collect();
    let avail = bar.x1 - bar.x0;
    let natural: Vec<f64> = layouts
        .iter()
        .map(|l| (l.width() as f64 + 2.0 * pad).clamp(MIN_W as f64 * s, MAX_W as f64 * s))
        .collect();
    let total: f64 = natural.iter().sum();
    let shrink = if total > avail { avail / total } else { 1.0 };

    let mut rects = Vec::with_capacity(tabs.len());
    let mut x = bar.x0;
    for (i, layout) in layouts.iter().enumerate() {
        let w = natural[i] * shrink;
        let r = Rect::new(x, bar.y0, x + w, bar.y1);
        if i == active {
            scene.fill(Fill::NonZero, Affine::IDENTITY, peniko_color(theme.background), None, &r);
            // Accent line on the active tab's top edge.
            scene.stroke(
                &Stroke::new(2.0 * s),
                Affine::IDENTITY,
                peniko_color(theme.cyan),
                None,
                &Line::new((r.x0, r.y0 + s), (r.x1, r.y0 + s)),
            );
        } else if hover == Some(i) {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                peniko_color_alpha(theme.foreground, 0.06),
                None,
                &r,
            );
        }
        // Separator.
        scene.stroke(
            &Stroke::new(s),
            Affine::IDENTITY,
            peniko_color(theme.selection),
            None,
            &Line::new((r.x1, r.y0 + 4.0 * s), (r.x1, r.y1 - 4.0 * s)),
        );
        // Label, clipped to the tab.
        scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &Rect::new(r.x0 + pad, r.y0, r.x1 - pad * 0.5, r.y1));
        let ty = bar.y0 as f32 + ((bar.y1 - bar.y0) as f32 - layout.height()) / 2.0;
        engine.draw_line(scene, layout, ((r.x0 + pad) as f32, ty));
        scene.pop_layer();
        rects.push((r.x0, r.y0, r.x1, r.y1));
        x += w;
    }
    rects
}
