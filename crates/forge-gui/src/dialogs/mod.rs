//! Modal dialogs beyond the simple name-input/confirm windows already
//! inlined in `panels::collections`: Settings, Search Everywhere, curl/
//! OpenAPI import, code export, the environment manager, About and the
//! empty-workspace Welcome pane.
//!
//! Every dialog's transient UI state hangs off a single [`DialogManager`]
//! embedded in [`AppState`], and every `show`/`dispatch` function here takes
//! `&mut AppState` (never `&mut self`) — the same shape `panels::collections`
//! already uses — so `app.rs` never has to juggle a second mutable borrow of
//! the state a dialog needs to read and mutate (workspace, tabs, status...).

pub mod about;
pub mod bruno_import;
pub mod curl_import;
pub mod env_editor;
pub mod grpc_call;
pub mod hooks_editor;
pub mod openapi_import;
pub mod postman_import;
pub mod quarantine;
#[cfg(feature = "pro")]
pub mod report;
pub mod search;
pub mod settings;
pub mod snippet_export;
pub mod tour;
pub mod v1_editor;
pub mod welcome;

use forge_core::store::Workspace;
use std::path::PathBuf;

use crate::bridge::Bridge;
use crate::keymap::ActionId;
use crate::panels::request_editor;
use crate::state::{AppState, StatusMessage};

/// All dialog-local UI state, owned by [`AppState`].
#[derive(Default)]
pub struct DialogManager {
    pub about_open: bool,
    pub settings: settings::SettingsState,
    pub search: search::SearchState,
    pub curl_import: curl_import::CurlImportState,
    pub openapi_import: openapi_import::OpenApiImportState,
    pub postman_import: postman_import::PostmanImportState,
    pub bruno_import: bruno_import::BrunoImportState,
    pub snippet_export: snippet_export::SnippetExportState,
    pub import_report: ImportReportState,
    pub export_review: ExportReviewState,
    pub tour: tour::TourState,
    pub env_editor: env_editor::EnvEditorState,
    pub hooks_editor: hooks_editor::HooksEditorState,
    pub grpc_call: grpc_call::GrpcCallState,
    pub v1_editor: v1_editor::V1EditorState,
    pub quarantine: quarantine::QuarantineState,
    pub update: crate::updater::UpdateState,
    pub license: crate::license::LicenseState,
    #[cfg(feature = "pro")]
    pub jira: crate::jira::JiraState,
    #[cfg(feature = "pro")]
    pub report: report::ReportState,
}

/// Persistent result of the last import. It stays open until dismissed so
/// conversion losses and created files remain inspectable after the toast.
#[derive(Default)]
pub struct ImportReportState {
    pub open: bool,
    pub source: String,
    pub summary: String,
    pub created: Vec<PathBuf>,
    pub created_count: usize,
    pub skipped: Vec<String>,
    pub blocked: Vec<String>,
    pub warnings: Vec<String>,
    pub quarantined_count: usize,
}

impl ImportReportState {
    pub(crate) fn completed(&mut self, source: &str, summary: String, report: ImportReportParts) {
        self.open = true;
        self.source = source.to_string();
        self.summary = summary;
        self.created_count = report.created_count;
        self.created = report.created;
        self.skipped = report.skipped;
        self.blocked = report.blocked;
        self.warnings = report.warnings;
        self.quarantined_count = report.quarantined_count;
    }
}

#[derive(Default)]
pub(crate) struct ImportReportParts {
    pub created: Vec<PathBuf>,
    pub created_count: usize,
    pub skipped: Vec<String>,
    pub blocked: Vec<String>,
    pub warnings: Vec<String>,
    pub quarantined_count: usize,
}

#[derive(Default)]
pub struct ExportReviewState {
    pub open: bool,
    pub title: String,
    pub output: Option<PathBuf>,
    pub content: Option<String>,
    pub warnings: Vec<String>,
    pub completed: bool,
    pub summary: String,
    pub error: Option<String>,
    pub filter: String,
    pub extension: String,
    reviewed_output: Option<PathBuf>,
    reviewed_destination: Option<Option<Vec<u8>>>,
}

impl ExportReviewState {
    pub(crate) fn prepare(
        &mut self,
        title: &str,
        output: PathBuf,
        content: String,
        warnings: Vec<String>,
        filter: &str,
        extension: &str,
    ) {
        let reviewed_destination = snapshot_export_destination(&output);
        self.open = true;
        self.title = title.to_string();
        self.output = Some(output.clone());
        self.reviewed_output = Some(output);
        self.reviewed_destination = reviewed_destination.as_ref().ok().cloned();
        self.content = Some(content);
        self.warnings = warnings;
        self.completed = false;
        self.summary.clear();
        self.error = reviewed_destination.err();
        self.filter = filter.to_string();
        self.extension = extension.to_string();
    }

