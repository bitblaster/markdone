//! The top menu bar.

use crate::commands::Command;

use super::MENU_BAR_H;

/// State the menu needs to enable/check its entries.
pub struct MenuState {
    pub outline_open: bool,
    pub tab_count: usize,
    /// Recent files: (file name, shortened folder), most recent first.
    pub recent: Vec<(String, String)>,
}

/// Lay out the menu bar in a top panel; returns the command picked this frame, if any.
pub fn menu_bar(ui: &mut egui::Ui, state: &MenuState) -> Option<Command> {
    let mut picked = None;
    egui::Panel::top("writ-menu-bar")
        .exact_size(MENU_BAR_H)
        .resizable(false)
        .show(ui, |ui| {
            let config = egui::containers::menu::MenuConfig::new().style(super::menu_entry_style);
            egui::MenuBar::new()
                .style(super::menu_entry_style)
                .config(config)
                .ui(ui, |ui| {
                    ui.menu_button("File", |ui| {
                        item(ui, "New", Command::New, true, &mut picked);
                        item(ui, "Open…", Command::Open, true, &mut picked);
                        ui.menu_button("Open Recent", |ui| {
                            if state.recent.is_empty() {
                                ui.add_enabled(false, egui::Button::new("No Recent Files"));
                            }
                            for (i, (name, dir)) in state.recent.iter().enumerate() {
                                let button = egui::Button::new(name.as_str()).shortcut_text(dir.as_str());
                                if ui.add(button).clicked() {
                                    picked = Some(Command::OpenRecent(i));
                                    ui.close();
                                }
                            }
                            ui.separator();
                            let any = !state.recent.is_empty();
                            item(ui, "Clear Recent", Command::ClearRecent, any, &mut picked);
                        });
                        ui.separator();
                        item(ui, "Save", Command::Save, true, &mut picked);
                        item(ui, "Save As…", Command::SaveAs, true, &mut picked);
                        ui.separator();
                        item(ui, "Close Tab", Command::CloseTab, true, &mut picked);
                        item(ui, "Quit", Command::Quit, true, &mut picked);
                    });
                    ui.menu_button("Edit", |ui| {
                        item(ui, "Undo", Command::Undo, true, &mut picked);
                        item(ui, "Redo", Command::Redo, true, &mut picked);
                        ui.separator();
                        item(ui, "Cut", Command::Cut, true, &mut picked);
                        item(ui, "Copy", Command::Copy, true, &mut picked);
                        item(ui, "Paste", Command::Paste, true, &mut picked);
                        item(ui, "Select All", Command::SelectAll, true, &mut picked);
                        ui.separator();
                        item(ui, "Find…", Command::Find, true, &mut picked);
                        item(ui, "Replace…", Command::Replace, true, &mut picked);
                    });
                    ui.menu_button("View", |ui| {
                        let toc = egui::Button::selectable(state.outline_open, "Table of Contents")
                            .shortcut_text(Command::ToggleOutline.shortcut());
                        if ui.add(toc).clicked() {
                            picked = Some(Command::ToggleOutline);
                            ui.close();
                        }
                        ui.separator();
                        let multi = state.tab_count > 1;
                        item(ui, "Next Tab", Command::NextTab, multi, &mut picked);
                        item(ui, "Previous Tab", Command::PrevTab, multi, &mut picked);
                    });
                });
        });
    picked
}

fn item(ui: &mut egui::Ui, label: &str, cmd: Command, enabled: bool, picked: &mut Option<Command>) {
    let button = egui::Button::new(label).shortcut_text(cmd.shortcut());
    if ui.add_enabled(enabled, button).clicked() {
        *picked = Some(cmd);
        ui.close();
    }
}
