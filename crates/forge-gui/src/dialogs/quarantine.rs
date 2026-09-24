use std::path::{Path, PathBuf};

use forge_core::convert::{
    load_import_quarantine, save_import_quarantine, ImportQuarantineManifest, QuarantineCategory,
};
use sha2::{Digest, Sha256};

use crate::state::{AppState, StatusMessage};
use crate::theme::icons;
use crate::widgets::code_editor::{code_editor_numbered, Lang};

pub struct QuarantineState {
    root: Option<PathBuf>,
    manifest: Option<ImportQuarantineManifest>,
    selected: Option<String>,
    category: QuarantineCategory,
    script: String,
    comment: String,
    baseline_fingerprint: String,
    baseline_revision: String,
    dirty: bool,
    pub open: bool,
    error: Option<String>,
}

impl Default for QuarantineState {
    fn default() -> Self {
        Self {
            root: None,
            manifest: None,
            selected: None,
            category: QuarantineCategory::Assertion,
            script: String::new(),
            comment: String::new(),
            baseline_fingerprint: String::new(),
            baseline_revision: String::new(),
            dirty: false,
            open: false,
            error: None,
        }
    }
}

impl QuarantineState {
    pub fn available(&self) -> bool {
        self.error.is_some()
            || self
                .manifest
                .as_ref()
                .is_some_and(|manifest| !manifest.entries.is_empty())
    }

    pub fn sync_root(&mut self, root: Option<&Path>) -> Result<(), String> {
        if self.root.as_deref() == root {
            return Ok(());
        }
        match root {
            Some(root) => self.reload(root),
            None => {
                self.save()?;
                *self = Self::default();
                Ok(())
            }
        }
    }

    pub fn reload(&mut self, root: &Path) -> Result<(), String> {
        self.save()?;
        self.root = Some(root.to_path_buf());
        self.selected = None;
        self.script.clear();
        self.comment.clear();
        self.baseline_fingerprint.clear();
        self.baseline_revision.clear();
        self.dirty = false;
        self.error = None;
        match load_import_quarantine(root) {
            Ok(manifest) => {
                self.manifest = manifest;
                if let Some(entry) = self
                    .manifest
                    .as_ref()
                    .and_then(|manifest| manifest.entries.first())
                {
                    self.selected = Some(entry.id.clone());
                    self.category = entry.category;
                    self.script.clone_from(&entry.script);
                    self.comment.clone_from(&entry.comment);
                    self.baseline_fingerprint
                        .clone_from(&entry.source_fingerprint);
                    self.baseline_revision = review_revision(&entry.script, &entry.comment);
                }
            }
            Err(error) => {
                self.manifest = None;
                self.error = Some(error.to_string());
            }
        }
        if !self.available() {
            self.open = false;
        }
        match &self.error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn save(&mut self) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        let root = self.root.as_ref().ok_or("No project is open")?;
        let selected = self.selected.as_deref().ok_or("No script is selected")?;
        let mut manifest = load_import_quarantine(root)
            .map_err(|error| error.to_string())?
            .ok_or("No import quarantine is available")?;
        let entry = manifest
            .entries
            .iter_mut()
            .find(|entry| entry.id == selected)
            .ok_or("The selected quarantine entry no longer exists")?;
        if entry.source_fingerprint != self.baseline_fingerprint {
            let error =
                "The imported source changed while this script was open; discard edits and reload"
                    .to_string();
            self.error = Some(error.clone());
            self.open = true;
            return Err(error);
        }
        if review_revision(&entry.script, &entry.comment) != self.baseline_revision {
            let error =
                "This review was changed elsewhere; discard edits and reload before continuing"
                    .to_string();
            self.error = Some(error.clone());
            self.open = true;
            return Err(error);
        }
        entry.script.clone_from(&self.script);
        entry.comment.clone_from(&self.comment);
        save_import_quarantine(root, &manifest).map_err(|error| error.to_string())?;
        self.manifest = Some(manifest);
        self.baseline_revision = review_revision(&self.script, &self.comment);
        self.dirty = false;
        Ok(())
    }

    pub fn save_pending(&mut self) -> Result<(), String> {
        self.save()
    }

    pub fn close(&mut self) -> Result<(), String> {
        self.save()?;
        self.open = false;
        Ok(())
    }

