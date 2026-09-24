//! Import Bruno (File menu): pick a Bruno collection directory (the one
//! holding `bruno.json`), preview requests/environments plus everything
//! that can't be mapped, and write it into the workspace. Reuses the
//! Postman dialog's write path for the collection tree.

use std::{
    sync::mpsc::{self, Receiver, TryRecvError},
    time::Duration,
};

use egui::Window;

use forge_core::convert::{import_bruno, BrunoImport};
use forge_core::store::{create_environment, save_environment, save_secrets};

use super::postman_import::{
    cached_v1_preview, import_collection, import_v1_environment, reload_workspace, show_blocked,
    show_warnings, v1_environment_path, V1ImportPreview,
};
use super::ImportReportParts;
use crate::state::{AppState, StatusMessage};

/// Transient state of the Bruno-import dialog, owned by
/// [`crate::dialogs::DialogManager`].
#[derive(Default)]
pub struct BrunoImportState {
    open: bool,
    parsed: Option<Result<BrunoImport, String>>,
    pending_parse: Option<Receiver<Result<BrunoImport, String>>>,
    cached_preview: Option<V1ImportPreview>,
    name: String,
    import_environments: bool,
}

impl BrunoImportState {
    /// Open a directory picker for a Bruno collection and parse it in the
    /// background.
    pub fn open(&mut self) {
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        self.parsed = None;
        self.pending_parse = Some(receiver);
        self.cached_preview = None;
        self.name.clear();
        self.import_environments = true;
        self.open = true;
        if let Err(error) = std::thread::Builder::new()
            .name("apiwright-bruno-import".to_string())
            .spawn(move || {
                let _ = sender.send(import_bruno(&dir).map_err(|error| error.to_string()));
            })
        {
            self.pending_parse = None;
            self.parsed = Some(Err(error.to_string()));
        }
    }
}

