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
pub mod tabs;

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
}

impl UiLayer {
    pub fn new(
        window: &Window,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        theme: &EditorTheme,
        ui_font: Option<(Vec<u8>, u32)>,
    ) -> Self {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::Theme::Dark);
        ctx.set_style_of(egui::Theme::Dark, style_from_theme(theme));
        if let Some((data, index)) = ui_font {
            ctx.set_fonts(fonts_with_ui_font(data, index));
        }
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
        Self {
            ctx,
            winit,
            renderer,
            active: false,
            egui_ime: false,
            repaint_at: None,
            cursor_released: false,
        }
    }

    /// The menu bar height in device px for the given scale factor.
    pub fn top_inset(scale: f32) -> f32 {
        ((MENU_BAR_H + tabs::TAB_STRIP_H) * scale).round()
    }

    /// Feed a window event to egui. Returns `true` when egui consumed it and the editor
    /// must not see it. Requests a redraw when egui needs a new frame.
    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        let response = self.winit.on_window_event(window, event);
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
        let was_active = self.active;
        self.active = modal
            || popup_open
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
        // egui-winit only calls `set_cursor` when its icon differs from its cache, and
        // it does not see what the editor sets. So on entering egui reset the window
        // cursor to the default (matching egui's cache), and over the document report
        // the default icon so egui changes nothing.
        let mut platform_output = output.platform_output;
        let wants_ime = platform_output.ime.is_some();
        if self.active && !was_active {
            window.set_cursor(winit::window::CursorIcon::Default);
        }
        if !self.active {
            platform_output.cursor_icon = egui::CursorIcon::Default;
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

fn color32(c: vello::peniko::Color) -> egui::Color32 {
    let rgba = c.to_rgba8();
    egui::Color32::from_rgba_unmultiplied(rgba.r, rgba.g, rgba.b, rgba.a)
}

/// egui style derived from the editor theme, so the chrome matches the document:
/// desktop-sized text, roomy menu entries, rounded menus with a soft shadow.
fn style_from_theme(theme: &EditorTheme) -> egui::Style {
    use egui::{CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, vec2};

    let mut style = egui::Style::default();
    style.text_styles = [
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
    .into();

    let s = &mut style.spacing;
    s.item_spacing = vec2(8.0, 2.0);
    s.button_padding = vec2(10.0, 5.0);
    s.interact_size = vec2(40.0, 26.0);
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

    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = surface;
    v.window_fill = surface;
    v.extreme_bg_color = background;
    v.faint_bg_color = background;
    v.override_text_color = None;
    v.hyperlink_color = color32(theme.cyan);
    v.selection.bg_fill = hover;
    v.selection.stroke = Stroke::new(1.0, fg);
    v.window_stroke = Stroke::new(1.0, egui::Color32::from_white_alpha(18));
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: egui::Color32::from_black_alpha(110),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: egui::Color32::from_black_alpha(100),
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
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, egui::Color32::from_white_alpha(14));
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

/// egui's default fonts with the system UI font first for proportional text.
fn fonts_with_ui_font(data: Vec<u8>, index: u32) -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    let mut font = egui::FontData::from_owned(data);
    font.index = index;
    fonts
        .font_data
        .insert("system-ui".to_owned(), std::sync::Arc::new(font));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "system-ui".to_owned());
    fonts
}
