//! The Preferences window (Edit → Preferences…). It edits a copy of the preferences;
//! the shell applies every change immediately and saves it to `config.toml`.

use std::path::Path;

use crate::config::{ExternalChange, FONT_SIZE_RANGE, MAX_WIDTH_RANGE, Preferences, ThemeMode};
use crate::text_engine::FontFamilyInfo;

/// Font sizes offered by the slider (the config file accepts a wider range).
const FONT_SLIDER: std::ops::RangeInclusive<f32> = 8.0..=40.0;

/// Show the window while `open`; edits `prefs` in place. Returns `true` when a value
/// changed this frame.
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    prefs: &mut Preferences,
    theme_names: &[String],
    font_families: &[FontFamilyInfo],
    config_path: Option<&Path>,
) -> bool {
    let before = prefs.clone();
    let response = egui::Modal::new(egui::Id::new("writ-preferences")).show(ctx, |ui| {
        ui.set_width(460.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.label(egui::RichText::new("Preferences").heading().strong());

        section(ui, "Appearance");
        egui::Grid::new("prefs-appearance")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                ui.label("Dark theme");
                theme_picker(ui, "prefs-dark-theme", &mut prefs.dark_theme, theme_names);
                ui.end_row();

                ui.label("Light theme");
                theme_picker(ui, "prefs-light-theme", &mut prefs.light_theme, theme_names);
                ui.end_row();

                ui.label("Mode");
                ui.vertical(|ui| {
                    ui.checkbox(
                        &mut prefs.follow_system_theme,
                        "Follow the system light/dark setting",
                    );
                    ui.add_enabled_ui(!prefs.follow_system_theme, |ui| {
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut prefs.theme_mode, ThemeMode::Dark, "Dark");
                            ui.radio_value(&mut prefs.theme_mode, ThemeMode::Light, "Light");
                        });
                    });
                });
                ui.end_row();

                ui.label("Text font");
                font_picker(
                    ui,
                    "prefs-text-font",
                    &mut prefs.font_family,
                    font_families,
                    false,
                );
                ui.end_row();

                ui.label("Code font");
                font_picker(
                    ui,
                    "prefs-code-font",
                    &mut prefs.code_font_family,
                    font_families,
                    true,
                );
                ui.end_row();

                ui.label("Font size");
                let range = FONT_SLIDER.start().min(prefs.font_size)
                    ..=FONT_SLIDER.end().max(prefs.font_size);
                ui.add(
                    egui::Slider::new(&mut prefs.font_size, range)
                        .step_by(1.0)
                        .suffix(" px"),
                );
                ui.end_row();

                ui.label("Text width");
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut prefs.full_width, false, "At most");
                        ui.add_enabled(
                            !prefs.full_width,
                            egui::DragValue::new(&mut prefs.max_width)
                                .range(MAX_WIDTH_RANGE)
                                .speed(5.0)
                                .suffix(" px"),
                        );
                    });
                    ui.radio_value(&mut prefs.full_width, true, "Full window width");
                });
                ui.end_row();
            });

        section(ui, "Files");
        ui.horizontal(|ui| {
            ui.label("When a file changes on disk");
            egui::ComboBox::from_id_salt("prefs-external")
                .selected_text(match prefs.external_change {
                    ExternalChange::Reload => "Reload it",
                    ExternalChange::Ask => "Ask before reloading",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut prefs.external_change,
                        ExternalChange::Reload,
                        "Reload it",
                    );
                    ui.selectable_value(
                        &mut prefs.external_change,
                        ExternalChange::Ask,
                        "Ask before reloading",
                    );
                });
        });
        note(
            ui,
            "A document with unsaved changes is never reloaded without asking.",
        );
        ui.checkbox(
            &mut prefs.check_on_focus,
            "Also check the files when the window gets the focus or a tab is selected",
        );
        note(
            ui,
            "Catches changes the file watcher misses, e.g. on network shares.",
        );
        ui.checkbox(&mut prefs.autosave, "Save documents automatically");
        note(
            ui,
            "About a second after you stop typing, and when a document is closed. A new \
             document is saved by hand the first time, to give it a name.",
        );

        section(ui, "Privacy");
        ui.checkbox(
            &mut prefs.github,
            "Look up GitHub issues and pull requests mentioned in documents",
        );
        note(
            ui,
            "Reads the repository from .git/config and contacts api.github.com. \
                 Applies to documents opened after the change.",
        );
        // Always shown; editable only while the lookups are on.
        ui.add_enabled_ui(prefs.github, |ui| {
            ui.horizontal(|ui| {
                ui.label("GitHub token");
                let mut token = prefs.github_token.clone().unwrap_or_default();
                let field = egui::TextEdit::singleline(&mut token)
                    .password(true)
                    .hint_text("needed to check the references")
                    .desired_width(260.0);
                if ui.add(field).changed() {
                    let token = token.trim();
                    prefs.github_token = (!token.is_empty()).then(|| token.to_string());
                }
            });
            note(
                ui,
                "Stored in plain text in the configuration file. --github-token or the \
                 GITHUB_TOKEN variable take precedence.",
            );
        });

        ui.add_space(6.0);
        ui.separator();
        ui.horizontal(|ui| {
            if let Some(path) = config_path {
                note(ui, &format!("Saved to {}", path.display()));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    *open = false;
                }
            });
        });
    });
    // Changes apply as they are made, so Escape or a click outside just closes.
    if response.should_close() {
        *open = false;
    }
    // Keep the values in range whatever the widgets allowed (typed-in numbers).
    prefs.font_size = prefs
        .font_size
        .clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end())
        .round();
    prefs.max_width = prefs
        .max_width
        .clamp(*MAX_WIDTH_RANGE.start(), *MAX_WIDTH_RANGE.end())
        .round();
    *prefs != before
}

/// A drop-down of the theme names (built-ins, then the config file's own).
fn theme_picker(ui: &mut egui::Ui, id: &str, value: &mut String, names: &[String]) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.as_str())
        .width(220.0)
        .show_ui(ui, |ui| {
            for name in names {
                ui.selectable_value(value, name.clone(), name.as_str());
            }
        });
}

/// The default entry of the font pickers (`None`).
const SYSTEM_FONT: &str = "System monospace";

/// A drop-down of installed font families (fixed-pitch ones only when `monospace_only`),
/// headed by the system default. A configured family no longer installed still shows
/// as the selection.
fn font_picker(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut Option<String>,
    families: &[FontFamilyInfo],
    monospace_only: bool,
) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.as_deref().unwrap_or(SYSTEM_FONT))
        .width(220.0)
        .height(360.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, SYSTEM_FONT);
            for family in families.iter().filter(|f| !monospace_only || f.monospace) {
                ui.selectable_value(value, Some(family.name.clone()), family.name.as_str());
            }
        });
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(title).strong());
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().weak());
}
