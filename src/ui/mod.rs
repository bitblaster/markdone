//! Application chrome (menu bar, menus, dialogs) drawn with egui on top of the Vello
//! document.
//!
//! The document keeps its own renderer; egui only paints the chrome. Each frame Vello
//! renders the document into the swapchain image and egui draws over it in a second
//! pass (`LoadOp::Load`), on the same wgpu device.
//!
//! **Coordinates.** The menu bar claims a strip of [`MENU_BAR_H`] logical px at the
//! top. The shell does not move the document's origin: it shifts the whole document
//! scene down by [`UiLayer::top_inset`] when rendering, and shifts pointer positions up
//! by the same amount when it receives them. The document code keeps working in its own
//! coordinates, with the origin at the top of the document area.
//!
//! **Input.** Every window event goes to egui first ([`UiLayer::on_window_event`]). If
//! egui consumes it (pointer over the bar or an open menu, focus in a text field), the
//! editor does not receive it.

use std::time::{Duration, Instant};

use egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
use vello::wgpu;
use winit::event::WindowEvent;
use winit::window::Window;

use crate::editor::EditorTheme;

pub mod dialogs;
pub mod menu;
pub mod preferences;
pub mod tabs;
pub mod toc;

/// Height of the top menu bar, in logical px.
pub const MENU_BAR_H: f32 = 30.0;

/// Geometry and tessellated output of one egui frame, ready to paint.
pub struct PreparedFrame {
    primitives: Vec<egui::ClippedPrimitive>,
    textures_delta: egui::TexturesDelta,
    pixels_per_point: f32,
}

pub struct UiLayer {
    ctx: egui::Context,
    winit: egui_winit::State,
    renderer: Renderer,
    /// Whether egui wanted the pointer, or had a menu open, at the end of the last frame.
    /// While true, pointer moves repaint (for hover highlights); otherwise a move over
    /// the document does not cost an egui frame.
    active: bool,
    /// Whether egui asked for IME at the last frame (a text field had focus). Used to
    /// give IME back to the editor when the field loses focus: egui-winit disables it.
    egui_ime: bool,
    /// When egui wants its next frame (animations, tooltip delays). `None` = no request.
    repaint_at: Option<Instant>,
    /// The pointer left egui's areas since the last [`Self::take_cursor_released`]: the
    /// editor must re-apply its own cursor even if its cached icon did not change.
    cursor_released: bool,
    /// The pointer is over egui (bars, a menu, a dialog) as of its last move.
    pointer_over: bool,
    /// A menu or popup was open at the end of the last frame.
    popup_open: bool,
    /// The table of contents panel (points), when shown: egui owns the pointer there.
    side_panel: Option<egui::Rect>,
    /// The cursor the editor shows over the document, reported to egui-winit whenever
    /// egui is inactive (see [`Self::run`]).
    editor_cursor: egui::CursorIcon,
}

