//! The Preferences window (Edit → Preferences…). It edits a copy of the preferences;
//! the shell applies every change immediately and saves it to `config.toml`.

use std::collections::HashMap;
use std::path::Path;

use crate::config::{ExternalChange, FONT_SIZE_RANGE, MAX_WIDTH_RANGE, Preferences, ThemeMode};
use crate::spelling::{self, Availability, DownloadState};
use crate::text_engine::FontFamilyInfo;

/// Font sizes offered by the slider (the config file accepts a wider range).
const FONT_SLIDER: std::ops::RangeInclusive<f32> = 8.0..=40.0;

/// The categories of the window, listed on its left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum Page {
    #[default]
    Appearance,
    Text,
    Files,
    Spelling,
    Privacy,
}

const PAGES: [(Page, &str); 5] = [
    (Page::Appearance, "Appearance"),
    (Page::Text, "Text"),
    (Page::Files, "Files"),
    (Page::Spelling, "Spelling"),
    (Page::Privacy, "Privacy"),
];

/// The width of the category list and of the page beside it, and the page's height (the
/// window keeps its size whatever the page; a longer page scrolls).
const LIST_W: f32 = 140.0;
const PAGE_W: f32 = 440.0;
const PAGE_H: f32 = 340.0;

/// Show the window while `open`; edits `prefs` in place. Returns `true` when a value
/// changed this frame.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    prefs: &mut Preferences,
    theme_names: &[String],
    font_families: &[FontFamilyInfo],
    config_path: Option<&Path>,
    dict_status: &HashMap<String, DownloadState>,
) -> bool {
    let before = prefs.clone();
    // The page shown is remembered while Markdone runs.
    let page_id = egui::Id::new("prefs-page");
    let mut page: Page = ctx.data(|d| d.get_temp(page_id)).unwrap_or_default();
    let response = egui::Modal::new(egui::Id::new("writ-preferences")).show(ctx, |ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.label(egui::RichText::new("Preferences").heading().strong());
        ui.add_space(4.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(LIST_W);
                ui.spacing_mut().item_spacing.y = 2.0;
                for (p, label) in PAGES {
                    if category_entry(ui, label, page == p) {
                        page = p;
                    }
                }
            });
            // A rule of fixed height between the list and the page (`ui.separator()` here
            // would take all the height available, and the window would grow with it
            // frame after frame).
            let (rule, _) = ui.allocate_exact_size(egui::vec2(9.0, PAGE_H), egui::Sense::hover());
            ui.painter().vline(
                rule.center().x,
                rule.y_range(),
                ui.visuals().widgets.noninteractive.bg_stroke,
            );
            ui.vertical(|ui| {
                ui.set_width(PAGE_W);
                ui.set_min_height(PAGE_H);
                egui::ScrollArea::vertical()
                    .id_salt(page)
                    .max_height(PAGE_H)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let title = PAGES.iter().find(|(p, _)| *p == page).map_or("", |p| p.1);
                        ui.label(egui::RichText::new(title).size(17.0).strong());
                        ui.add_space(2.0);
                        match page {
                            Page::Appearance => appearance_page(ui, prefs, theme_names),
                            Page::Text => text_page(ui, prefs, font_families),
                            Page::Files => files_page(ui, prefs),
                            Page::Spelling => spelling_page(ui, prefs, dict_status),
                            Page::Privacy => privacy_page(ui, prefs),
                        }
                    });
            });
        });
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
    ctx.data_mut(|d| d.insert_temp(page_id, page));
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

/// One entry of the category list; returns whether it was clicked. Drawn by hand: a
/// button's hover frame adds a border that shifts its text.
fn category_entry(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(LIST_W, 28.0), egui::Sense::click());
    let visuals = ui.visuals();
    let fill = if selected {
        Some(visuals.selection.bg_fill)
    } else if response.hovered() {
        Some(visuals.widgets.hovered.weak_bg_fill)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, 4.0, fill);
    }
    let color = if selected {
        visuals.selection.stroke.color
    } else {
        visuals.text_color()
    };
    ui.painter().text(
        egui::pos2(rect.left() + 10.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::TextStyle::Button.resolve(ui.style()),
        color,
    );
    response.clicked()
}

/// A two-column grid of labelled settings.
fn settings_grid(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([16.0, 10.0])
        .show(ui, add);
}

fn appearance_page(ui: &mut egui::Ui, prefs: &mut Preferences, theme_names: &[String]) {
    settings_grid(ui, "prefs-appearance", |ui| {
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

        ui.label("Dark theme");
        theme_picker(ui, "prefs-dark-theme", &mut prefs.dark_theme, theme_names);
        ui.end_row();

        ui.label("Light theme");
        theme_picker(ui, "prefs-light-theme", &mut prefs.light_theme, theme_names);
        ui.end_row();
    });
    note(
        ui,
        "More themes can be defined in the configuration file ([themes.<name>]).",
    );
}

