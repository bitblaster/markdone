use vello::peniko::Color;

use crate::highlight::Highlighter;

/// `a` blended toward `b` by `t` (0 = `a`, 1 = `b`), per sRGB channel.
fn mix(a: Color, b: Color, t: f32) -> Color {
    let (a, b) = (a.to_rgba8(), b.to_rgba8());
    let ch = |x: u8, y: u8| {
        (f32::from(x) + (f32::from(y) - f32::from(x)) * t.clamp(0.0, 1.0)).round() as u8
    };
    Color::from_rgba8(ch(a.r, b.r), ch(a.g, b.g), ch(a.b, b.b), 0xFF)
}

/// WCAG relative luminance of an sRGB color.
fn luminance(c: Color) -> f32 {
    let c = c.to_rgba8();
    let lin = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG contrast ratio between two colors (1 to 21).
fn contrast(a: Color, b: Color) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Color theme for the editor.
///
/// Provides colors for the background, foreground, selection, and syntax
/// highlighting. Use [`EditorTheme::dracula()`] for the built-in Dracula theme.
#[derive(Clone, Debug)]
pub struct EditorTheme {
    pub background: Color,
    /// Slightly-darker fill for chrome (the status bar) so it reads as a distinct
    /// surface rather than blending into the document background.
    pub surface: Color,
    pub foreground: Color,
    pub selection: Color,
    pub comment: Color,
    pub red: Color,
    pub orange: Color,
    pub yellow: Color,
    pub green: Color,
    pub cyan: Color,
    pub purple: Color,
    pub pink: Color,
}

impl EditorTheme {
    /// The Dracula color theme.
    pub fn dracula() -> Self {
        Self {
            background: Color::from_rgba8(0x28, 0x2A, 0x36, 0xFF),
            surface: Color::from_rgba8(0x21, 0x22, 0x2C, 0xFF),
            foreground: Color::from_rgba8(0xF8, 0xF8, 0xF2, 0xFF),
            selection: Color::from_rgba8(0x44, 0x47, 0x5A, 0xFF),
            comment: Color::from_rgba8(0x62, 0x72, 0xA4, 0xFF),
            red: Color::from_rgba8(0xFF, 0x55, 0x55, 0xFF),
            orange: Color::from_rgba8(0xFF, 0xB8, 0x6C, 0xFF),
            yellow: Color::from_rgba8(0xF1, 0xFA, 0x8C, 0xFF),
            green: Color::from_rgba8(0x50, 0xFA, 0x7B, 0xFF),
            cyan: Color::from_rgba8(0x8B, 0xE9, 0xFD, 0xFF),
            purple: Color::from_rgba8(0xBD, 0x93, 0xF9, 0xFF),
            pink: Color::from_rgba8(0xFF, 0x79, 0xC6, 0xFF),
        }
    }

    /// The Nord color theme (arctic dark): keywords blue, types teal, strings amber.
    pub fn nord() -> Self {
        Self {
            background: Color::from_rgba8(0x2E, 0x34, 0x40, 0xFF),
            surface: Color::from_rgba8(0x2B, 0x30, 0x3B, 0xFF),
            foreground: Color::from_rgba8(0xD8, 0xDE, 0xE9, 0xFF),
            selection: Color::from_rgba8(0x43, 0x4C, 0x5E, 0xFF),
            comment: Color::from_rgba8(0x61, 0x6E, 0x88, 0xFF),
            red: Color::from_rgba8(0xBF, 0x61, 0x6A, 0xFF),
            orange: Color::from_rgba8(0xD0, 0x87, 0x70, 0xFF),
            yellow: Color::from_rgba8(0xEB, 0xCB, 0x8B, 0xFF),
            green: Color::from_rgba8(0xA3, 0xBE, 0x8C, 0xFF),
            cyan: Color::from_rgba8(0x8F, 0xBC, 0xBB, 0xFF),
            purple: Color::from_rgba8(0xB4, 0x8E, 0xAD, 0xFF),
            pink: Color::from_rgba8(0x81, 0xA1, 0xC1, 0xFF),
        }
    }

    /// The Solarized Light color theme: keywords magenta, types cyan, strings amber.
    pub fn solarized_light() -> Self {
        Self {
            background: Color::from_rgba8(0xFD, 0xF6, 0xE3, 0xFF),
            surface: Color::from_rgba8(0xEE, 0xE8, 0xD5, 0xFF),
            foreground: Color::from_rgba8(0x65, 0x7B, 0x83, 0xFF),
            selection: Color::from_rgba8(0xD9, 0xD2, 0xBE, 0xFF),
            comment: Color::from_rgba8(0x93, 0xA1, 0xA1, 0xFF),
            red: Color::from_rgba8(0xDC, 0x32, 0x2F, 0xFF),
            orange: Color::from_rgba8(0xCB, 0x4B, 0x16, 0xFF),
            yellow: Color::from_rgba8(0xB5, 0x89, 0x00, 0xFF),
            green: Color::from_rgba8(0x85, 0x99, 0x00, 0xFF),
            cyan: Color::from_rgba8(0x2A, 0xA1, 0x98, 0xFF),
            purple: Color::from_rgba8(0x6C, 0x71, 0xC4, 0xFF),
            pink: Color::from_rgba8(0xD3, 0x36, 0x82, 0xFF),
        }
    }

    /// The Solarized Dark color theme (companion to [`solarized_light`](Self::solarized_light)).
    pub fn solarized_dark() -> Self {
        Self {
            background: Color::from_rgba8(0x00, 0x2B, 0x36, 0xFF),
            surface: Color::from_rgba8(0x07, 0x36, 0x42, 0xFF),
            foreground: Color::from_rgba8(0x83, 0x94, 0x96, 0xFF),
            selection: Color::from_rgba8(0x0A, 0x4B, 0x5C, 0xFF),
            comment: Color::from_rgba8(0x58, 0x6E, 0x75, 0xFF),
            red: Color::from_rgba8(0xDC, 0x32, 0x2F, 0xFF),
            orange: Color::from_rgba8(0xCB, 0x4B, 0x16, 0xFF),
            yellow: Color::from_rgba8(0xB5, 0x89, 0x00, 0xFF),
            green: Color::from_rgba8(0x85, 0x99, 0x00, 0xFF),
            cyan: Color::from_rgba8(0x2A, 0xA1, 0x98, 0xFF),
            purple: Color::from_rgba8(0x6C, 0x71, 0xC4, 0xFF),
            pink: Color::from_rgba8(0xD3, 0x36, 0x82, 0xFF),
        }
    }

    /// The Gruvbox Dark color theme: keywords red, types aqua, strings green.
    pub fn gruvbox_dark() -> Self {
        Self {
            background: Color::from_rgba8(0x28, 0x28, 0x28, 0xFF),
            surface: Color::from_rgba8(0x1D, 0x20, 0x21, 0xFF),
            foreground: Color::from_rgba8(0xEB, 0xDB, 0xB2, 0xFF),
            selection: Color::from_rgba8(0x50, 0x49, 0x45, 0xFF),
            comment: Color::from_rgba8(0x92, 0x83, 0x74, 0xFF),
            red: Color::from_rgba8(0xFB, 0x49, 0x34, 0xFF),
            orange: Color::from_rgba8(0xFE, 0x80, 0x19, 0xFF),
            yellow: Color::from_rgba8(0xFA, 0xBD, 0x2F, 0xFF),
            green: Color::from_rgba8(0xB8, 0xBB, 0x26, 0xFF),
            cyan: Color::from_rgba8(0x8E, 0xC0, 0x7C, 0xFF),
            purple: Color::from_rgba8(0xD3, 0x86, 0x9B, 0xFF),
            pink: Color::from_rgba8(0xFB, 0x49, 0x34, 0xFF),
        }
    }

    /// The Tokyo Night color theme (dark): keywords magenta, types cyan.
    pub fn tokyo_night() -> Self {
        Self {
            background: Color::from_rgba8(0x1A, 0x1B, 0x26, 0xFF),
            surface: Color::from_rgba8(0x16, 0x16, 0x1E, 0xFF),
            foreground: Color::from_rgba8(0xC0, 0xCA, 0xF5, 0xFF),
            selection: Color::from_rgba8(0x33, 0x46, 0x7C, 0xFF),
            comment: Color::from_rgba8(0x56, 0x5F, 0x89, 0xFF),
            red: Color::from_rgba8(0xF7, 0x76, 0x8E, 0xFF),
            orange: Color::from_rgba8(0xFF, 0x9E, 0x64, 0xFF),
            yellow: Color::from_rgba8(0xE0, 0xAF, 0x68, 0xFF),
            green: Color::from_rgba8(0x9E, 0xCE, 0x6A, 0xFF),
            cyan: Color::from_rgba8(0x7D, 0xCF, 0xFF, 0xFF),
            purple: Color::from_rgba8(0xBB, 0x9A, 0xF7, 0xFF),
            pink: Color::from_rgba8(0xBB, 0x9A, 0xF7, 0xFF),
        }
    }

    /// The Catppuccin Mocha color theme (dark): keywords mauve, types teal.
    pub fn catppuccin_mocha() -> Self {
        Self {
            background: Color::from_rgba8(0x1E, 0x1E, 0x2E, 0xFF),
            surface: Color::from_rgba8(0x18, 0x18, 0x25, 0xFF),
            foreground: Color::from_rgba8(0xCD, 0xD6, 0xF4, 0xFF),
            selection: Color::from_rgba8(0x45, 0x47, 0x5A, 0xFF),
            comment: Color::from_rgba8(0x93, 0x99, 0xB2, 0xFF),
            red: Color::from_rgba8(0xF3, 0x8B, 0xA8, 0xFF),
            orange: Color::from_rgba8(0xFA, 0xB3, 0x87, 0xFF),
            yellow: Color::from_rgba8(0xF9, 0xE2, 0xAF, 0xFF),
            green: Color::from_rgba8(0xA6, 0xE3, 0xA1, 0xFF),
            cyan: Color::from_rgba8(0x94, 0xE2, 0xD5, 0xFF),
            purple: Color::from_rgba8(0xCB, 0xA6, 0xF7, 0xFF),
            pink: Color::from_rgba8(0xCB, 0xA6, 0xF7, 0xFF),
        }
    }

    /// The Catppuccin Latte color theme (light): keywords mauve, types teal.
    pub fn catppuccin_latte() -> Self {
        Self {
            background: Color::from_rgba8(0xEF, 0xF1, 0xF5, 0xFF),
            surface: Color::from_rgba8(0xE6, 0xE9, 0xEF, 0xFF),
            foreground: Color::from_rgba8(0x4C, 0x4F, 0x69, 0xFF),
            selection: Color::from_rgba8(0xBC, 0xC0, 0xCC, 0xFF),
            comment: Color::from_rgba8(0x8C, 0x8F, 0xA1, 0xFF),
            red: Color::from_rgba8(0xD2, 0x0F, 0x39, 0xFF),
            orange: Color::from_rgba8(0xFE, 0x64, 0x0B, 0xFF),
            yellow: Color::from_rgba8(0xDF, 0x8E, 0x1D, 0xFF),
            green: Color::from_rgba8(0x40, 0xA0, 0x2B, 0xFF),
            cyan: Color::from_rgba8(0x17, 0x92, 0x99, 0xFF),
            purple: Color::from_rgba8(0x88, 0x39, 0xEF, 0xFF),
            pink: Color::from_rgba8(0x88, 0x39, 0xEF, 0xFF),
        }
    }

    /// The Gruvbox Light color theme (companion to [`gruvbox_dark`](Self::gruvbox_dark)).
    pub fn gruvbox_light() -> Self {
        Self {
            background: Color::from_rgba8(0xFB, 0xF1, 0xC7, 0xFF),
            surface: Color::from_rgba8(0xF2, 0xE5, 0xBC, 0xFF),
            foreground: Color::from_rgba8(0x3C, 0x38, 0x36, 0xFF),
            selection: Color::from_rgba8(0xD5, 0xC4, 0xA1, 0xFF),
            comment: Color::from_rgba8(0x92, 0x83, 0x74, 0xFF),
            red: Color::from_rgba8(0x9D, 0x00, 0x06, 0xFF),
            orange: Color::from_rgba8(0xAF, 0x3A, 0x03, 0xFF),
            yellow: Color::from_rgba8(0xB5, 0x76, 0x14, 0xFF),
            green: Color::from_rgba8(0x79, 0x74, 0x0E, 0xFF),
            cyan: Color::from_rgba8(0x42, 0x7B, 0x58, 0xFF),
            purple: Color::from_rgba8(0x8F, 0x3F, 0x71, 0xFF),
            pink: Color::from_rgba8(0x9D, 0x00, 0x06, 0xFF),
        }
    }

    /// The Tokyo Night Day color theme (companion to [`tokyo_night`](Self::tokyo_night)).
    pub fn tokyo_night_day() -> Self {
        Self {
            background: Color::from_rgba8(0xE1, 0xE2, 0xE7, 0xFF),
            surface: Color::from_rgba8(0xD0, 0xD5, 0xE3, 0xFF),
            foreground: Color::from_rgba8(0x37, 0x60, 0xBF, 0xFF),
            selection: Color::from_rgba8(0xB7, 0xC1, 0xE3, 0xFF),
            comment: Color::from_rgba8(0x84, 0x8C, 0xB5, 0xFF),
            red: Color::from_rgba8(0xF5, 0x2A, 0x65, 0xFF),
            orange: Color::from_rgba8(0xB1, 0x5C, 0x00, 0xFF),
            yellow: Color::from_rgba8(0x8C, 0x6C, 0x3E, 0xFF),
            green: Color::from_rgba8(0x58, 0x75, 0x39, 0xFF),
            cyan: Color::from_rgba8(0x00, 0x71, 0x97, 0xFF),
            purple: Color::from_rgba8(0x98, 0x54, 0xF1, 0xFF),
            pink: Color::from_rgba8(0x98, 0x54, 0xF1, 0xFF),
        }
    }

    /// The GitHub Light color theme (Primer colors): white page, gray chrome.
    pub fn github_light() -> Self {
        Self {
            background: Color::from_rgba8(0xFF, 0xFF, 0xFF, 0xFF),
            surface: Color::from_rgba8(0xF6, 0xF8, 0xFA, 0xFF),
            foreground: Color::from_rgba8(0x1F, 0x23, 0x28, 0xFF),
            selection: Color::from_rgba8(0xAD, 0xD6, 0xFF, 0xFF),
            comment: Color::from_rgba8(0x6E, 0x77, 0x81, 0xFF),
            red: Color::from_rgba8(0xCF, 0x22, 0x2E, 0xFF),
            orange: Color::from_rgba8(0xBC, 0x4C, 0x00, 0xFF),
            yellow: Color::from_rgba8(0x9A, 0x67, 0x00, 0xFF),
            green: Color::from_rgba8(0x11, 0x63, 0x29, 0xFF),
            cyan: Color::from_rgba8(0x05, 0x50, 0xAE, 0xFF),
            purple: Color::from_rgba8(0x82, 0x50, 0xDF, 0xFF),
            pink: Color::from_rgba8(0xBF, 0x39, 0x89, 0xFF),
        }
    }

    /// The One Light color theme (Atom): keywords purple, functions blue.
    pub fn one_light() -> Self {
        Self {
            background: Color::from_rgba8(0xFA, 0xFA, 0xFA, 0xFF),
            surface: Color::from_rgba8(0xF0, 0xF0, 0xF0, 0xFF),
            foreground: Color::from_rgba8(0x38, 0x3A, 0x42, 0xFF),
            selection: Color::from_rgba8(0xD4, 0xD5, 0xDA, 0xFF),
            comment: Color::from_rgba8(0xA0, 0xA1, 0xA7, 0xFF),
            red: Color::from_rgba8(0xE4, 0x56, 0x49, 0xFF),
            orange: Color::from_rgba8(0x98, 0x68, 0x01, 0xFF),
            yellow: Color::from_rgba8(0xC1, 0x84, 0x01, 0xFF),
            green: Color::from_rgba8(0x50, 0xA1, 0x4F, 0xFF),
            cyan: Color::from_rgba8(0x01, 0x84, 0xBC, 0xFF),
            purple: Color::from_rgba8(0xA6, 0x26, 0xA4, 0xFF),
            pink: Color::from_rgba8(0x40, 0x78, 0xF2, 0xFF),
        }
    }

    /// Config names of the built-in themes.
    pub const BUILTIN_NAMES: &'static [&'static str] = &[
        "dracula",
        "nord",
        "solarized-dark",
        "gruvbox-dark",
        "tokyo-night",
        "catppuccin-mocha",
        "solarized-light",
        "catppuccin-latte",
        "gruvbox-light",
        "tokyo-night-day",
        "github-light",
        "one-light",
    ];

    /// `comment` moved toward the foreground until it reads at WCAG AA contrast (4.5:1)
    /// on both `background` and `surface`: the color of secondary interface text (status
    /// bar, inactive tabs, the outline's lower levels), which must stay legible. Themes
    /// whose `comment` already has the contrast keep it unchanged.
    pub fn muted_text(&self) -> Color {
        let mut t = 0.0f32;
        loop {
            let c = mix(self.comment, self.foreground, t);
            if t >= 1.0 || (contrast(c, self.background) >= 4.5 && contrast(c, self.surface) >= 4.5)
            {
                return c;
            }
            t += 0.05;
        }
    }

    /// Interface labels: halfway between [`Self::muted_text`] and the foreground.
    pub fn label_text(&self) -> Color {
        mix(self.muted_text(), self.foreground, 0.5)
    }

    /// Whether this is a light theme (light background).
    pub fn is_light(&self) -> bool {
        let c = self.background.to_rgba8();
        let luma = 0.299 * f32::from(c.r) + 0.587 * f32::from(c.g) + 0.114 * f32::from(c.b);
        luma > 128.0
    }

    /// A built-in theme by its config name, or `None` if unknown.
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "dracula" => Some(Self::dracula()),
            "nord" => Some(Self::nord()),
            "solarized-light" => Some(Self::solarized_light()),
            "solarized-dark" => Some(Self::solarized_dark()),
            "gruvbox-dark" => Some(Self::gruvbox_dark()),
            "tokyo-night" => Some(Self::tokyo_night()),
            "catppuccin-mocha" => Some(Self::catppuccin_mocha()),
            "catppuccin-latte" => Some(Self::catppuccin_latte()),
            "gruvbox-light" => Some(Self::gruvbox_light()),
            "tokyo-night-day" => Some(Self::tokyo_night_day()),
            "github-light" => Some(Self::github_light()),
            "one-light" => Some(Self::one_light()),
            _ => None,
        }
    }

    pub fn color_for_capture(&self, capture: &str) -> Color {
        // Handle specific sub-captures first
        match capture {
            "variable.special" => return self.purple,
            "variable.parameter" => return self.orange,
            "punctuation.special" => return self.pink,
            "string.escape" => return self.pink,
            "lifetime" => return self.pink,
            _ => {}
        }

        let base = capture.split('.').next().unwrap_or(capture);

        match base {
            "keyword" => self.pink,
            "function" => self.green,
            "type" => self.cyan,
            "string" => self.yellow,
            "number" | "boolean" => self.purple,
            "comment" => self.comment,
            "constant" => self.purple,
            "operator" => self.pink,
            "attribute" => self.pink,
            "property" => self.cyan,
            "punctuation" => self.foreground,
            _ => self.foreground,
        }
    }

    pub fn color_for_highlight(&self, highlight_id: usize) -> Color {
        self.color_for_capture(Highlighter::capture_name(highlight_id))
    }
}

