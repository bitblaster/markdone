#[cfg(feature = "app")]
use std::collections::HashMap;
#[cfg(feature = "app")]
use std::path::{Path, PathBuf};

#[cfg(feature = "app")]
use clap::Parser;
#[cfg(feature = "app")]
use serde::Deserialize;
#[cfg(feature = "app")]
use vello::peniko::Color;

#[cfg(feature = "app")]
use crate::consts::FONT_SIZE;
#[cfg(feature = "app")]
use crate::editor::EditorTheme;

/// Environment variable name for GitHub token (shared between clap and tests)
pub const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";

#[cfg(feature = "app")]
#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Config {
    /// Document to open (the first tab). Without any file, the previous session is
    /// restored.
    #[arg(short, long)]
    pub file: Option<PathBuf>,

    /// More documents to open, one tab each (after `--file`, if given).
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,

    #[arg(long)]
    pub demo: bool,

    /// Start a separate instance instead of opening the files in the running one.
    #[arg(long)]
    pub new_instance: bool,

    /// Save file automatically on every edit (useful for GhostText integration)
    #[arg(long)]
    pub autosave: bool,

    /// GitHub personal access token for API access (issue/PR references)
    #[arg(long, env = GITHUB_TOKEN_ENV)]
    pub github_token: Option<String>,

    /// GitHub repository (owner/repo) for autolink detection.
    /// If not specified, will try to detect from .git/config.
    #[arg(long, env = "MARKDONE_GITHUB_REPO")]
    pub github_repo: Option<String>,

    /// Path to a TOML config file (theme + font). Defaults to the platform config dir,
    /// e.g. `~/.config/markdone/config.toml`.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

/// Font size bounds (logical px) accepted from the config and the preferences.
#[cfg(feature = "app")]
pub const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 6.0..=96.0;
/// Text width bounds (logical px) for the "limited width" layout.
#[cfg(feature = "app")]
pub const MAX_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 400.0..=2400.0;

/// What to do when an open file changes on disk and its document has no unsaved
/// changes. (With unsaved changes the user is always asked.)
#[cfg(feature = "app")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalChange {
    /// Reload the document silently.
    Reload,
    /// Ask before reloading.
    Ask,
}

#[cfg(feature = "app")]
impl ExternalChange {
    fn key(self) -> &'static str {
        match self {
            ExternalChange::Reload => "reload",
            ExternalChange::Ask => "ask",
        }
    }
}

/// The settings editable in the Preferences window (saved to `config.toml`).
#[cfg(feature = "app")]
#[derive(Debug, Clone, PartialEq)]
pub struct Preferences {
    /// Document font size, logical px.
    pub font_size: f32,
    /// Text spans the whole window width instead of `max_width`.
    pub full_width: bool,
    /// Maximum text width (logical px) when not `full_width`.
    pub max_width: f32,
    /// Show line numbers in the left margin (View menu).
    pub line_numbers: bool,
    /// Show the differences from the last git commit (View menu).
    pub git_diff: bool,
    /// Theme name: a built-in or a `[themes.<name>]` of the config file.
    pub theme: String,
    pub external_change: ExternalChange,
    /// Compare the files with the disk when the window gets the focus or a tab is
    /// activated (catches changes the file watcher misses, e.g. on network shares).
    pub check_on_focus: bool,
    /// Look up GitHub references (issues, PRs, users) found in documents: reads the
    /// repository from `.git/config` and calls api.github.com.
    pub github: bool,
}

#[cfg(feature = "app")]
impl Default for Preferences {
    fn default() -> Self {
        Self {
            font_size: FONT_SIZE,
            full_width: false,
            max_width: crate::consts::MAX_CONTENT_WIDTH,
            line_numbers: false,
            git_diff: true,
            theme: "dracula".to_string(),
            external_change: ExternalChange::Reload,
            check_on_focus: true,
            github: false,
        }
    }
}

