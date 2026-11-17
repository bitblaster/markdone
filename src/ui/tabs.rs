//! Document tab strip, under the menu bar.
//!
//! One tab per open document: click to activate, × or middle click to close, drag to
//! reorder, right click for the context menu. When the tabs do not fit, the strip
//! scrolls horizontally (mouse wheel over it).

use std::path::PathBuf;

use crate::commands::Command;

/// Height of the tab strip, in logical px.
pub const TAB_STRIP_H: f32 = 34.0;
/// Natural tab width bounds (logical px).
const MIN_W: f32 = 120.0;
const MAX_W: f32 = 240.0;
/// Size of the close button (logical px).
const CLOSE_W: f32 = 18.0;
/// Room taken by the unsaved-changes dot before the name (logical px).
const DOT_W: f32 = 12.0;

/// What a tab shows.
pub struct TabInfo {
    pub name: String,
    pub path: Option<PathBuf>,
    pub dirty: bool,
}

/// Lay out the tab strip in a top panel (call after the menu bar's panel). Returns the
/// command triggered this frame, if any.
pub fn tab_strip(ui: &mut egui::Ui, tabs: &[TabInfo], active: usize) -> Option<Command> {
    let mut picked = None;
    let frame = egui::Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .inner_margin(egui::Margin::symmetric(6, 0));
    egui::Panel::top("writ-tab-strip")
        .exact_size(TAB_STRIP_H)
        .resizable(false)
        .show_separator_line(false)
        .frame(frame)
        .show(ui, |ui| {
            egui::ScrollArea::horizontal()
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (i, tab) in tabs.iter().enumerate() {
                            if let Some(cmd) = tab_ui(ui, tab, i, i == active, tabs.len()) {
                                picked = Some(cmd);
                            }
                        }
                    });
                });
        });
    picked
}

/// Drag-and-drop payload: the index of the dragged tab.
#[derive(Clone, Copy)]
struct DraggedTab(usize);