    pub(crate) fn completed(&mut self, title: &str, summary: String, output: PathBuf) {
        self.open = true;
        self.title = title.to_string();
        self.output = Some(output);
        self.content = None;
        self.warnings.clear();
        self.completed = true;
        self.summary = summary;
        self.error = None;
    }
}

/// Render whichever overlay dialogs are currently open. Call once per frame;
/// each dialog internally no-ops when its own `open` flag is `false`. The
/// Welcome pane is not included here — it replaces the central panel's
/// content directly (see `dialogs::welcome::show`) rather than overlaying it.
pub fn show(ctx: &egui::Context, state: &mut AppState, bridge: &Bridge) {
    about::show(ctx, state);
    settings::show(ctx, state);
    search::show(ctx, state, bridge);
    curl_import::show(ctx, state);
    openapi_import::show(ctx, state);
    postman_import::show(ctx, state);
    bruno_import::show(ctx, state);
    snippet_export::show(ctx, state);
    show_import_report(ctx, state);
    show_export_review(ctx, state);
    env_editor::show(ctx, state);
    hooks_editor::show(ctx, state);
    grpc_call::show(ctx, state, bridge);
    crate::updater::show(ctx, &mut state.dialogs.update, bridge);
    crate::license::show(ctx, state, bridge);
    #[cfg(feature = "pro")]
    {
        crate::jira::show(ctx, state, bridge);
        report::show(ctx, state, bridge);
    }
    tour::show(ctx, state);
}

fn show_export_review(ctx: &egui::Context, state: &mut AppState) {
    if !state.dialogs.export_review.open {
        return;
    }
    let mut open = true;
    let mut export_clicked = false;
    let mut choose_destination = false;
    let mut done_clicked = false;
    let mut cancel_clicked = false;
    let report = &mut state.dialogs.export_review;
    let title = if report.completed {
        format!("{} export", report.title)
    } else {
        format!("Review {} export", report.title)
    };
    egui::Window::new(title)
        .id(egui::Id::new("export-review"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([560.0, 420.0])
        .show(ctx, |ui| {
            if report.completed {
                ui.label(&report.summary);
                if !report.warnings.is_empty() {
                    ui.add_space(6.0);
                    ui.strong("Omitted features");
                    for warning in &report.warnings {
                        ui.label(egui::RichText::new(warning).weak());
                    }
                }
            } else {
                if let Some(output) = &report.output {
                    ui.label(format!("Destination: {}", output.display()));
                }
                if report.warnings.is_empty() {
                    ui.label("No known export losses were detected.");
                } else {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("{} feature(s) will not be included:", report.warnings.len()),
                    );
                    egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                        for warning in &report.warnings {
                            ui.label(egui::RichText::new(warning).weak());
                        }
                    });
                }
                if let Some(error) = &report.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                if report.output.as_ref().is_some_and(|output| output.exists()) {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "The destination already exists. Export will replace that file only after you confirm.",
                    );
                }
                ui.horizontal(|ui| {
                    if ui.button("Choose another destination…").clicked() {
                        choose_destination = true;
                    }
                    let label = if report.output.as_ref().is_some_and(|output| output.exists()) {
                        "Overwrite file"
                    } else {
                        "Export"
                    };
                    if ui.button(label).clicked() {
                        export_clicked = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                });
            }
            if report.completed && ui.button("Done").clicked() {
                done_clicked = true;
            }
        });
    if done_clicked || cancel_clicked {
        open = false;
    }
    report.open = open;

    if choose_destination {
        let output = report.output.clone();
        let directory = output.as_deref().and_then(std::path::Path::parent);
        let file_name = output
            .as_deref()
            .and_then(std::path::Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("export.{}", report.extension));
        let picked = rfd::FileDialog::new()
            .add_filter(&report.filter, &[report.extension.as_str()])
            .set_directory(directory.unwrap_or_else(|| std::path::Path::new(".")))
            .set_file_name(file_name)
            .save_file();
        if let Some(output) = picked {
            let reviewed_destination = snapshot_export_destination(&output);
            report.reviewed_output = Some(output.clone());
            report.reviewed_destination = reviewed_destination.as_ref().ok().cloned();
            report.output = Some(output);
            report.error = reviewed_destination.err();
        }
    }
    if export_clicked {
        match write_reviewed_export(report) {
            Ok(()) => {
                let output = report
                    .output
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                report.completed = true;
                report.summary = format!("Exported to {output}");
                report.content = None;
                report.error = None;
            }
            Err(error) => report.error = Some(error),
        }
    }
}