/// The runtime config: the resolved theme, the document font and the preferences.
/// Built by [`load`] from `config.toml`, falling back to the built-in defaults.
#[cfg(feature = "app")]
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub theme: EditorTheme,
    /// The document font family; `None` = system monospace.
    pub font_family: Option<String>,
    pub font_size: f32,
    pub prefs: Preferences,
    /// Theme names to offer: the built-ins, then the config's own `[themes.*]`.
    pub theme_names: Vec<String>,
}

#[cfg(feature = "app")]
impl Default for ResolvedConfig {
    fn default() -> Self {
        Self {
            theme: EditorTheme::dracula(),
            font_family: None,
            font_size: FONT_SIZE,
            prefs: Preferences::default(),
            theme_names: EditorTheme::BUILTIN_NAMES.iter().map(|n| n.to_string()).collect(),
        }
    }
}

/// Top-level `config.toml` shape. Unknown keys are rejected so typos surface as a clear
/// parse error (which downgrades to defaults) instead of being silently ignored.
#[cfg(feature = "app")]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    font: Option<FontSpec>,
    theme: Option<ThemeSpec>,
    editor: Option<EditorSpec>,
    files: Option<FilesSpec>,
    network: Option<NetworkSpec>,
    /// User-defined named palettes, selectable via `theme.name`.
    #[serde(default)]
    themes: HashMap<String, Palette>,
}

#[cfg(feature = "app")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FontSpec {
    family: Option<String>,
    size: Option<f32>,
}

#[cfg(feature = "app")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditorSpec {
    full_width: Option<bool>,
    max_width: Option<f32>,
    line_numbers: Option<bool>,
    git_diff: Option<bool>,
}

#[cfg(feature = "app")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesSpec {
    /// "reload" or "ask".
    on_external_change: Option<String>,
    check_on_focus: Option<bool>,
}

#[cfg(feature = "app")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkSpec {
    github: Option<bool>,
}

#[cfg(feature = "app")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeSpec {
    name: Option<String>,
    overrides: Option<Palette>,
}

/// A partial color palette: any omitted field inherits from the theme it overlays.
/// Colors are `#RRGGBB` or `#RRGGBBAA` hex strings.
#[cfg(feature = "app")]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Palette {
    background: Option<String>,
    surface: Option<String>,
    foreground: Option<String>,
    selection: Option<String>,
    comment: Option<String>,
    red: Option<String>,
    orange: Option<String>,
    yellow: Option<String>,
    green: Option<String>,
    cyan: Option<String>,
    purple: Option<String>,
    pink: Option<String>,
}

#[cfg(feature = "app")]
impl Palette {
    /// Overlay this partial palette onto `theme`, warning about (and skipping) bad hex.
    fn apply(&self, theme: &mut EditorTheme) {
        let set = |slot: &mut Color, field: &Option<String>, name: &str| {
            if let Some(hex) = field {
                match parse_hex(hex) {
                    Ok(c) => *slot = c,
                    Err(e) => eprintln!("[markdone] config: {name}: {e}"),
                }
            }
        };
        set(&mut theme.background, &self.background, "background");
        set(&mut theme.surface, &self.surface, "surface");
        set(&mut theme.foreground, &self.foreground, "foreground");
        set(&mut theme.selection, &self.selection, "selection");
        set(&mut theme.comment, &self.comment, "comment");
        set(&mut theme.red, &self.red, "red");
        set(&mut theme.orange, &self.orange, "orange");
        set(&mut theme.yellow, &self.yellow, "yellow");
        set(&mut theme.green, &self.green, "green");
        set(&mut theme.cyan, &self.cyan, "cyan");
        set(&mut theme.purple, &self.purple, "purple");
        set(&mut theme.pink, &self.pink, "pink");
    }
}