/// Render the dialog if open; no-op otherwise.
pub fn show(ctx: &egui::Context, state: &mut AppState) {
    if !state.dialogs.bruno_import.open {
        return;
    }
    poll_parse(ctx, &mut state.dialogs.bruno_import);
    let workspace = state.workspace.clone();
    let Some(root) = workspace
        .as_ref()
        .map(|workspace| workspace.root.clone())
        .or_else(|| state.assets.project_root())
    else {
        state.dialogs.bruno_import.open = false;
        state.status = Some(StatusMessage::error("Open a workspace before importing"));
        return;
    };

    let mut window_open = true;
    let mut import_clicked = false;
    let mut cancel_clicked = false;
    let mut collection_import_allowed = false;

    Window::new("Import Bruno")
        .id(egui::Id::new("bruno-import-dialog"))
        .collapsible(false)
        .resizable(true)
        .default_size([560.0, 420.0])
        .open(&mut window_open)
        .show(ctx, |ui| {
            let dialog = &mut state.dialogs.bruno_import;
            match &dialog.parsed {
                None => {
                    let message = if dialog.pending_parse.is_some() {
                        "Scanning and parsing collection…"
                    } else {
                        "No collection loaded."
                    };
                    ui.weak(message);
                }
                Some(Err(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                Some(Ok(import)) => {
                    ui.label(format!(
                        "Collection \"{}\" — {} request(s), {} environment(s)",
                        import.collection.name,
                        import.collection.request_count(),
                        import.environments.len()
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Import as:");
                        ui.text_edit_singleline(&mut dialog.name);
                    });
                    collection_import_allowed = !dialog.name.trim().is_empty();
                    if root.join("project.json").is_file() {
                        let preview = cached_v1_preview(
                            &mut dialog.cached_preview,
                            &import.collection,
                            &root,
                            dialog.name.trim(),
                        );
                        collection_import_allowed = collection_import_allowed
                            && (preview.request_count > 0
                                || preview.has_environment
                                || !import.collection.quarantine.is_empty());
                        ui.weak(format!(
                            "Request-v1 preview: {} request(s) ready, {} blocked; collection variables become an environment.",
                            preview.request_count,
                            preview.blocked.len()
                        ));
                        show_blocked(ui, &preview.blocked);
                        show_warnings(ui, &preview.warnings);
                    }
                    if !import.environments.is_empty() {
                        ui.checkbox(&mut dialog.import_environments, "Import environments");
                        ui.weak(
                            "Bruno exports never contain secret values — secrets come over \
                             declared but empty. Add their values to the gitignored project \
                             .env.local file before running requests.",
                        );
                        let duplicates = import
                            .environments
                            .iter()
                            .filter(|(environment, _)| {
                                if root.join("project.json").is_file() {
                                    v1_environment_path(&root, &environment.name).exists()
                                } else {
                                    workspace.as_ref().is_some_and(|workspace| {
                                        workspace
                                            .environments
                                            .iter()
                                            .any(|existing| {
                                                existing.env.name == environment.name
                                            })
                                    })
                                }
                            })
                            .count();
                        if duplicates > 0 {
                            ui.colored_label(
                                ui.visuals().warn_fg_color,
                                format!(
                                    "{duplicates} environment(s) already exist and will be skipped"
                                ),
                            );
                        }
                    }
                    if !import.collection.quarantine.is_empty() {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!(
                                "{} JavaScript block(s) will be preserved in import quarantine and remain non-executable.",
                                import.collection.quarantine.len()
                            ),
                        );
                    }
                    show_skipped(ui, &import.collection.skipped);
                }
            }

            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cancel_clicked = true;
                }
                let can_import = match &state.dialogs.bruno_import.parsed {
                    Some(Ok(_)) if root.join("project.json").is_file() => {
                        collection_import_allowed
                    }
                    Some(Ok(_)) => !state.dialogs.bruno_import.name.trim().is_empty(),
                    _ => false,
                };
                if ui
                    .add_enabled(can_import, egui::Button::new("Import"))
                    .clicked()
                {
                    import_clicked = true;
                }
            });
        });

    if import_clicked {
        let report_parts =
            import_report_parts(&root, &state.dialogs.bruno_import, workspace.as_ref());
        let result = state
            .dialogs
            .quarantine
            .save_pending()
            .and_then(|()| do_import(&root, &mut state.dialogs.bruno_import));
        match result {
            Err(e) => state.status = Some(StatusMessage::error(e)),
            Ok(msg) => {
                state.dialogs.bruno_import.open = false;
                if let Some(report) = report_parts {
                    state
                        .dialogs
                        .import_report
                        .completed("Bruno", msg.clone(), report);
                }
                let workspace_reload = reload_workspace(state);
                let quarantine_reload = state.dialogs.quarantine.reload(&root);
                state.status = Some(match (workspace_reload, quarantine_reload) {
                    (Ok(()), Ok(())) => StatusMessage::info(msg),
                    (Err(error), Ok(())) | (Ok(()), Err(error)) => StatusMessage::error(error),
                    (Err(workspace_error), Err(quarantine_error)) => StatusMessage::error(format!(
                        "{workspace_error}; quarantine reload failed: {quarantine_error}"
                    )),
                });
            }
        }
    }
    if cancel_clicked || !window_open {
        state.dialogs.bruno_import.open = false;
        state.dialogs.bruno_import.pending_parse = None;
    }
}

fn poll_parse(ctx: &egui::Context, dialog: &mut BrunoImportState) {
    let Some(receiver) = dialog.pending_parse.take() else {
        return;
    };
    match receiver.try_recv() {
        Ok(parsed) => {
            dialog.name = match &parsed {
                Ok(import) => import.collection.name.clone(),
                Err(_) => String::new(),
            };
            dialog.parsed = Some(parsed);
        }
        Err(TryRecvError::Empty) => {
            dialog.pending_parse = Some(receiver);
            ctx.request_repaint_after(Duration::from_millis(80));
        }
        Err(TryRecvError::Disconnected) => {
            dialog.parsed = Some(Err("Bruno import parser stopped unexpectedly".to_string()));
        }
    }
}

