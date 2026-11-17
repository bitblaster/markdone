//! The Preferences window (Edit → Preferences…). It edits a copy of the preferences;
//! the shell applies every change immediately and saves it to `config.toml`.

use std::path::Path;

use crate::config::{ExternalChange, FONT_SIZE_RANGE, MAX_WIDTH_RANGE, Preferences};

/// Font sizes offered by the slider (the config file accepts a wider range).
const FONT_SLIDER: std::ops::RangeInclusive<f32> = 8.0..=40.0;

/// Show the window while `open`; edits `prefs` in place. Returns `true` when a value
/// changed this frame.
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    prefs: &mut Preferences,
    theme_names: &[String],
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
                    ui.label("Theme");
                    egui::ComboBox::from_id_salt("prefs-theme")
                        .selected_text(prefs.theme.as_str())
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            for name in theme_names {
                                ui.selectable_value(&mut prefs.theme, name.clone(), name.as_str());
                            }
                        });
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
            note(ui, "A document with unsaved changes is never reloaded without asking.");
            ui.checkbox(
                &mut prefs.check_on_focus,
                "Also check the files when the window gets the focus or a tab is selected",
            );
            note(ui, "Catches changes the file watcher misses, e.g. on network shares.");

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

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(title).strong());
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().weak());
}