    fn select(&mut self, id: &str) -> Result<(), String> {
        if self.selected.as_deref() == Some(id) {
            return Ok(());
        }
        self.save()?;
        let entry = self
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.entries.iter().find(|entry| entry.id == id))
            .ok_or("Quarantine entry no longer exists")?;
        self.selected = Some(entry.id.clone());
        self.category = entry.category;
        self.script.clone_from(&entry.script);
        self.comment.clone_from(&entry.comment);
        self.baseline_fingerprint
            .clone_from(&entry.source_fingerprint);
        self.baseline_revision = review_revision(&entry.script, &entry.comment);
        self.dirty = false;
        Ok(())
    }

    fn select_category(&mut self, category: QuarantineCategory) -> Result<(), String> {
        if self.category == category {
            return Ok(());
        }
        self.save()?;
        self.category = category;
        self.selected = None;
        self.script.clear();
        self.comment.clear();
        self.baseline_fingerprint.clear();
        self.baseline_revision.clear();
        let first = self.manifest.as_ref().and_then(|manifest| {
            manifest
                .entries
                .iter()
                .find(|entry| entry.category == category)
        });
        if let Some(entry) = first {
            self.selected = Some(entry.id.clone());
            self.script.clone_from(&entry.script);
            self.comment.clone_from(&entry.comment);
            self.baseline_fingerprint
                .clone_from(&entry.source_fingerprint);
            self.baseline_revision = review_revision(&entry.script, &entry.comment);
        }
        Ok(())
    }
}

fn review_revision(script: &str, comment: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(script.as_bytes());
    hash.update(b"\0");
    hash.update(comment.as_bytes());
    encode_lower_hex(&hash.finalize())
}

fn encode_lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let mut save_clicked = false;
    let mut close_clicked = false;
    let mut selected = None;
    let mut selected_category = None;
    let quarantine = &mut state.dialogs.quarantine;

    ui.horizontal(|ui| {
        ui.heading("Import quarantine");
        if quarantine.dirty {
            ui.label(egui::RichText::new(icons::DIRTY).color(ui.visuals().warn_fg_color));
        }
        ui.add_space(8.0);
        ui.weak("Imported JavaScript stays non-executable until reviewed and promoted.");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(icons::CLOSE).on_hover_text("Close").clicked() {
                close_clicked = true;
            }
            if ui
                .add_enabled(
                    quarantine.dirty,
                    egui::Button::new(format!("{} Save", icons::SAVE)),
                )
                .clicked()
            {
                save_clicked = true;
            }
        });
    });
    ui.separator();

    if let Some(error) = quarantine.error.clone() {
        ui.colored_label(ui.visuals().error_fg_color, error);
        if ui.button("Discard edits and reload").clicked() {
            if let Some(root) = quarantine.root.clone() {
                quarantine.dirty = false;
                quarantine.error = None;
                if let Err(error) = quarantine.reload(&root) {
                    state.status = Some(StatusMessage::error(error));
                }
            }
        }
        if close_clicked {
            if quarantine.dirty {
                state.status = Some(StatusMessage::error(
                    "Resolve or discard the conflicting quarantine edit before closing",
                ));
            } else {
                quarantine.open = false;
            }
        }
        return;
    }

    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(260.0, ui.available_height()),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.label(egui::RichText::new("CATEGORY").small().weak());
                for category in QuarantineCategory::ALL {
                    let count = quarantine.manifest.as_ref().map_or(0, |manifest| {
                        manifest
                            .entries
                            .iter()
                            .filter(|entry| entry.category == category)
                            .count()
                    });
                    if count == 0 {
                        continue;
                    }
                    if ui
                        .selectable_label(
                            quarantine.category == category,
                            format!("{}  {count}", category.label()),
                        )
                        .clicked()
                    {
                        selected_category = Some(category);
                    }
                }
                ui.add_space(10.0);
                ui.label(egui::RichText::new("SCRIPTS").small().weak());
                egui::ScrollArea::vertical()
                    .id_salt("quarantine-entry-list")
                    .show(ui, |ui| {
                        if let Some(manifest) = &quarantine.manifest {
                            for entry in manifest
                                .entries
                                .iter()
                                .filter(|entry| entry.category == quarantine.category)
                            {
                                let label = Path::new(&entry.source_path)
                                    .file_name()
                                    .map(|name| name.to_string_lossy())
                                    .unwrap_or_else(|| entry.source_path.as_str().into());
                                if ui
                                    .selectable_label(
                                        quarantine.selected.as_deref() == Some(&entry.id),
                                        format!("{label} · {}", entry.source_index),
                                    )
                                    .on_hover_text(&entry.source_path)
                                    .clicked()
                                {
                                    selected = Some(entry.id.clone());
                                }
                            }
                        }
                    });
            },
        );
        ui.separator();
        ui.vertical(|ui| {
            let entry = quarantine.selected.as_deref().and_then(|selected| {
                quarantine
                    .manifest
                    .as_ref()
                    .and_then(|manifest| manifest.entries.iter().find(|entry| entry.id == selected))
            });
            let Some(entry) = entry else {
                ui.centered_and_justified(|ui| {
                    ui.weak("Select an imported script to review it.");
                });
                return;
            };
            ui.horizontal_wrapped(|ui| {
                ui.strong(&entry.source_path);
                ui.weak(format!(
                    "{} · {} · {}",
                    entry.source_format.label(),
                    entry.category.label(),
                    entry.disposition.label()
                ));
            });
            ui.colored_label(ui.visuals().warn_fg_color, &entry.reason);
            ui.add_space(8.0);
            ui.label(egui::RichText::new("JAVASCRIPT").small().weak());
            let script_changed = egui::Frame::group(ui.style())
                .show(ui, |ui| {
                    egui::ScrollArea::both()
                        .id_salt("quarantine-script-scroll")
                        .max_height((ui.available_height() - 150.0).max(220.0))
                        .show(ui, |ui| {
                            code_editor_numbered(
                                ui,
                                quarantine
                                    .selected
                                    .as_deref()
                                    .unwrap_or("quarantine-script"),
                                &mut quarantine.script,
                                Lang::Plain,
                                None,
                                false,
                                18,
                                false,
                            )
                        })
                        .inner
                })
                .inner
                .changed();
            ui.add_space(8.0);
            ui.label(egui::RichText::new("REVIEW COMMENT").small().weak());
            let comment_changed = ui
                .add(
                    egui::TextEdit::multiline(&mut quarantine.comment)
                        .id_salt("quarantine-comment")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text(
                            "Add review notes, risks, or the intended native replacement...",
                        ),
                )
                .changed();
            quarantine.dirty |= script_changed || comment_changed;
        });
    });

    if let Some(id) = selected {
        if let Err(error) = quarantine.select(&id) {
            state.status = Some(StatusMessage::error(error));
        }
    }
    if let Some(category) = selected_category {
        if let Err(error) = quarantine.select_category(category) {
            state.status = Some(StatusMessage::error(error));
        }
    }
    if save_clicked {
        state.status = Some(match quarantine.save() {
            Ok(()) => StatusMessage::info("Saved import quarantine review"),
            Err(error) => StatusMessage::error(error),
        });
    }
    if close_clicked {
        if let Err(error) = quarantine.close() {
            state.status = Some(StatusMessage::error(error));
        }
    }
}