fn import_report_parts(
    root: &std::path::Path,
    dialog: &BrunoImportState,
    workspace: Option<&forge_core::store::Workspace>,
) -> Option<ImportReportParts> {
    let Some(Ok(import)) = dialog.parsed.as_ref() else {
        return None;
    };
    let request_v1 = root.join("project.json").is_file();
    let plan = request_v1.then(|| {
        forge_core::reqv1::plan_imported_collection(&import.collection, root, dialog.name.trim())
    });
    let mut skipped = import.collection.skipped.clone();
    let mut imported_environments = 0;
    if dialog.import_environments {
        for (environment, _) in &import.environments {
            let exists = if request_v1 {
                v1_environment_path(root, &environment.name).exists()
            } else {
                workspace.is_some_and(|workspace| {
                    workspace
                        .environments
                        .iter()
                        .any(|existing| existing.env.name == environment.name)
                })
            };
            if exists {
                skipped.push(format!(
                    "Environment \"{}\" already exists and will be skipped.",
                    environment.name
                ));
            } else {
                imported_environments += 1;
            }
        }
    }
    Some(ImportReportParts {
        created: plan
            .as_ref()
            .map(|plan| {
                plan.requests
                    .iter()
                    .map(|request| root.join(&request.path))
                    .collect()
            })
            .unwrap_or_default(),
        created_count: plan
            .as_ref()
            .map_or(import.collection.request_count(), |plan| {
                plan.requests.len()
            })
            + imported_environments,
        skipped,
        blocked: plan
            .as_ref()
            .map(|plan| plan.blocked.clone())
            .unwrap_or_default(),
        warnings: plan
            .as_ref()
            .map(|plan| plan.warnings.clone())
            .unwrap_or_default(),
        quarantined_count: import.collection.quarantine.len(),
    })
}

fn show_skipped(ui: &mut egui::Ui, skipped: &[String]) {
    if skipped.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.colored_label(
        ui.visuals().warn_fg_color,
        format!("{} item(s) can't be imported:", skipped.len()),
    );
    egui::ScrollArea::vertical()
        .id_salt("bruno_import-sa-1")
        .max_height(160.0)
        .show(ui, |ui| {
            for note in skipped {
                ui.weak(note);
            }
        });
}

fn do_import(root: &std::path::Path, dialog: &mut BrunoImportState) -> Result<String, String> {
    let Some(Ok(import)) = dialog.parsed.as_ref() else {
        return Err("nothing to import".to_string());
    };
    let name = dialog.name.trim();

    let plan = forge_core::reqv1::plan_imported_collection(&import.collection, root, name);
    let request_v1 = root.join("project.json").is_file();
    let count = if request_v1 {
        plan.requests.len()
    } else {
        import.collection.request_count()
    };
    let blocked = if request_v1 { plan.blocked.len() } else { 0 };
    let notes = import.collection.skipped.len() + if request_v1 { plan.warnings.len() } else { 0 };
    import_collection(root, &import.collection, name)?;

    let mut env_count = 0;
    if dialog.import_environments {
        for (env, secrets) in &import.environments {
            // Duplicate environment names are skipped rather than failing
            // the whole import — the collection is already on disk.
            if request_v1 {
                if v1_environment_path(root, &env.name).exists() {
                    continue;
                }
                import_v1_environment(root, &env.name, env, secrets)?;
                env_count += 1;
                continue;
            }
            match create_environment(root, &env.name) {
                Ok(file) => {
                    save_environment(&file, env).map_err(|e| e.to_string())?;
                    if !secrets.is_empty() {
                        save_secrets(&file, secrets).map_err(|e| e.to_string())?;
                    }
                    env_count += 1;
                }
                Err(e) => {
                    if !matches!(e, forge_core::store::StoreError::AlreadyExists(_)) {
                        return Err(e.to_string());
                    }
                }
            }
        }
    }

    Ok(format!(
        "Imported {count} request(s) and {env_count} environment(s) from Bruno; {blocked} request(s) blocked, {notes} conversion note(s) shown in the preview"
    ))
}
