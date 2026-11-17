//! Modal dialogs: unsaved changes on close, a file changed on disk under unsaved edits,
//! and plain messages (errors, notices).
//!
//! These functions only draw and report the user's choice. The shell owns the state
//! behind a dialog (which tabs are closing, the disk content waiting to be applied) and
//! acts on the returned [`Answer`].

/// What a dialog shows.
pub enum DialogView {
    /// Documents with unsaved changes are about to be closed (file names, in tab order).
    Unsaved { names: Vec<String>, quitting: bool },
    /// The file of the active document changed on disk while it had unsaved changes.
    DiskConflict { name: String },
    Message { title: String, text: String },
}

/// The user's choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Save the documents, then close.
    Save,
    /// Close without saving.
    Discard,
    /// Abort: nothing is closed.
    Cancel,
    /// Replace the buffer with the disk version.
    Reload,
    /// Keep the buffer; the disk version is ignored (the next save overwrites it).
    KeepMine,
    /// Dismiss a message.
    Ok,
}

const WIDTH: f32 = 420.0;

/// Draw the dialog; returns the answer once the user picks one.
pub fn show(ctx: &egui::Context, view: &DialogView) -> Option<Answer> {
    let mut answer = None;
    let response = egui::Modal::new(egui::Id::new("writ-dialog")).show(ctx, |ui| {
        ui.set_width(WIDTH);
        ui.spacing_mut().item_spacing.y = 10.0;
        match view {
            DialogView::Unsaved { names, quitting } => {
                let title = if names.len() == 1 {
                    format!("Save changes to “{}”?", names[0])
                } else {
                    format!("Save changes to {} documents?", names.len())
                };
                heading(ui, &title);
                if names.len() > 1 {
                    egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                        for name in names {
                            ui.label(format!("•  {name}"));
                        }
                    });
                }
                ui.label(if *quitting {
                    "Your changes will be lost if you quit without saving."
                } else {
                    "Your changes will be lost if you close without saving."
                });
                let save = if names.len() == 1 { "Save" } else { "Save All" };
                let discard = if *quitting { "Quit Without Saving" } else { "Don't Save" };
                answer = buttons(
                    ui,
                    &[(discard, Answer::Discard)],
                    &[("Cancel", Answer::Cancel), (save, Answer::Save)],
                );
            }
            DialogView::DiskConflict { name } => {
                heading(ui, &format!("“{name}” changed on disk"));
                ui.label(
                    "Another program modified this file, but it also has unsaved changes \
                     here. Reloading replaces your changes with the version on disk; \
                     keeping yours means the next save overwrites the disk version.",
                );
                answer = buttons(
                    ui,
                    &[("Reload from Disk", Answer::Reload)],
                    &[("Keep My Changes", Answer::KeepMine)],
                );
            }
            DialogView::Message { title, text } => {
                heading(ui, title);
                ui.label(text);
                answer = buttons(ui, &[], &[("OK", Answer::Ok)]);
            }
        }
    });
    // Escape (or a click outside the dialog) takes the choice that loses nothing.
    if answer.is_none() && response.should_close() {
        answer = Some(match view {
            DialogView::Unsaved { .. } => Answer::Cancel,
            DialogView::DiskConflict { .. } => Answer::KeepMine,
            DialogView::Message { .. } => Answer::Ok,
        });
    }
    answer
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).heading().strong());
}

/// A button row: `left` aligned left, `right` aligned right. The last right button is
/// the default one: highlighted and focused, so Enter picks it.
fn buttons(
    ui: &mut egui::Ui,
    left: &[(&str, Answer)],
    right: &[(&str, Answer)],
) -> Option<Answer> {
    let mut answer = None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        for &(label, a) in left {
            if ui.button(label).clicked() {
                answer = Some(a);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for (i, &(label, a)) in right.iter().rev().enumerate() {
                let default = i == 0;
                let mut button = egui::Button::new(label);
                if default {
                    button = button.fill(ui.visuals().selection.bg_fill);
                }
                let response = ui.add(button);
                if default && ui.memory(|m| m.focused().is_none()) {
                    response.request_focus();
                }
                if response.clicked() {
                    answer = Some(a);
                }
            }
        });
    });
    answer
}