/// Parse a `#RRGGBB` / `#RRGGBBAA` hex color (leading `#` optional).
#[cfg(feature = "app")]
fn parse_hex(s: &str) -> Result<Color, String> {
    let t = s.trim();
    let h = t.strip_prefix('#').unwrap_or(t);
    // Reject non-ASCII first: `h.len()` is bytes, so a multi-byte char (e.g. `€000`, 6
    // bytes) would pass the length match and then `h[i..i+2]` would slice inside the char
    // and panic — crashing `config::load` at startup instead of falling back to defaults.
    if !h.is_ascii() {
        return Err(format!("invalid hex color '{s}'"));
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16);
    match h.len() {
        6 => match (byte(0), byte(2), byte(4)) {
            (Ok(r), Ok(g), Ok(b)) => Ok(Color::from_rgba8(r, g, b, 0xFF)),
            _ => Err(format!("invalid hex color '{s}'")),
        },
        8 => match (byte(0), byte(2), byte(4), byte(6)) {
            (Ok(r), Ok(g), Ok(b), Ok(a)) => Ok(Color::from_rgba8(r, g, b, a)),
            _ => Err(format!("invalid hex color '{s}'")),
        },
        _ => Err(format!("expected #RRGGBB or #RRGGBBAA, got '{s}'")),
    }
}

/// The default config path: `<platform config dir>/markdone/config.toml`.
#[cfg(feature = "app")]
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("markdone").join("config.toml"))
}

/// Load and resolve the config. `explicit` is the `--config` path (else the default
/// location is used). A missing file → defaults silently (unless named explicitly); a
/// read or parse error → warn to stderr + defaults, so a bad config never blocks startup.
#[cfg(feature = "app")]
pub fn load(explicit: Option<&Path>) -> ResolvedConfig {
    let Some(path) = explicit.map(Path::to_path_buf).or_else(default_config_path) else {
        return ResolvedConfig::default();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if explicit.is_some() {
                eprintln!("[markdone] config: {} not found", path.display());
            }
            return ResolvedConfig::default();
        }
        Err(e) => {
            eprintln!("[markdone] config: cannot read {}: {e}", path.display());
            return ResolvedConfig::default();
        }
    };
    match toml::from_str::<FileConfig>(&text) {
        Ok(file) => resolve(file),
        Err(e) => {
            eprintln!("[markdone] config: {} parse error: {e}", path.display());
            ResolvedConfig::default()
        }
    }
}

/// Resolve a parsed file into runtime settings: pick the base theme (a user-defined
/// `[themes.<name>]` shadows a built-in of the same name and inherits dracula for omitted
/// colors), apply `[theme.overrides]`, then the font.
#[cfg(feature = "app")]
fn resolve(file: FileConfig) -> ResolvedConfig {
    let mut cfg = ResolvedConfig::default();
    let mut custom: Vec<String> = file
        .themes
        .keys()
        .filter(|n| !cfg.theme_names.contains(n))
        .cloned()
        .collect();
    custom.sort();
    cfg.theme_names.extend(custom);
    if let Some(theme) = &file.theme {
        if let Some(name) = &theme.name {
            cfg.prefs.theme = name.clone();
            if let Some(custom) = file.themes.get(name) {
                let mut base = EditorTheme::dracula();
                custom.apply(&mut base);
                cfg.theme = base;
            } else if let Some(builtin) = EditorTheme::by_name(name) {
                cfg.theme = builtin;
            } else {
                eprintln!("[markdone] config: unknown theme '{name}', using dracula");
            }
        }
        if let Some(overrides) = &theme.overrides {
            overrides.apply(&mut cfg.theme);
        }
    }
    if let Some(font) = &file.font {
        if let Some(family) = &font.family {
            let f = family.trim();
            if !f.is_empty() {
                cfg.font_family = Some(f.to_string());
            }
        }
        if let Some(size) = font.size {
            if size.is_finite() && FONT_SIZE_RANGE.contains(&size) {
                cfg.font_size = size;
                cfg.prefs.font_size = size;
            } else {
                eprintln!(
                    "[markdone] config: font.size {size} out of range (6–96), using {}",
                    cfg.font_size
                );
            }
        }
    }
    if let Some(editor) = &file.editor {
        if let Some(full) = editor.full_width {
            cfg.prefs.full_width = full;
        }
        if let Some(numbers) = editor.line_numbers {
            cfg.prefs.line_numbers = numbers;
        }
        if let Some(diff) = editor.git_diff {
            cfg.prefs.git_diff = diff;
        }
        if let Some(w) = editor.max_width {
            if w.is_finite() && MAX_WIDTH_RANGE.contains(&w) {
                cfg.prefs.max_width = w;
            } else {
                eprintln!("[markdone] config: editor.max_width {w} out of range (400–2400)");
            }
        }
    }
    if let Some(files) = &file.files {
        match files.on_external_change.as_deref() {
            Some("reload") => cfg.prefs.external_change = ExternalChange::Reload,
            Some("ask") => cfg.prefs.external_change = ExternalChange::Ask,
            Some(other) => eprintln!(
                "[markdone] config: files.on_external_change '{other}' (expected reload or ask)"
            ),
            None => {}
        }
        if let Some(check) = files.check_on_focus {
            cfg.prefs.check_on_focus = check;
        }
    }
    if let Some(network) = &file.network
        && let Some(github) = network.github
    {
        cfg.prefs.github = github;
    }
    cfg
}