fn text_page(ui: &mut egui::Ui, prefs: &mut Preferences, font_families: &[FontFamilyInfo]) {
    settings_grid(ui, "prefs-text", |ui| {
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
        let range =
            FONT_SLIDER.start().min(prefs.font_size)..=FONT_SLIDER.end().max(prefs.font_size);
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
    note(ui, "The code font lists fixed-pitch fonts only.");
}

fn files_page(ui: &mut egui::Ui, prefs: &mut Preferences) {
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
    ui.add_space(4.0);
    ui.checkbox(
        &mut prefs.check_on_focus,
        "Also check the files when the window gets the focus or a tab is selected",
    );
    note(
        ui,
        "Catches changes the file watcher misses, e.g. on network shares.",
    );
    ui.add_space(4.0);
    ui.checkbox(&mut prefs.autosave, "Save documents automatically");
    note(
        ui,
        "About a second after you stop typing, and when a document is closed. A new \
         document is saved by hand the first time, to give it a name.",
    );
}

fn spelling_page(
    ui: &mut egui::Ui,
    prefs: &mut Preferences,
    dict_status: &HashMap<String, DownloadState>,
) {
    ui.checkbox(&mut prefs.spell_check, "Check spelling");
    note(
        ui,
        "A word is correct if any checked language accepts it. Right-click an \
         underlined word for suggestions, or to add it to your dictionary.",
    );
    ui.add_space(4.0);
    ui.add_enabled_ui(prefs.spell_check, |ui| {
        egui::Grid::new("prefs-spelling")
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for lang in spelling::LANGUAGES {
                    let mut on = prefs.spell_languages.iter().any(|c| c == lang.code);
                    if ui.checkbox(&mut on, lang.name).changed() {
                        if on {
                            prefs.spell_languages.push(lang.code.to_string());
                        } else {
                            prefs.spell_languages.retain(|c| c != lang.code);
                        }
                    }
                    let status = match dict_status.get(lang.code) {
                        Some(DownloadState::Downloading) => {
                            egui::RichText::new("downloading…").small().weak()
                        }
                        Some(DownloadState::Failed(_)) => {
                            egui::RichText::new("download failed (uncheck and check to retry)")
                                .small()
                                .color(ui.visuals().error_fg_color)
                        }
                        None => egui::RichText::new(match spelling::availability(lang.code) {
                            Availability::System => "system dictionary",
                            Availability::Downloaded => "downloaded",
                            Availability::Missing => "downloaded when checked",
                        })
                        .small()
                        .weak(),
                    };
                    ui.label(status);
                    ui.end_row();
                }
            });
    });
}

fn privacy_page(ui: &mut egui::Ui, prefs: &mut Preferences) {
    ui.checkbox(
        &mut prefs.github,
        "Look up GitHub issues and pull requests mentioned in documents",
    );
    note(
        ui,
        "Reads the repository from .git/config and contacts api.github.com. Applies to \
         documents opened after the change.",
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
    ui.add_space(8.0);
    ui.checkbox(
        &mut prefs.check_updates,
        "Check for a new version of Markdone at startup",
    );
    note(
        ui,
        "Asks api.github.com for the latest release; a newer one is offered for \
         download. Help → Check for Updates… checks at any time.",
    );
    ui.add_space(8.0);
    ui.label(egui::RichText::new("Spelling dictionaries").strong());
    note(
        ui,
        "Dictionaries the system does not have are downloaded from LibreOffice's \
         repository on GitHub (raw.githubusercontent.com) when their language is checked \
         in Spelling. Nothing else is sent.",
    );
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

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().weak());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every page draws (a few frames each, so the layout settles) and nothing changes
    /// the preferences by itself.
    #[test]
    fn every_page_draws_without_changing_anything() {
        let ctx = egui::Context::default();
        let mut prefs = Preferences::default();
        let themes = vec!["dracula".to_string(), "catppuccin-latte".to_string()];
        let status = HashMap::from([(
            "it_IT".to_string(),
            DownloadState::Failed("offline".to_string()),
        )]);
        let window = egui::Id::new("writ-preferences");
        let mut size: Option<egui::Vec2> = None;
        for (page, _) in PAGES {
            ctx.data_mut(|d| d.insert_temp(egui::Id::new("prefs-page"), page));
            for frame in 0..6 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 700.0),
                    )),
                    ..Default::default()
                };
                let mut open = true;
                let mut changed = false;
                let _ = ctx.run_ui(input, |ui| {
                    changed = show(ui.ctx(), &mut open, &mut prefs, &themes, &[], None, &status);
                });
                assert!(open, "{page:?}: the window stays open");
                assert!(!changed, "{page:?}: drawing changes nothing");
                // Once laid out, the window keeps one size, whatever the page.
                let rect = ctx.memory(|m| m.area_rect(window)).expect("window shown");
                if frame >= 2 {
                    match size {
                        None => size = Some(rect.size()),
                        Some(s) => assert!(
                            (s - rect.size()).length() < 0.5,
                            "{page:?}: the window changed size, {s:?} → {:?}",
                            rect.size()
                        ),
                    }
                }
            }
            let shown: Page = ctx
                .data(|d| d.get_temp(egui::Id::new("prefs-page")))
                .unwrap();
            assert_eq!(shown, page, "the page is remembered");
        }
    }
}