fn tab_ui(ui: &mut egui::Ui, tab: &TabInfo, i: usize, active: bool, count: usize) -> Option<Command> {
    let mut picked = None;
    let visuals = ui.visuals().clone();
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text_color = if active {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    let galley = ui
        .painter()
        .layout_no_wrap(tab.name.clone(), font, text_color);

    let pad = 12.0;
    // Unsaved changes: a dot before the name (drawn, not a glyph: the UI font may lack ●).
    let dot_w = if tab.dirty { DOT_W } else { 0.0 };
    let height = TAB_STRIP_H - 6.0;
    let width = (galley.size().x + 2.0 * pad + dot_w + CLOSE_W + 4.0).clamp(MIN_W, MAX_W);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, height),
        egui::Sense::click_and_drag(),
    );
    let response = response.on_hover_text(
        tab.path
            .as_ref()
            .map_or_else(|| tab.name.clone(), |p| p.display().to_string()),
    );

    // Drag to reorder: the tab is the drag source; dropping on another tab moves it there.
    if response.drag_started() {
        response.dnd_set_drag_payload(DraggedTab(i));
    }
    let hovered_by_drag = response.dnd_hover_payload::<DraggedTab>().is_some_and(|d| d.0 != i);
    if let Some(from) = response.dnd_release_payload::<DraggedTab>()
        && from.0 != i
    {
        picked = Some(Command::MoveTab { from: from.0, to: i });
    }

    // Background: active tab = document background with an accent underline; hovered
    // tab = a faint fill.
    let painter = ui.painter_at(rect);
    let radius = egui::CornerRadius {
        nw: 6,
        ne: 6,
        sw: 0,
        se: 0,
    };
    if active {
        painter.rect_filled(rect, radius, visuals.panel_fill);
        let underline = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 4.0, rect.bottom() - 2.0),
            egui::pos2(rect.right() - 4.0, rect.bottom()),
        );
        painter.rect_filled(underline, 1.0, visuals.widgets.active.bg_fill);
    } else if response.hovered() || hovered_by_drag {
        painter.rect_filled(rect, radius, visuals.widgets.hovered.weak_bg_fill.gamma_multiply(0.6));
    }
    if response.dragged() {
        drag_ghost(ui, rect, &galley, tab.dirty, &visuals);
    }
    if hovered_by_drag {
        let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(2.0, rect.height()));
        painter.rect_filled(bar, 1.0, visuals.widgets.active.bg_fill);
    }

    if tab.dirty {
        let center = egui::pos2(rect.left() + pad + 3.5, rect.center().y);
        painter.circle_filled(center, 3.5, visuals.widgets.active.bg_fill);
    }

    // Label, truncated with an ellipsis when the tab is narrower than the text.
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + pad + dot_w, rect.top()),
        egui::pos2(rect.right() - CLOSE_W - 6.0, rect.bottom()),
    );
    let text_pos = egui::pos2(text_rect.left(), rect.center().y - galley.size().y / 2.0);
    ui.painter()
        .with_clip_rect(text_rect)
        .galley(text_pos, galley.clone(), text_color);
    if galley.size().x > text_rect.width() {
        let fade = egui::Rect::from_min_max(
            egui::pos2(text_rect.right() - 18.0, rect.top()),
            egui::pos2(text_rect.right(), rect.bottom() - 2.0),
        );
        let bg = if active { visuals.panel_fill } else { visuals.extreme_bg_color };
        painter.rect_filled(fade, 0.0, bg.gamma_multiply(0.85));
        painter.text(
            egui::pos2(text_rect.right() - 2.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            "…",
            egui::TextStyle::Button.resolve(ui.style()),
            text_color,
        );
    }

    // Close button: always on the active tab, on hover for the others.
    let close_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - pad / 2.0 - CLOSE_W / 2.0, rect.center().y),
        egui::vec2(CLOSE_W, CLOSE_W),
    );
    let close = ui.interact(close_rect, response.id.with("close"), egui::Sense::click());
    if active || response.hovered() || close.hovered() {
        if close.hovered() {
            painter.rect_filled(close_rect, 4.0, visuals.widgets.hovered.bg_fill);
        }
        let c = close_rect.center();
        let d = 3.5;
        let stroke = egui::Stroke::new(1.4, text_color);
        painter.line_segment([c + egui::vec2(-d, -d), c + egui::vec2(d, d)], stroke);
        painter.line_segment([c + egui::vec2(-d, d), c + egui::vec2(d, -d)], stroke);
    }
    if close.on_hover_text("Close (Ctrl+W)").clicked() || response.middle_clicked() {
        picked = Some(Command::CloseTabAt(i));
    } else if response.clicked() && !active {
        picked = Some(Command::SwitchTab(i));
    }

    response.context_menu(|ui| {
        ui.set_min_width(220.0);
        let has_path = tab.path.is_some();
        if ui.add_enabled(has_path, egui::Button::new("Copy Location")).clicked() {
            picked = Some(Command::CopyPath(i));
            ui.close();
        }
        if ui
            .add_enabled(has_path, egui::Button::new("Open Containing Folder"))
            .clicked()
        {
            picked = Some(Command::RevealInFileManager(i));
            ui.close();
        }
        ui.separator();
        if ui.button("Close").clicked() {
            picked = Some(Command::CloseTabAt(i));
            ui.close();
        }
        if ui.add_enabled(count > 1, egui::Button::new("Close Others")).clicked() {
            picked = Some(Command::CloseOtherTabs(i));
            ui.close();
        }
        if ui.button("Close All").clicked() {
            picked = Some(Command::CloseAllTabs);
            ui.close();
        }
    });
    picked
}

/// A translucent copy of the dragged tab following the pointer, kept at the offset where
/// the tab was grabbed.
fn drag_ghost(
    ui: &egui::Ui,
    rect: egui::Rect,
    galley: &std::sync::Arc<egui::Galley>,
    dirty: bool,
    visuals: &egui::Visuals,
) {
    let (Some(pointer), Some(origin)) = ui.input(|i| (i.pointer.hover_pos(), i.pointer.press_origin()))
    else {
        return;
    };
    let ghost = rect.translate(pointer - origin);
    let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("writ-tab-ghost"));
    let painter = ui.ctx().layer_painter(layer);
    let alpha = 0.75;
    painter.rect_filled(ghost, 6.0, visuals.panel_fill.gamma_multiply(alpha));
    painter.rect_stroke(
        ghost,
        6.0,
        egui::Stroke::new(1.0, visuals.widgets.active.bg_fill.gamma_multiply(alpha)),
        egui::StrokeKind::Inside,
    );
    let pad = 12.0;
    let mut x = ghost.left() + pad;
    if dirty {
        painter.circle_filled(
            egui::pos2(x + 3.5, ghost.center().y),
            3.5,
            visuals.widgets.active.bg_fill.gamma_multiply(alpha),
        );
        x += DOT_W;
    }
    let text_clip = egui::Rect::from_min_max(
        egui::pos2(x, ghost.top()),
        egui::pos2(ghost.right() - pad, ghost.bottom()),
    );
    painter.with_clip_rect(text_clip).galley_with_override_text_color(
        egui::pos2(x, ghost.center().y - galley.size().y / 2.0),
        galley.clone(),
        visuals.strong_text_color().gamma_multiply(alpha),
    );
}