/// Write `prefs` into the config file at `path`, keeping everything else in it
/// (comments, custom themes, overrides). A missing file is created. A file that does
/// not parse is left untouched (error).
#[cfg(feature = "app")]
pub fn save_preferences(path: &Path, prefs: &Preferences) -> std::io::Result<()> {
    use toml_edit::{DocumentMut, Item, Table, value};

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc: DocumentMut = text.parse().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not valid TOML: {e}", path.display()),
        )
    })?;
    let mut set = |table: &str, key: &str, item: Item| {
        let entry = doc.entry(table).or_insert_with(|| Item::Table(Table::new()));
        if let Some(t) = entry.as_table_like_mut() {
            t.insert(key, item);
        }
    };
    // Whole numbers are written without a fraction ("18", not "18.0").
    let number = |x: f32| {
        if x.fract() == 0.0 {
            value(x as i64)
        } else {
            value(f64::from(x))
        }
    };
    set("font", "size", number(prefs.font_size));
    set("theme", "name", value(prefs.theme.as_str()));
    set("editor", "full_width", value(prefs.full_width));
    set("editor", "max_width", number(prefs.max_width));
    set("editor", "line_numbers", value(prefs.line_numbers));
    set("editor", "git_diff", value(prefs.git_diff));
    set("files", "on_external_change", value(prefs.external_change.key()));
    set("files", "check_on_focus", value(prefs.check_on_focus));
    set("network", "github", value(prefs.github));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, path)
}

