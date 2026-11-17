//! The table of contents: a side panel listing the document's headings, docked at the
//! right of the document area. Click a heading to jump to it; the current section is
//! highlighted and kept in view; drag the left edge to resize.

/// Panel width bounds, logical px.
pub const MIN_W: f32 = 160.0;
pub const MAX_W: f32 = 560.0;
/// Width of the grab zone on the panel's left edge (half of it overlaps the document).
pub const GRIP_W: f32 = 8.0;

const ROW_H: f32 = 24.0;
const INDENT: f32 = 14.0;
const PAD_X: f32 = 12.0;

/// One heading of the list.
pub struct TocEntry {
    pub level: u8,
    pub text: String,
    /// The section is folded in the document.
    pub folded: bool,
}

pub struct TocResponse {
    /// Index of the heading clicked this frame.
    pub clicked: Option<usize>,
    /// The panel width (logical px) after this frame's resize drag, if any.
    pub width: f32,
    /// A resize drag is in progress.
    pub resizing: bool,
}

/// Draw the panel in `rect` (points). `reveal_current`: scroll the list so the current
/// heading is visible (set when it changes).
pub fn toc_panel(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    entries: &[TocEntry],
    current: Option<usize>,
    reveal_current: bool,
) -> TocResponse {
    let visuals = ui.visuals().clone();
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, visuals.panel_fill);
    painter.vline(
        rect.left(),
        rect.y_range(),
        egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
    );

    // Resize grip on the left edge.
    let grip = egui::Rect::from_min_max(
        egui::pos2(rect.left() - GRIP_W / 2.0, rect.top()),
        egui::pos2(rect.left() + GRIP_W / 2.0, rect.bottom()),
    );
    let grip_response = ui.interact(grip, egui::Id::new("writ-toc-grip"), egui::Sense::drag());
    if grip_response.hovered() || grip_response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        painter.vline(
            rect.left(),
            rect.y_range(),
            egui::Stroke::new(2.0, visuals.widgets.active.bg_fill),
        );
    }
    let width = (rect.width() - grip_response.drag_delta().x).clamp(MIN_W, MAX_W);

    let mut clicked = None;
    let inner = rect.shrink2(egui::vec2(0.0, 6.0));
    // A child that does not allocate in the parent: allocating would shrink the root's
    // free area, and egui would then claim the pointer (wheel, clicks) over the whole
    // document.
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    (|ui: &mut egui::Ui| {
            ui.set_clip_rect(inner);
            ui.horizontal(|ui| {
                ui.add_space(PAD_X);
                ui.label(egui::RichText::new("Contents").small().weak());
            });
            ui.add_space(2.0);
            if entries.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(PAD_X);
                    ui.label(egui::RichText::new("No headings").weak());
                });
                return;
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (i, entry) in entries.iter().enumerate() {
                        let is_current = current == Some(i);
                        if row(ui, entry, is_current, &visuals, reveal_current && is_current) {
                            clicked = Some(i);
                        }
                    }
                });
    })(&mut child);
    TocResponse {
        clicked,
        width,
        resizing: grip_response.dragged(),
    }
}

/// One heading row; returns `true` when clicked.
fn row(
    ui: &mut egui::Ui,
    entry: &TocEntry,
    current: bool,
    visuals: &egui::Visuals,
    reveal: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_H),
        egui::Sense::click(),
    );
    if reveal {
        response.scroll_to_me(Some(egui::Align::Center));
    }
    let painter = ui.painter_at(rect);
    if current {
        painter.rect_filled(rect, 0.0, visuals.selection.bg_fill);
    } else if response.hovered() {
        painter.rect_filled(rect, 0.0, visuals.widgets.hovered.weak_bg_fill.gamma_multiply(0.6));
    }

    let indent = f32::from(entry.level.saturating_sub(1)) * INDENT;
    let x = rect.left() + PAD_X + indent;
    let color = if current || entry.level == 1 {
        visuals.strong_text_color()
    } else if entry.level == 2 {
        visuals.text_color()
    } else {
        visuals.weak_text_color()
    };
    if entry.folded {
        // ▸ in the indent gap: the section is collapsed in the document.
        let c = egui::pos2(x - 7.0, rect.center().y);
        painter.add(egui::Shape::convex_polygon(
            vec![
                c + egui::vec2(-2.0, -4.0),
                c + egui::vec2(3.0, 0.0),
                c + egui::vec2(-2.0, 4.0),
            ],
            color,
            egui::Stroke::NONE,
        ));
    }
    let mut job = egui::text::LayoutJob::single_section(
        entry.text.clone(),
        egui::TextFormat {
            font_id: egui::TextStyle::Body.resolve(ui.style()),
            color,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width((rect.right() - PAD_X - x).max(10.0));
    let galley = painter.layout_job(job);
    let elided = galley.elided;
    painter.galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, color);
    let response = if elided {
        response.on_hover_text(&entry.text)
    } else {
        response
    };
    response.clicked()
}