#[cfg(test)]
mod tests {
    use forge_core::convert::{
        merge_import_quarantine, ImportQuarantineEntry, ImportSourceFormat, QuarantineDisposition,
    };

    use super::*;

    #[test]
    fn lower_hex_preserves_leading_zeroes() {
        assert_eq!(encode_lower_hex(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn view_is_available_only_for_import_manifest() {
        let root = tempfile::tempdir().unwrap();
        let mut state = QuarantineState::default();
        state.reload(root.path()).unwrap();
        assert!(!state.available());
        merge_import_quarantine(
            root.path(),
            [ImportQuarantineEntry::new(
                ImportSourceFormat::Bruno,
                QuarantineCategory::Assertion,
                QuarantineDisposition::Blocked,
                "request.bru",
                1,
                "test('x', () => {})",
                "review",
            )],
        )
        .unwrap();
        state.reload(root.path()).unwrap();
        assert!(state.available());
    }

    #[test]
    fn reload_saves_edits_and_preserves_concurrently_added_entries() {
        let root = tempfile::tempdir().unwrap();
        let first = ImportQuarantineEntry::new(
            ImportSourceFormat::Bruno,
            QuarantineCategory::Assertion,
            QuarantineDisposition::Blocked,
            "first.bru",
            1,
            "test('first', () => {})",
            "review",
        );
        merge_import_quarantine(root.path(), [first]).unwrap();
        let mut state = QuarantineState::default();
        state.reload(root.path()).unwrap();
        state.script = "// reviewed first".to_string();
        state.comment = "keep this note".to_string();
        state.dirty = true;

        let second = ImportQuarantineEntry::new(
            ImportSourceFormat::Postman,
            QuarantineCategory::BeforeRequest,
            QuarantineDisposition::ReviewRequired,
            "second.json",
            1,
            "pm.variables.set('x', 1);",
            "review",
        );
        merge_import_quarantine(root.path(), [second]).unwrap();
        state.reload(root.path()).unwrap();

        let manifest = load_import_quarantine(root.path()).unwrap().unwrap();
        assert_eq!(manifest.entries.len(), 2);
        let reviewed = manifest
            .entries
            .iter()
            .find(|entry| entry.source_path == "first.bru")
            .unwrap();
        assert_eq!(reviewed.script, "// reviewed first");
        assert_eq!(reviewed.comment, "keep this note");
    }

    #[test]
    fn malformed_manifest_keeps_quarantine_navigation_available() {
        let root = tempfile::tempdir().unwrap();
        let path = forge_core::convert::quarantine_path(root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}").unwrap();
        let mut state = QuarantineState::default();

        assert!(state.reload(root.path()).is_err());
        assert!(state.available());
    }
}