#[cfg(all(test, feature = "app"))]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(
            parse_hex("#FF0000").unwrap(),
            Color::from_rgba8(255, 0, 0, 255)
        );
        assert_eq!(
            parse_hex("00FF00").unwrap(),
            Color::from_rgba8(0, 255, 0, 255)
        );
        assert_eq!(
            parse_hex("#0000FF80").unwrap(),
            Color::from_rgba8(0, 0, 255, 0x80)
        );
        assert!(parse_hex("#ZZ0000").is_err());
        assert!(parse_hex("#FFF").is_err());
        // Non-ASCII of the right byte length must error, not panic (would crash startup).
        assert!(parse_hex("€000").is_err());
        assert!(parse_hex("#café00").is_err());
    }

    #[test]
    fn empty_config_is_default() {
        let cfg = resolve(FileConfig::default());
        assert_eq!(cfg.theme.background, EditorTheme::dracula().background);
        assert!(cfg.font_family.is_none());
        assert_eq!(cfg.font_size, FONT_SIZE);
    }

    #[test]
    fn builtin_theme_by_name() {
        let f: FileConfig = toml::from_str("[theme]\nname = \"solarized-light\"").unwrap();
        let cfg = resolve(f);
        assert_eq!(
            cfg.theme.background,
            EditorTheme::solarized_light().background
        );
    }

    #[test]
    fn unknown_theme_falls_back_to_dracula() {
        let f: FileConfig = toml::from_str("[theme]\nname = \"nope\"").unwrap();
        let cfg = resolve(f);
        assert_eq!(cfg.theme.background, EditorTheme::dracula().background);
    }

    #[test]
    fn overrides_apply_on_top_of_preset() {
        let f: FileConfig =
            toml::from_str("[theme]\nname = \"nord\"\n[theme.overrides]\nbackground = \"#010203\"")
                .unwrap();
        let cfg = resolve(f);
        assert_eq!(cfg.theme.background, Color::from_rgba8(1, 2, 3, 255));
        assert_eq!(cfg.theme.pink, EditorTheme::nord().pink);
    }

    #[test]
    fn custom_theme_inherits_dracula_and_shadows_builtin() {
        let f: FileConfig = toml::from_str(
            "[theme]\nname = \"dracula\"\n[themes.dracula]\nbackground = \"#101010\"",
        )
        .unwrap();
        let cfg = resolve(f);
        assert_eq!(
            cfg.theme.background,
            Color::from_rgba8(0x10, 0x10, 0x10, 255)
        );
        assert_eq!(cfg.theme.foreground, EditorTheme::dracula().foreground);
    }

    #[test]
    fn font_family_and_size() {
        let f: FileConfig =
            toml::from_str("[font]\nfamily = \"JetBrains Mono\"\nsize = 20.0").unwrap();
        let cfg = resolve(f);
        assert_eq!(cfg.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(cfg.font_size, 20.0);
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(toml::from_str::<FileConfig>("[font]\nfamly = \"x\"").is_err());
    }

    #[test]
    fn preference_sections_parse() {
        let file: FileConfig = toml::from_str(
            "[editor]\nfull_width = true\nmax_width = 1000\n\
             [files]\non_external_change = \"ask\"\ncheck_on_focus = false\n\
             [network]\ngithub = true\n",
        )
        .unwrap();
        let p = resolve(file).prefs;
        assert!(p.full_width);
        assert_eq!(p.max_width, 1000.0);
        assert_eq!(p.external_change, ExternalChange::Ask);
        assert!(!p.check_on_focus);
        assert!(p.github);
        // Defaults: limited width, reload, check on focus, no network.
        let d = resolve(FileConfig::default()).prefs;
        assert_eq!(d, Preferences::default());
        assert!(!d.github);
    }

    #[test]
    fn custom_themes_are_offered_after_the_builtins() {
        let file: FileConfig =
            toml::from_str("[themes.zeta]\nbackground = \"#000000\"\n[themes.alpha]\n").unwrap();
        let names = resolve(file).theme_names;
        let n = EditorTheme::BUILTIN_NAMES.len();
        assert_eq!(names[..n], EditorTheme::BUILTIN_NAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>()[..]);
        assert_eq!(names[n..], ["alpha".to_string(), "zeta".to_string()]);
    }

    /// Saving the preferences keeps the user's comments, custom themes and unrelated
    /// keys, and the result loads back to the same preferences.
    #[test]
    fn save_preferences_round_trips_and_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("markdone_prefs_{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "# my comment\n[font]\nfamily = \"JetBrains Mono\" # inline\nsize = 18\n\n\
             [themes.mine]\nbackground = \"#101010\"\n",
        )
        .unwrap();
        let prefs = Preferences {
            font_size: 20.0,
            full_width: true,
            max_width: 960.0,
            line_numbers: true,
            git_diff: false,
            theme: "mine".into(),
            external_change: ExternalChange::Ask,
            check_on_focus: false,
            github: true,
        };
        save_preferences(&path, &prefs).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my comment"));
        assert!(text.contains("# inline"));
        assert!(text.contains("[themes.mine]"));
        assert!(text.contains("size = 20\n"), "{text}");
        let loaded = load(Some(&path));
        assert_eq!(loaded.prefs, prefs);
        assert_eq!(loaded.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(loaded.theme.background.to_rgba8().r, 0x10);

        // A file that is not valid TOML is never overwritten.
        std::fs::write(&path, "[font\n").unwrap();
        assert!(save_preferences(&path, &prefs).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[font\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