fn write_reviewed_export(report: &mut ExportReviewState) -> Result<(), String> {
    let output = report
        .output
        .clone()
        .ok_or_else(|| "choose an export destination first".to_string())?;
    let content = report
        .content
        .clone()
        .ok_or_else(|| "export content is no longer available".to_string())?;
    if report.reviewed_output.as_ref() != Some(&output) {
        return Err(
            "the destination changed after review; review this path before exporting".into(),
        );
    }
    let reviewed = report
        .reviewed_destination
        .as_ref()
        .ok_or_else(|| "the destination could not be reviewed; choose it again".to_string())?;
    let current = snapshot_export_destination(&output)?;
    if reviewed != &current {
        report.reviewed_destination = Some(current);
        return Err(
            "the destination changed after review; inspect the updated destination and export again to confirm".into(),
        );
    }
    forge_core::reqv1::atomic_write(&output, content.as_bytes(), current.is_none())
}

fn snapshot_export_destination(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "refusing symbolic-link destination {}",
            path.display()
        )),
        Ok(metadata) if !metadata.is_file() => {
            Err(format!("{} is not a regular file", path.display()))
        }
        Ok(_) => std::fs::read(path)
            .map(Some)
            .map_err(|error| format!("cannot read {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

fn show_import_report(ctx: &egui::Context, state: &mut AppState) {
    if !state.dialogs.import_report.open {
        return;
    }
    let root = state
        .workspace
        .as_ref()
        .map(|workspace| workspace.root.clone())
        .or_else(|| state.assets.project_root());
    let mut open = state.dialogs.import_report.open;
    let mut open_request = None;
    let mut done_clicked = false;
    egui::Window::new(format!(
        "{} import report",
        state.dialogs.import_report.source
    ))
    .id(egui::Id::new("import-report"))
    .open(&mut open)
    .collapsible(false)
    .resizable(true)
    .default_size([600.0, 500.0])
    .show(ctx, |ui| {
        let report = &state.dialogs.import_report;
        ui.label(&report.summary);
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Created: {}", report.created_count));
            ui.separator();
            ui.label(format!("Skipped: {}", report.skipped.len()));
            ui.separator();
            ui.label(format!("Blocked: {}", report.blocked.len()));
            ui.separator();
            ui.label(format!("Warnings: {}", report.warnings.len()));
            ui.separator();
            ui.label(format!("Quarantined scripts: {}", report.quarantined_count));
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .show(ui, |ui| {
                if !report.created.is_empty() {
                    ui.strong("Created requests");
                    for path in &report.created {
                        ui.horizontal(|ui| {
                            let label = root
                                .as_deref()
                                .and_then(|root| path.strip_prefix(root).ok())
                                .unwrap_or(path)
                                .display()
                                .to_string();
                            ui.label(label);
                            if ui.small_button("Open").clicked() {
                                open_request = Some(path.clone());
                            }
                        });
                    }
                }
                show_report_items(ui, "Skipped", &report.skipped);
                show_report_items(ui, "Blocked", &report.blocked);
                show_report_items(ui, "Conversion warnings", &report.warnings);
            });
        if ui.button("Done").clicked() {
            done_clicked = true;
        }
    });
    if done_clicked {
        open = false;
    }
    state.dialogs.import_report.open = open;
    if let Some(path) = open_request {
        if let Err(error) = state
            .dialogs
            .v1_editor
            .open_file(path, state.active_env.clone())
        {
            state.status = Some(StatusMessage::error(error));
        } else {
            state.dialogs.import_report.open = false;
        }
    }
}

fn show_report_items(ui: &mut egui::Ui, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.strong(format!("{title} ({})", items.len()));
    for item in items {
        ui.label(egui::RichText::new(item).weak());
    }
}

/// Detect the global dialog-opening gestures that don't fit a single
/// [`egui::KeyboardShortcut`] in the `keymap` registry — currently just a
/// bare double `Shift` press for Search Everywhere. Call once per frame,
/// before `keymap::dispatch`.
pub fn handle_global_shortcuts(ctx: &egui::Context, state: &mut AppState) {
    if search::detect_double_shift(ctx, &mut state.dialogs.search) {
        state.dialogs.search.open(false);
    }
}

/// Execute a registered [`ActionId`]. Shared by the keyboard-shortcut
/// dispatcher in `app.rs` and Search Everywhere's Actions section (selecting
/// an action there routes through this exact function).
pub fn dispatch_action(state: &mut AppState, bridge: &Bridge, action: ActionId) {
    match action {
        ActionId::Save => {
            if state.dialogs.v1_editor.open {
                if state.dialogs.v1_editor.current_has_unsaved_edits() {
                    state.dialogs.v1_editor.save();
                }
            } else if let Some(idx) = state.active_tab {
                crate::app::save_tab(state, idx);
            }
        }
        ActionId::SaveAll => {
            if state.dialogs.v1_editor.open && state.dialogs.v1_editor.has_unsaved_edits() {
                state.dialogs.v1_editor.save();
            }
            crate::app::save_all(state);
        }
        ActionId::Send => {
            if state.dialogs.v1_editor.open {
                state.dialogs.v1_editor.run_current(bridge);
            } else {
                request_editor::send_active(state, bridge);
            }
        }
        ActionId::CloseTab => {
            if state.dialogs.v1_editor.open {
                state.dialogs.v1_editor.request_close();
            } else if let Some(idx) = state.active_tab {
                if state.auto_save && state.tabs.get(idx).is_some_and(|tab| tab.dirty) {
                    crate::app::save_tab(state, idx);
                }
                state.close_tab(idx);
            }
        }
        ActionId::NextTab => {
            if state.dialogs.v1_editor.open {
                if state.auto_save && state.dialogs.v1_editor.current_has_unsaved_edits() {
                    state.dialogs.v1_editor.save();
                }
                state.dialogs.v1_editor.next_open_tab();
            } else {
                auto_save_active(state);
                state.next_tab();
            }
        }
        ActionId::PrevTab => {
            if state.dialogs.v1_editor.open {
                if state.auto_save && state.dialogs.v1_editor.current_has_unsaved_edits() {
                    state.dialogs.v1_editor.save();
                }
                state.dialogs.v1_editor.previous_open_tab();
            } else {
                auto_save_active(state);
                state.prev_tab();
            }
        }
        ActionId::OpenWorkspace => open_workspace(state),
        ActionId::ToggleCollections => state.show_collections = !state.show_collections,
        ActionId::ToggleZen => {
            state.zen_mode = !state.zen_mode;
            state.zen_left_revealed = false;
            state.zen_right_revealed = false;
            state.zen_bottom_revealed = false;
        }
        ActionId::OpenSettings => state.dialogs.settings.open = true,
        ActionId::ImportCurl => state.dialogs.curl_import.open(),
        ActionId::SearchActions => state.dialogs.search.open(true),
    }
}

fn auto_save_active(state: &mut AppState) {
    if let Some(idx) = state.active_tab {
        if state.auto_save && state.tabs.get(idx).is_some_and(|tab| tab.dirty) {
            crate::app::save_tab(state, idx);
        }
    }
}

/// Open a workspace via a folder picker, replacing whatever is currently
/// loaded. Shared by the File menu, the `Ctrl+O` shortcut and the Welcome
/// pane's "Open Workspace..." button.
pub fn open_workspace(state: &mut AppState) {
    if let Some(path) = rfd::FileDialog::new().pick_folder() {
        match Workspace::load(&path) {
            Ok(ws) => {
                // Handed to `ForgeApp` at the top of the next frame, which
                // runs the full switch flow (history store, cookie load,
                // UI-state restore) — see `app.rs`.
                state.pending_workspace = Some(ws);
                state.pending_api_project = None;
                state.open_request_after_workspace = false;
                state.status = Some(StatusMessage::info(format!("Opened {}", path.display())));
                welcome::remember_recent(&path);
            }
            Err(e) => state.status = Some(StatusMessage::error(e.to_string())),
        }
    }
}

/// Create a ready-to-use API project via one folder picker.
pub fn new_workspace(state: &mut AppState) {
    if let Some(path) = rfd::FileDialog::new().pick_folder() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Workspace".to_string());
        match Workspace::create(&path, &name) {
            Ok(ws) => {
                state.pending_workspace = Some(ws);
                state.pending_api_project = None;
                state.open_request_after_workspace = true;
                state.status = Some(StatusMessage::info(format!(
                    "Created project at {}",
                    path.display()
                )));
                welcome::remember_recent(&path);
            }
            Err(e) => state.status = Some(StatusMessage::error(e.to_string())),
        }
    }
}