impl UiLayer {
    pub fn new(
        window: &Window,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        theme: &EditorTheme,
        ui_font: Option<(Vec<u8>, u32)>,
        fallback_fonts: Vec<(Vec<u8>, u32)>,
    ) -> Self {
        let ctx = new_context();
        ctx.set_fonts(ui_fonts(ui_font, fallback_fonts));
        let max_texture_side = device.limits().max_texture_dimension_2d as usize;
        let winit = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            Some(max_texture_side),
        );
        let renderer = Renderer::new(device, format, RendererOptions::default());
        Self::apply_theme(&ctx, theme);
        Self {
            ctx,
            winit,
            renderer,
            active: false,
            egui_ime: false,
            repaint_at: None,
            cursor_released: false,
            pointer_over: false,
            popup_open: false,
            side_panel: None,
            editor_cursor: egui::CursorIcon::Default,
        }
    }

    /// Restyle the chrome for a new editor theme (light or dark).
    pub fn set_theme(&mut self, window: &Window, theme: &EditorTheme) {
        Self::apply_theme(&self.ctx, theme);
        window.request_redraw();
    }

    fn apply_theme(ctx: &egui::Context, theme: &EditorTheme) {
        let mode = if theme.is_light() {
            egui::Theme::Light
        } else {
            egui::Theme::Dark
        };
        ctx.set_theme(mode);
        ctx.set_style_of(mode, style_from_theme(theme));
    }

    /// The menu bar height in device px for the given scale factor.
    pub fn top_inset(scale: f32) -> f32 {
        ((MENU_BAR_H + tabs::TAB_STRIP_H) * scale).round()
    }

    /// Feed a window event to egui. Returns `true` when egui consumed it and the editor
    /// must not see it. Requests a redraw when egui needs a new frame.
    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        let response = self.winit.on_window_event(window, event);
        // Pointer moves over egui belong to egui: the editor must not react to them (its
        // hover would set the text cursor under an open menu). Decided by position: the
        // bars at the top, or any egui area (menu, popup, dialog) drawn last frame.
        if let WindowEvent::CursorMoved { position, .. } = event {
            let ppp = self.ctx.pixels_per_point();
            let pos = egui::pos2(position.x as f32 / ppp, position.y as f32 / ppp);
            let over = (position.y as f32) < Self::top_inset(window.scale_factor() as f32)
                || area_at(&self.ctx, pos)
                || self.side_panel.is_some_and(|r| r.contains(pos));
            if !over && self.pointer_over {
                self.cursor_released = true;
            }
            self.pointer_over = over;
            if over {
                window.request_redraw();
                return true;
            }
        }
        // Buttons and wheel over egui are egui's too (egui itself does not claim them
        // over plain background content such as the table of contents).
        if self.pointer_over
            && matches!(
                event,
                WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. }
            )
        {
            window.request_redraw();
            return true;
        }
        if let WindowEvent::CursorLeft { .. } = event {
            self.pointer_over = false;
        }
        let repaint = match event {
            // A move over the document does not concern egui unless a menu is open or
            // the pointer is in (or just left) the bar: no frame for that.
            WindowEvent::CursorMoved { position, .. } => {
                self.active || (position.y as f32) < Self::top_inset(window.scale_factor() as f32)
            }
            WindowEvent::CursorLeft { .. } => self.active,
            _ => response.repaint && (self.active || response.consumed),
        };
        if repaint {
            window.request_redraw();
        }
        response.consumed
    }

    /// Set the area of the table of contents panel (points), `None` when closed.
    pub fn set_side_panel(&mut self, rect: Option<egui::Rect>) {
        self.side_panel = rect;
    }

    /// The cursor the editor shows over the document (the window has it already).
    pub fn set_editor_cursor(&mut self, icon: winit::window::CursorIcon) {
        use winit::window::CursorIcon as W;
        self.editor_cursor = match icon {
            W::Text => egui::CursorIcon::Text,
            W::Pointer => egui::CursorIcon::PointingHand,
            _ => egui::CursorIcon::Default,
        };
    }

    /// Whether a menu or popup is open (a click outside it only closes it).
    pub fn menu_open(&self) -> bool {
        self.popup_open
    }

    /// Whether egui is using the pointer or has a menu/popup open. A click on the
    /// document while a menu is open only closes the menu.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Run one egui frame: `build` lays out the chrome. `modal` = a modal dialog is shown
    /// this frame (it owns the keyboard, and the pointer over the whole window). Applies
    /// the platform output (cursor, IME) and returns the tessellated frame for
    /// [`Self::paint`].
    pub fn run(
        &mut self,
        window: &Window,
        modal: bool,
        mut build: impl FnMut(&mut egui::Ui),
    ) -> PreparedFrame {
        let raw_input = self.winit.take_egui_input(window);
        let output = self.ctx.run_ui(raw_input, |ui| build(ui));

        let popup_open = egui::Popup::is_any_open(&self.ctx);
        self.popup_open = popup_open;
        let was_active = self.active;
        // `pointer_over` also covers chrome egui does not count as its own (the table of
        // contents, drawn in the background layer): its hover cursor (the resize grip)
        // must reach the window too.
        self.active = modal
            || popup_open
            || self.pointer_over
            || self.ctx.is_pointer_over_egui()
            || self.ctx.egui_is_using_pointer();
        // A clicked menu button keeps keyboard focus, and egui would then claim every key
        // (Enter re-opens the menu). Release it as soon as no menu is open, unless a text
        // field is being edited or a dialog (whose default button holds focus) is shown.
        if !modal && !popup_open && !self.ctx.text_edit_focused() {
            self.ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }

        // Cursor ownership: egui over its own areas, the editor over the document.
        // egui-winit calls `set_cursor` only when the icon differs from its cache, and it
        // does not see what the editor sets on the window. So over the document report
        // the editor's own icon: egui-winit's cache then follows the window, and when it
        // re-applies its icon (it drops the cache whenever a frame runs with the pointer
        // outside the window) it re-applies the editor's, not a stale arrow.
        let mut platform_output = output.platform_output;
        let wants_ime = platform_output.ime.is_some();
        if !self.active {
            platform_output.cursor_icon = self.editor_cursor;
            self.cursor_released |= was_active;
        }
        self.winit.handle_platform_output(window, platform_output);
        if self.egui_ime && !wants_ime {
            // egui-winit just disabled IME; the editor needs it back.
            window.set_ime_allowed(true);
        }
        self.egui_ime = wants_ime;

        let delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |v| v.repaint_delay);
        self.repaint_at = if delay.is_zero() {
            window.request_redraw();
            None
        } else if delay < Duration::from_secs(60) {
            Some(Instant::now() + delay)
        } else {
            None
        };

        let primitives = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        PreparedFrame {
            primitives,
            textures_delta: output.textures_delta,
            pixels_per_point: output.pixels_per_point,
        }
    }

    /// Whether the pointer left egui since the last call (see `cursor_released`).
    pub fn take_cursor_released(&mut self) -> bool {
        std::mem::take(&mut self.cursor_released)
    }

    /// When egui wants its next frame, for the event loop's `WaitUntil`.
    pub fn repaint_deadline(&self) -> Option<Instant> {
        self.repaint_at
    }

    /// Clear a due repaint deadline; returns `true` if it was due (the caller redraws).
    pub fn take_due(&mut self, now: Instant) -> bool {
        if self.repaint_at.is_some_and(|t| t <= now) {
            self.repaint_at = None;
            true
        } else {
            false
        }
    }

    /// Draw a prepared frame over `view` (already holding the document image).
    pub fn paint(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size: [u32; 2],
        frame: PreparedFrame,
    ) {
        for (id, delta) in &frame.textures_delta.set {
            self.renderer.update_texture(device, queue, *id, delta);
        }
        let screen = ScreenDescriptor {
            size_in_pixels: size,
            pixels_per_point: frame.pixels_per_point,
        };
        let callbacks =
            self.renderer
                .update_buffers(device, queue, encoder, &frame.primitives, &screen);
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut pass, &frame.primitives, &screen);
        }
        debug_assert!(callbacks.is_empty(), "no egui paint callbacks are used");
        for id in &frame.textures_delta.free {
            self.renderer.free_texture(id);
        }
    }
}