impl Default for EditorTheme {
    fn default() -> Self {
        Self::dracula()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Secondary interface text is legible (WCAG AA, or as legible as the foreground) on
    /// every built-in theme, and a
    /// `comment` that already is stays as it is.
    #[test]
    fn muted_text_is_legible() {
        for name in EditorTheme::BUILTIN_NAMES {
            let t = EditorTheme::by_name(name).unwrap();
            let m = t.muted_text();
            // Where even the foreground falls short (Solarized's surface), it is the best.
            let legible = contrast(m, t.background) >= 4.5 && contrast(m, t.surface) >= 4.5;
            assert!(legible || m.to_rgba8() == t.foreground.to_rgba8(), "{name}");
        }
        let latte = EditorTheme::catppuccin_latte();
        assert!(
            contrast(latte.comment, latte.background) < 4.5,
            "Latte's comment is faint"
        );
        let mut t = EditorTheme::catppuccin_latte();
        t.comment = t.foreground;
        assert_eq!(t.muted_text().to_rgba8(), t.foreground.to_rgba8());
    }

    /// The built-in list resolves by name, and light/dark detection matches the presets.
    #[test]
    fn builtin_names_resolve_and_light_themes_are_detected() {
        for name in EditorTheme::BUILTIN_NAMES {
            let theme = EditorTheme::by_name(name).expect(name);
            let light =
                name.ends_with("-light") || matches!(*name, "catppuccin-latte" | "tokyo-night-day");
            assert_eq!(theme.is_light(), light, "{name}");
        }
    }

    /// Every built-in preset resolves, and no two share a background — a cheap guard
    /// against a copy-paste slip leaving two presets identical.
    #[test]
    fn presets_resolve_and_are_distinct() {
        let names = [
            "dracula",
            "nord",
            "solarized-light",
            "solarized-dark",
            "gruvbox-dark",
            "tokyo-night",
            "catppuccin-mocha",
            "catppuccin-latte",
            "gruvbox-light",
            "tokyo-night-day",
            "github-light",
            "one-light",
        ];
        assert_eq!(names.len(), EditorTheme::BUILTIN_NAMES.len());
        let light = names
            .iter()
            .filter(|n| EditorTheme::by_name(n).unwrap().is_light())
            .count();
        assert_eq!(light, 6, "as many light presets as dark ones");
        let mut backgrounds = Vec::new();
        for name in names {
            let theme = EditorTheme::by_name(name)
                .unwrap_or_else(|| panic!("preset '{name}' should resolve"));
            let bg = theme.background.to_rgba8();
            let key = (bg.r, bg.g, bg.b);
            assert!(
                !backgrounds.contains(&key),
                "preset '{name}' shares a background with another preset"
            );
            backgrounds.push(key);
        }
        assert!(EditorTheme::by_name("does-not-exist").is_none());
    }
}