/// The egui context for the chrome. egui's own keyboard zoom is off: `Ctrl` `+`/`-`/`0`
/// are the editor's zoom (font size), and egui reacting to them too would scale its
/// points away from the window scale — while the shell places the table of contents, the
/// bars and the pointer test assuming one point is `scale` device px.
fn new_context() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
    ctx
}

/// Whether an egui area (menu, popup, dialog, backdrop) lies under `pos` (points), as of
/// the last frame. The background layer, which holds the bars and spans the whole
/// window, does not count: over the document it is empty.
fn area_at(ctx: &egui::Context, pos: egui::Pos2) -> bool {
    ctx.layer_id_at(pos)
        .is_some_and(|layer| layer.order != egui::Order::Background)
}

fn color32(c: vello::peniko::Color) -> egui::Color32 {
    let rgba = c.to_rgba8();
    egui::Color32::from_rgba_unmultiplied(rgba.r, rgba.g, rgba.b, rgba.a)
}

/// egui style derived from the editor theme, so the chrome matches the document:
/// desktop-sized text, roomy menu entries, rounded menus with a soft shadow.
fn style_from_theme(theme: &EditorTheme) -> egui::Style {
    use egui::{CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, vec2};

    let mut style = egui::Style {
        text_styles: [
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Heading,
                FontId::new(18.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(13.0, FontFamily::Monospace),
            ),
        ]
        .into(),
        ..Default::default()
    };

    let s = &mut style.spacing;
    s.item_spacing = vec2(8.0, 2.0);
    s.button_padding = vec2(10.0, 5.0);
    s.interact_size = vec2(40.0, 26.0);
    // No hover slack around widgets: on a desktop it only blurs edges (the table of
    // contents' resize grip would reach into the document's scrollbar).
    style.interaction.interact_radius = 0.0;
    s.menu_margin = Margin::same(6);
    s.menu_width = 260.0;
    s.menu_spacing = 4.0;
    s.window_margin = Margin::same(14);

    let surface = color32(theme.surface);
    let background = color32(theme.background);
    let fg = color32(theme.foreground);
    let dim = color32(theme.comment);
    let hover = color32(theme.selection);
    let accent = color32(theme.purple);

    let light = theme.is_light();
    // Faint lines and shadows: white on dark themes, black on light ones.
    let faint = |alpha: u8| {
        if light {
            egui::Color32::from_black_alpha(alpha)
        } else {
            egui::Color32::from_white_alpha(alpha)
        }
    };
    let shadow_alpha = if light { 40 } else { 110 };

    let v = &mut style.visuals;
    *v = if light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    v.panel_fill = surface;
    v.window_fill = surface;
    v.extreme_bg_color = background;
    v.faint_bg_color = background;
    v.override_text_color = None;
    v.hyperlink_color = color32(theme.cyan);
    v.selection.bg_fill = hover;
    v.selection.stroke = Stroke::new(1.0, fg);
    v.window_stroke = Stroke::new(1.0, faint(18));
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: egui::Color32::from_black_alpha(shadow_alpha),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: egui::Color32::from_black_alpha(shadow_alpha - 10),
    };
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(5);
        w.fg_stroke = Stroke::new(1.0, fg);
    }
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, dim);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, faint(14));
    // Text fields and other framed widgets: a visible border on both themes.
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, faint(30));
    v.widgets.inactive.bg_fill = background;
    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.hovered.weak_bg_fill = hover;
    v.widgets.hovered.bg_fill = hover;
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.active.weak_bg_fill = hover;
    v.widgets.active.bg_fill = accent;
    v.widgets.open.weak_bg_fill = hover;
    v.widgets.open.bg_fill = hover;
    style
}

/// Menu style: no frames on entries, roomy padding (egui's default menu style shrinks
/// the padding to 2×0 px, which reads as cramped on a desktop).
pub fn menu_entry_style(style: &mut egui::Style) {
    egui::containers::menu::menu_style(style);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
}

/// The current monochrome Noto Emoji (Google Fonts, SIL OFL 1.1, see
/// `assets/fonts/NotoEmoji-OFL.txt`; variable weight, drawn at its default 400). It
/// replaces the older "blob" design egui bundles, and covers newer emoji (🟢 U+1F7E2).
/// egui cannot draw color emoji, so the chrome shows them in the text color.
const NOTO_EMOJI: &[u8] = include_bytes!("../../assets/fonts/NotoEmoji-wght.ttf");
/// Emoji are drawn slightly smaller than the text, as egui does with its own.
const EMOJI_SCALE: f32 = 0.85;

/// egui's fonts for the chrome: the system UI font first for proportional text, egui's
/// own fonts (with the current Noto Emoji), then the system symbol fonts for what those
/// lack. Every fallback font's baseline is aligned with the primary font's.
fn ui_fonts(
    ui_font: Option<(Vec<u8>, u32)>,
    fallbacks: Vec<(Vec<u8>, u32)>,
) -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "NotoEmoji-Regular".to_owned(),
        std::sync::Arc::new(
            egui::FontData::from_static(NOTO_EMOJI).tweak(egui::FontTweak {
                scale: EMOJI_SCALE,
                ..Default::default()
            }),
        ),
    );
    let mut add = |name: String, (data, index): (Vec<u8>, u32)| {
        let mut font = egui::FontData::from_owned(data);
        font.index = index;
        fonts.font_data.insert(name, std::sync::Arc::new(font));
    };
    let fallback_names: Vec<String> = (0..fallbacks.len())
        .map(|i| format!("system-symbols-{i}"))
        .collect();
    for (name, font) in fallback_names.iter().zip(fallbacks) {
        add(name.clone(), font);
    }
    let has_ui_font = ui_font.is_some();
    if let Some(font) = ui_font {
        add("system-ui".to_owned(), font);
    }
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family.clone()).or_default();
        if has_ui_font && family == egui::FontFamily::Proportional {
            list.insert(0, "system-ui".to_owned());
        }
        list.extend(fallback_names.iter().cloned());
    }

    // Baselines: align every font but the families' primaries with the proportional
    // primary (the chrome is proportional text; monospace barely appears).
    let primaries: Vec<String> = [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
        .iter()
        .filter_map(|f| fonts.families.get(f).and_then(|l| l.first().cloned()))
        .collect();
    let primary = primaries
        .first()
        .and_then(|name| fonts.font_data.get(name))
        .and_then(|data| face_metrics(data));
    if let Some(primary) = primary {
        for (name, data) in fonts.font_data.iter_mut() {
            if primaries.contains(name) {
                continue;
            }
            if let Some(face) = face_metrics(data) {
                let mut tweaked = (**data).clone();
                tweaked.tweak.y_offset_factor = baseline_shift(primary, face, tweaked.tweak.scale);
                *data = std::sync::Arc::new(tweaked);
            }
        }
    }
    fonts
}

/// A font's vertical metrics, in em (`descent` negative, as in the font).
#[derive(Clone, Copy, Debug)]
struct VMetrics {
    ascent: f32,
    descent: f32,
    gap: f32,
}

impl VMetrics {
    fn height(self) -> f32 {
        self.ascent - self.descent + self.gap
    }
}

/// The vertical metrics epaint uses for a font (default instance of a variable font).
fn face_metrics(data: &egui::FontData) -> Option<VMetrics> {
    use skrifa::MetadataProvider;
    use skrifa::instance::{LocationRef, Size};

    let font = skrifa::FontRef::from_index(&data.font, data.index).ok()?;
    let m = font.metrics(Size::unscaled(), LocationRef::default());
    let upm = f32::from(m.units_per_em.max(1));
    Some(VMetrics {
        ascent: m.ascent / upm,
        descent: m.descent / upm,
        gap: m.leading / upm,
    })
}

/// The downward shift (fraction of the font size, for `FontTweak::y_offset_factor`)
/// that puts a fallback face's baseline on the primary font's. epaint places a
/// fallback glyph's baseline at `face ascent + (font height − face height) / 2`: it
/// centers the difference in line height, which is off whenever the two fonts split
/// their height differently between ascent and descent (e.g. Noto Sans Symbols2's long
/// descent drew its symbols ~0.17 em too high).
fn baseline_shift(primary: VMetrics, face: VMetrics, scale: f32) -> f32 {
    let baseline = scale * face.ascent + 0.5 * (primary.height() - scale * face.height());
    primary.ascent - baseline
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, popup: bool) {
        frame_with(ctx, popup, None, Vec::new());
    }

    fn frame_with(
        ctx: &egui::Context,
        popup: bool,
        toc_rect: Option<egui::Rect>,
        events: Vec<egui::Event>,
    ) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 800.0),
            )),
            events,
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            egui::Panel::top("bar").exact_size(30.0).show(ui, |ui| {
                ui.label("menu");
            });
            if let Some(rect) = toc_rect {
                let entries = [toc::TocEntry {
                    level: 1,
                    text: "Heading".into(),
                    folded: false,
                }];
                toc::toc_panel(ui, rect, &entries, Some(0), false);
            }
            if popup {
                egui::Area::new(egui::Id::new("popup"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(100.0, 100.0))
                    .show(ui.ctx(), |ui| {
                        ui.allocate_exact_size(egui::vec2(200.0, 200.0), egui::Sense::click());
                    });
            }
        });
    }

    /// `Ctrl` `+` zooms the editor's font only: egui must not rescale its own points too
    /// (regression: the table of contents jumped around on every zoom step, placed in
    /// points computed from the unchanged window scale).
    #[test]
    fn editor_zoom_keys_do_not_zoom_the_chrome() {
        let zoom_in = vec![egui::Event::Key {
            key: egui::Key::Plus,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        }];
        // egui applies the zoom at the end of a pass; a second pass makes it visible.
        let press = |ctx: &egui::Context| {
            frame_with(ctx, false, None, zoom_in.clone());
            frame_with(ctx, false, None, Vec::new());
        };
        // A plain egui context does zoom on this event (so the test exercises it)…
        let plain = egui::Context::default();
        press(&plain);
        assert!(
            plain.zoom_factor() > 1.0,
            "the event should trigger egui's zoom"
        );
        // …ours does not.
        let ctx = new_context();
        press(&ctx);
        assert_eq!(ctx.zoom_factor(), 1.0);
    }

    /// The document area is not "over egui" (regression: the background layer spans the
    /// window, and counting it swallowed every pointer move over the document), while a
    /// popup is.
    #[test]
    fn only_real_areas_count_as_pointer_over_egui() {
        let ctx = egui::Context::default();
        for _ in 0..2 {
            frame(&ctx, true);
        }
        assert!(area_at(&ctx, egui::pos2(150.0, 150.0)), "popup");
        assert!(!area_at(&ctx, egui::pos2(600.0, 500.0)), "document area");
        for _ in 0..2 {
            frame(&ctx, false);
        }
        assert!(!area_at(&ctx, egui::pos2(150.0, 150.0)), "popup closed");
    }

    /// With the table of contents open, egui must not claim the pointer over the
    /// document (regression: the panel shrank the root's free area, so egui took the
    /// mouse wheel everywhere and the document no longer scrolled).
    #[test]
    fn toc_panel_leaves_the_document_to_the_editor() {
        let ctx = egui::Context::default();
        let toc = egui::Rect::from_min_max(egui::pos2(760.0, 30.0), egui::pos2(1000.0, 780.0));
        let over_doc = vec![egui::Event::PointerMoved(egui::pos2(300.0, 400.0))];
        for _ in 0..3 {
            frame_with(&ctx, false, Some(toc), over_doc.clone());
        }
        assert!(!ctx.is_pointer_over_egui(), "document area claimed by egui");
        assert!(!ctx.egui_wants_pointer_input());
    }

    /// Symbols missing from the bundled fonts come from the installed symbol fonts
    /// instead of rendering as boxes. Skipped where no such font is installed.
    #[test]
    fn baseline_shift_aligns_fallback_baselines() {
        let noto_sans = VMetrics {
            ascent: 1.069,
            descent: -0.293,
            gap: 0.0,
        };
        let symbols2 = VMetrics {
            ascent: 1.069,
            descent: -0.630,
            gap: 0.0,
        };
        assert!(baseline_shift(noto_sans, noto_sans, 1.0).abs() < 1e-6);
        // Symbols2 would sit (1.699 − 1.362) / 2 ≈ 0.17 em too high: shift it down.
        assert!((baseline_shift(noto_sans, symbols2, 1.0) - 0.1685).abs() < 1e-3);
        // The shifted baseline lands on the primary's, at any scale.
        for scale in [0.85, 1.0, 1.2] {
            let shift = baseline_shift(noto_sans, symbols2, scale);
            let baseline = scale * symbols2.ascent
                + 0.5 * (noto_sans.height() - scale * symbols2.height())
                + shift;
            assert!((baseline - noto_sans.ascent).abs() < 1e-6);
        }
    }

    /// The bundled Noto Emoji is the one egui uses for emoji, and it covers the emoji
    /// found in headings (round faces, not the older "blob" design). Checked on the
    /// font's own character map: egui's `has_glyph` reports false for any character
    /// served by the face that also serves its replacement character (an emoji font).
    #[test]
    fn bundled_noto_emoji_covers_newer_emoji() {
        use skrifa::MetadataProvider;

        let fonts = ui_fonts(None, Vec::new());
        let emoji = &fonts.font_data["NotoEmoji-Regular"];
        assert_eq!(emoji.font.as_ref(), NOTO_EMOJI);
        assert_eq!(emoji.tweak.scale, EMOJI_SCALE);
        let font = skrifa::FontRef::new(NOTO_EMOJI).unwrap();
        let charmap = font.charmap();
        for c in "😅🟢✅🚀⚠".chars() {
            assert!(charmap.map(c).is_some(), "{c} U+{:X}", c as u32);
        }
        // Ahead of egui's other emoji font, so it is the one picked.
        let list = &fonts.families[&egui::FontFamily::Proportional];
        let pos = |n: &str| list.iter().position(|x| x == n).unwrap();
        assert!(pos("NotoEmoji-Regular") < pos("emoji-icon-font"));
    }

    #[test]
    fn system_symbol_fonts_cover_newer_emoji() {
        let fallbacks = crate::text_engine::TextEngine::new().ui_fallback_fonts();
        if fallbacks.is_empty() {
            eprintln!("no system symbol font installed; skipping");
            return;
        }
        let font = egui::FontId::proportional(14.0);
        // A symbol outside every emoji font (Noto Sans Symbols2: ⯑ U+2BD1).
        let bare = egui::Context::default();
        bare.set_fonts(ui_fonts(None, Vec::new()));
        let with = egui::Context::default();
        with.set_fonts(ui_fonts(None, fallbacks));
        for ctx in [&bare, &with] {
            frame(ctx, false);
        }
        assert!(
            !bare.fonts_mut(|f| f.has_glyph(&font, '⯑')),
            "egui alone lacks it"
        );
        assert!(with.fonts_mut(|f| f.has_glyph(&font, '⯑')));
    }
}
