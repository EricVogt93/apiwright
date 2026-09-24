//! Environment manager ("Manage..." in the right Environment panel + View
//! menu): create/delete environments and edit their variable tables,
//! including the gitignored secrets file for `secret: true` rows.
//!
//! Edits autosave: every row change is written straight to `<name>.env.json`
//! / `<name>.secrets.json` and the workspace is reloaded, the same
//! immediate-persistence model `panels::collections` uses for its CRUD
//! operations (there's no separate "dirty" draft to lose on close).

use std::path::PathBuf;

use egui::{TextEdit, Window};
use forge_core::reqv1::ProjectFileKind;
use serde_json::Value;

use forge_core::model::EnvVar;
use forge_core::store::{
    create_environment, save_environment, save_secrets, secrets_path, Workspace,
};

use crate::state::{AppState, StatusMessage};

/// Transient state of the environment manager, owned by
/// [`crate::dialogs::DialogManager`].
#[derive(Default)]
pub struct EnvEditorState {
    open: bool,
    selected: Option<String>,
    new_env_name: String,
    new_var_key: String,
    /// Environment name awaiting delete confirmation.
    pending_delete: Option<String>,
    /// Which secret rows currently have their value revealed in plain text.
    revealed: std::collections::HashSet<String>,
    request_v1_loaded: Option<String>,
    request_v1_revision: Option<String>,
    request_v1_text: String,
    request_v1_saved_text: String,
    request_v1_error: Option<String>,
    request_v1_close_prompt: bool,
    request_v1_delete_revision: Option<String>,
    request_v1_allow_broken_references: bool,
}

impl EnvEditorState {
    /// Open the manager, selecting `preferred` if given.
    pub fn open(&mut self, preferred: Option<String>) {
        self.open = true;
        self.selected = preferred;
        self.request_v1_loaded = None;
        self.request_v1_revision = None;
        self.request_v1_error = None;
        self.request_v1_close_prompt = false;
        self.request_v1_delete_revision = None;
        self.request_v1_allow_broken_references = false;
    }
}

/// Render the dialog (and its delete-confirmation popup) if open; no-op
/// otherwise.
pub fn show(ctx: &egui::Context, state: &mut AppState) {
    if !state.dialogs.env_editor.open {
        return;
    }
    if state.workspace.is_none() {
        if let Some(root) = state.assets.project_root() {
            show_request_v1(ctx, state, root);
            return;
        }
    }
    let Some(root) = state.workspace.as_ref().map(|w| w.root.clone()) else {
        state.dialogs.env_editor.open = false;
        state.status = Some(StatusMessage::error(
            "Open a workspace before managing environments",
        ));
        return;
    };

    let mut window_open = true;
    let mut new_env_clicked = false;
    let mut needs_reload = false;

    Window::new("Environments")
        .id(egui::Id::new("env-editor-dialog"))
        .collapsible(false)
        .resizable(true)
        .default_size([640.0, 440.0])
        .open(&mut window_open)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                render_env_list(ui, state, &mut new_env_clicked);
                ui.separator();
                if render_selected_env(ui, state) {
                    needs_reload = true;
                }
            });
        });

    if new_env_clicked {
        let n = state
            .workspace
            .as_ref()
            .map(|w| w.environments.len())
            .unwrap_or(0);
        let name = format!("Environment {}", n + 1);
        match create_environment(&root, &name) {
            Ok(_) => {
                needs_reload = true;
                state.dialogs.env_editor.selected = Some(name);
            }
            Err(e) => state.status = Some(StatusMessage::error(e.to_string())),
        }
    }

    if needs_reload {
        reload_workspace(state);
    }

    show_delete_confirm(ctx, state);

    if !window_open {
        state.dialogs.env_editor.open = false;
    }
}

fn request_v1_environment_path(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("Environment names may contain only letters, numbers, '-' and '_'".to_string());
    }
    Ok(format!("environments/{name}.json"))
}

fn list_request_v1_environments(root: &std::path::Path) -> Result<Vec<String>, String> {
    let directory = root.join("environments");
    match std::fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "refusing to list environments through symbolic link {}",
                directory.display()
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!("{} is not a directory", directory.display()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot inspect {}: {error}", directory.display())),
    }

    let entries = std::fs::read_dir(&directory)
        .map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read environment entry: {error}"))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json")
            || path.to_string_lossy().ends_with(".secrets.json")
        {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "refusing to list environment through symbolic link {}",
                path.display()
            ));
        }
        if !metadata.is_file() {
            continue;
        }
        if let Some(name) = path.file_stem().and_then(|value| value.to_str()) {
            if request_v1_environment_path(name).is_ok() {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

fn read_request_v1_environment(
    root: &std::path::Path,
    name: &str,
) -> Result<(String, Value), String> {
    let path = request_v1_environment_path(name)?;
    forge_core::reqv1::read_project_file(root, ProjectFileKind::Environment, &path)
        .map(|snapshot| (snapshot.revision, snapshot.content))
}

fn create_request_v1_environment(root: &std::path::Path, name: &str) -> Result<String, String> {
    let path = request_v1_environment_path(name)?;
    forge_core::reqv1::write_project_file(
        root,
        ProjectFileKind::Environment,
        &path,
        "new",
        serde_json::json!({}),
    )
    .map(|snapshot| snapshot.revision)
}

fn save_request_v1_environment(
    root: &std::path::Path,
    name: &str,
    expected_revision: &str,
    text: &str,
) -> Result<String, String> {
    let path = request_v1_environment_path(name)?;
    let content: Value =
        serde_json::from_str(text).map_err(|error| format!("invalid environment JSON: {error}"))?;
    if !content.is_object() {
        return Err("environment JSON must be an object".to_string());
    }
    forge_core::reqv1::write_project_file(
        root,
        ProjectFileKind::Environment,
        &path,
        expected_revision,
        content,
    )
    .map(|snapshot| snapshot.revision)
}

fn delete_request_v1_environment(
    root: &std::path::Path,
    name: &str,
    expected_revision: &str,
    allow_broken_references: bool,
) -> Result<(), String> {
    let path = request_v1_environment_path(name)?;
    forge_core::reqv1::delete_project_file(
        root,
        ProjectFileKind::Environment,
        &path,
        expected_revision,
        allow_broken_references,
    )
}

fn show_request_v1(ctx: &egui::Context, state: &mut AppState, root: PathBuf) {
    let names = match list_request_v1_environments(&root) {
        Ok(names) => names,
        Err(error) => {
            state.dialogs.env_editor.request_v1_error = Some(error);
            Vec::new()
        }
    };
    if state
        .dialogs
        .env_editor
        .selected
        .as_ref()
        .is_some_and(|selected| !names.contains(selected))
    {
        state.dialogs.env_editor.selected = names.first().cloned();
    }
    let selected = state.dialogs.env_editor.selected.clone();
    if state.dialogs.env_editor.request_v1_loaded != selected {
        match selected
            .as_deref()
            .map(|name| read_request_v1_environment(&root, name))
        {
            Some(Ok((revision, content))) => {
                state.dialogs.env_editor.request_v1_text =
                    serde_json::to_string_pretty(&content).unwrap_or_default();
                state.dialogs.env_editor.request_v1_saved_text =
                    state.dialogs.env_editor.request_v1_text.clone();
                state.dialogs.env_editor.request_v1_revision = Some(revision);
                state.dialogs.env_editor.request_v1_loaded = selected.clone();
                state.dialogs.env_editor.request_v1_error = None;
            }
            Some(Err(error)) => {
                state.dialogs.env_editor.request_v1_text.clear();
                state.dialogs.env_editor.request_v1_saved_text.clear();
                state.dialogs.env_editor.request_v1_revision = None;
                state.dialogs.env_editor.request_v1_loaded = selected.clone();
                state.dialogs.env_editor.request_v1_error = Some(error);
            }
            None => {
                state.dialogs.env_editor.request_v1_text.clear();
                state.dialogs.env_editor.request_v1_saved_text.clear();
                state.dialogs.env_editor.request_v1_revision = None;
                state.dialogs.env_editor.request_v1_loaded = None;
            }
        }
    }

    let mut window_open = true;
    let mut create_name = None;
    let mut delete_name = None;
    let mut reload_selected = false;
    let mut save_selected = false;
    let mut close_after_save = false;
    let draft_dirty =
        state.dialogs.env_editor.request_v1_text != state.dialogs.env_editor.request_v1_saved_text;
    Window::new("Environments")
        .id(egui::Id::new("env-editor-dialog"))
        .collapsible(false)
        .resizable(true)
        .default_size([700.0, 480.0])
        .open(&mut window_open)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(180.0);
                    for name in &names {
                        let active =
                            state.dialogs.env_editor.selected.as_deref() == Some(name.as_str());
                        let response = if draft_dirty && !active {
                            ui.add_enabled(false, egui::Button::new(name))
                        } else {
                            ui.selectable_label(active, name)
                        };
                        if response.clicked() {
                            state.dialogs.env_editor.selected = Some(name.clone());
                        }
                    }
                    if draft_dirty {
                        ui.weak("Save or reload this draft before changing environments.");
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            !draft_dirty,
                            TextEdit::singleline(&mut state.dialogs.env_editor.new_env_name)
                                .hint_text("New name"),
                        );
                        if ui
                            .add_enabled(!draft_dirty, egui::Button::new("+"))
                            .on_hover_text("Create environment")
                            .clicked()
                        {
                            create_name =
                                Some(state.dialogs.env_editor.new_env_name.trim().to_string());
                        }
                    });
                    if let Some(name) = state.dialogs.env_editor.selected.clone() {
                        if ui
                            .add_enabled(!draft_dirty, egui::Button::new("Delete…"))
                            .clicked()
                        {
                            delete_name = Some(name);
                        }
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.set_min_width(440.0);
                    if let Some(name) = state.dialogs.env_editor.selected.as_deref() {
                        ui.heading(name);
                        ui.label("JSON object stored in environments/<name>.json");
                        ui.label(
                            "Keep credentials in the local secret store, not in this project file.",
                        );
                        ui.add(
                            TextEdit::multiline(&mut state.dialogs.env_editor.request_v1_text)
                                .code_editor()
                                .desired_rows(18),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Reload").clicked() {
                                reload_selected = true;
                            }
                            if ui.button("Save environment").clicked() {
                                save_selected = true;
                            }
                        });
                    } else {
                        ui.weak("Create an environment to add project variables.");
                    }
                    if let Some(error) = &state.dialogs.env_editor.request_v1_error {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                });
            });
        });

    if !window_open {
        if draft_dirty {
            state.dialogs.env_editor.request_v1_close_prompt = true;
        } else {
            state.dialogs.env_editor.open = false;
        }
    }
    if state.dialogs.env_editor.request_v1_close_prompt {
        let mut prompt_open = true;
        let mut discard_close = false;
        Window::new("Unsaved environment")
            .id(egui::Id::new("env-editor-unsaved-confirm"))
            .collapsible(false)
            .resizable(false)
            .open(&mut prompt_open)
            .show(ctx, |ui| {
                ui.label("Save this environment draft before closing?");
                ui.horizontal(|ui| {
                    if ui.button("Save and close").clicked() {
                        save_selected = true;
                        close_after_save = true;
                    }
                    if ui.button("Discard and close").clicked() {
                        discard_close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        state.dialogs.env_editor.request_v1_close_prompt = false;
                        state.dialogs.env_editor.open = true;
                    }
                });
            });
        if discard_close {
            state.dialogs.env_editor.request_v1_text =
                state.dialogs.env_editor.request_v1_saved_text.clone();
            state.dialogs.env_editor.request_v1_close_prompt = false;
            state.dialogs.env_editor.open = false;
        } else if !prompt_open {
            state.dialogs.env_editor.request_v1_close_prompt = false;
            state.dialogs.env_editor.open = true;
        }
    }
    if let Some(name) = create_name {
        let result = create_request_v1_environment(&root, &name).map(|revision| (name, revision));
        match result {
            Ok((name, revision)) => {
                state.dialogs.env_editor.selected = Some(name.clone());
                state.dialogs.env_editor.new_env_name.clear();
                state.dialogs.env_editor.request_v1_loaded = Some(name);
                state.dialogs.env_editor.request_v1_revision = Some(revision);
                state.dialogs.env_editor.request_v1_text = "{}".to_string();
                state.dialogs.env_editor.request_v1_saved_text = "{}".to_string();
                state.dialogs.env_editor.request_v1_error = None;
                state.assets.load(root.clone());
            }
            Err(error) => state.dialogs.env_editor.request_v1_error = Some(error),
        }
    }
    if reload_selected {
        if let Some(name) = state.dialogs.env_editor.selected.as_deref() {
            match read_request_v1_environment(&root, name) {
                Ok((revision, content)) => {
                    state.dialogs.env_editor.request_v1_text =
                        serde_json::to_string_pretty(&content).unwrap_or_default();
                    state.dialogs.env_editor.request_v1_saved_text =
                        state.dialogs.env_editor.request_v1_text.clone();
                    state.dialogs.env_editor.request_v1_revision = Some(revision);
                    state.dialogs.env_editor.request_v1_error = None;
                }
                Err(error) => state.dialogs.env_editor.request_v1_error = Some(error),
            }
        }
    }
    if save_selected {
        let result = state
            .dialogs
            .env_editor
            .selected
            .as_deref()
            .ok_or_else(|| "select an environment first".to_string())
            .and_then(|name| {
                let expected = state
                    .dialogs
                    .env_editor
                    .request_v1_revision
                    .as_deref()
                    .ok_or_else(|| "reload this environment before saving".to_string())?;
                save_request_v1_environment(
                    &root,
                    name,
                    expected,
                    &state.dialogs.env_editor.request_v1_text,
                )
            });
        match result {
            Ok(revision) => {
                state.dialogs.env_editor.request_v1_revision = Some(revision);
                state.dialogs.env_editor.request_v1_saved_text =
                    state.dialogs.env_editor.request_v1_text.clone();
                state.dialogs.env_editor.request_v1_error = None;
                state.assets.load(root.clone());
                state.status = Some(StatusMessage::info("Environment saved"));
                if close_after_save {
                    state.dialogs.env_editor.request_v1_close_prompt = false;
                    state.dialogs.env_editor.open = false;
                }
            }
            Err(error) => state.dialogs.env_editor.request_v1_error = Some(error),
        }
    }
    if let Some(name) = delete_name {
        state.dialogs.env_editor.request_v1_delete_revision =
            state.dialogs.env_editor.request_v1_revision.clone();
        state.dialogs.env_editor.request_v1_allow_broken_references = false;
        state.dialogs.env_editor.pending_delete = Some(name);
    }

    show_delete_confirm(ctx, state);
}

/// Left column: the environment list, "+ New Environment" and "Delete".
fn render_env_list(ui: &mut egui::Ui, state: &mut AppState, new_env_clicked: &mut bool) {
    ui.vertical(|ui| {
        ui.set_width(160.0);
        let names: Vec<String> = state
            .workspace
            .as_ref()
            .map(|w| w.environments.iter().map(|e| e.env.name.clone()).collect())
            .unwrap_or_default();
        egui::ScrollArea::vertical()
            .id_salt("env-list-scroll")
            .max_height(300.0)
            .show(ui, |ui| {
                for name in &names {
                    let selected =
                        state.dialogs.env_editor.selected.as_deref() == Some(name.as_str());
                    if ui.selectable_label(selected, name).clicked() {
                        state.dialogs.env_editor.selected = Some(name.clone());
                    }
                }
            });
        ui.add_space(6.0);
        if ui.button("+ New Environment").clicked() {
            *new_env_clicked = true;
        }
        if let Some(sel) = state.dialogs.env_editor.selected.clone() {
            if ui.button("Delete").clicked() {
                state.dialogs.env_editor.pending_delete = Some(sel);
            }
        }
    });
}

/// Right column: the selected environment's variable table. Returns `true`
/// if a variable was added/edited/removed (the caller reloads the workspace
/// afterwards, once, outside any nested closure borrow of `state`).
fn render_selected_env(ui: &mut egui::Ui, state: &mut AppState) -> bool {
    let mut changed = false;
    ui.vertical(|ui| {
        ui.set_min_width(420.0);
        let Some(name) = state.dialogs.env_editor.selected.clone() else {
            ui.weak("Select an environment, or create a new one.");
            return;
        };
        let Some(loaded) = state
            .workspace
            .as_ref()
            .and_then(|w| w.environment(&name))
            .cloned()
        else {
            ui.weak("Environment not found.");
            return;
        };
        let mut env = loaded.env.clone();
        let mut secrets = loaded.secrets.clone();

        egui::ScrollArea::vertical()
            .id_salt("env-vars-scroll")
            .max_height(320.0)
            .show(ui, |ui| {
                egui::Grid::new("env-editor-grid")
                    .num_columns(5)
                    .striped(true)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.strong("Name");
                        ui.strong("Value");
                        ui.strong("Secret");
                        ui.strong("Description");
                        ui.strong("");
                        ui.end_row();

                        let mut remove: Option<String> = None;
                        for (key, var) in env.variables.iter_mut() {
                            ui.monospace(key.as_str());
                            if var.secret {
                                let revealed = state.dialogs.env_editor.revealed.contains(key);
                                let mut value = secrets.get(key).cloned().unwrap_or_default();
                                if ui
                                    .add(TextEdit::singleline(&mut value).password(!revealed))
                                    .changed()
                                {
                                    secrets.insert(key.clone(), value);
                                    changed = true;
                                }
                            } else {
                                let mut value = var.value.clone().unwrap_or_default();
                                if ui.text_edit_singleline(&mut value).changed() {
                                    var.value = Some(value);
                                    changed = true;
                                }
                            }
                            let mut secret = var.secret;
                            if ui.checkbox(&mut secret, "").changed() {
                                if secret {
                                    var.value = None;
                                } else {
                                    secrets.remove(key);
                                }
                                var.secret = secret;
                                changed = true;
                            }
                            if var.secret {
                                let mut revealed = state.dialogs.env_editor.revealed.contains(key);
                                if ui.checkbox(&mut revealed, "show").changed() {
                                    if revealed {
                                        state.dialogs.env_editor.revealed.insert(key.clone());
                                    } else {
                                        state.dialogs.env_editor.revealed.remove(key);
                                    }
                                }
                            } else {
                                ui.label("");
                            }
                            if ui.text_edit_singleline(&mut var.description).changed() {
                                changed = true;
                            }
                            if ui.small_button("\u{2715}").clicked() {
                                remove = Some(key.clone());
                            }
                            ui.end_row();
                        }
                        if let Some(key) = remove {
                            env.variables.remove(&key);
                            secrets.remove(&key);
                            changed = true;
                        }
                    });
            });

        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut state.dialogs.env_editor.new_var_key);
            let key = state.dialogs.env_editor.new_var_key.trim().to_string();
            if ui
                .add_enabled(!key.is_empty(), egui::Button::new("+ Add variable"))
                .clicked()
            {
                env.variables.insert(key, EnvVar::default());
                state.dialogs.env_editor.new_var_key.clear();
                changed = true;
            }
        });

        if changed {
            if let Err(e) = save_environment(&loaded.file, &env) {
                state.status = Some(StatusMessage::error(e.to_string()));
            } else if let Err(e) = save_secrets(&loaded.file, &secrets) {
                state.status = Some(StatusMessage::error(e.to_string()));
            }
        }
    });
    changed
}

fn show_delete_confirm(ctx: &egui::Context, state: &mut AppState) {
    let Some(name) = state.dialogs.env_editor.pending_delete.clone() else {
        return;
    };
    if state.workspace.is_none() {
        let Some(root) = state.assets.project_root() else {
            state.dialogs.env_editor.pending_delete = None;
            return;
        };
        let mut confirmed = false;
        let mut cancelled = false;
        let mut keep_open = true;
        Window::new("Delete Environment")
            .id(egui::Id::new("env-editor-delete-confirm"))
            .collapsible(false)
            .resizable(false)
            .open(&mut keep_open)
            .show(ctx, |ui| {
                ui.label(format!("Delete \"{name}\"? This cannot be undone."));
                ui.checkbox(
                    &mut state.dialogs.env_editor.request_v1_allow_broken_references,
                    "Allow requests or folders to keep referencing it",
                );
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        confirmed = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancelled = true;
                    }
                });
            });

        if confirmed {
            let result = {
                let revision = state
                    .dialogs
                    .env_editor
                    .request_v1_delete_revision
                    .as_deref()
                    .ok_or_else(|| "reload this environment before deleting it".to_string());
                revision.and_then(|revision| {
                    delete_request_v1_environment(
                        &root,
                        &name,
                        revision,
                        state.dialogs.env_editor.request_v1_allow_broken_references,
                    )
                })
            };
            match result {
                Ok(()) => {
                    if state.active_env.as_deref() == Some(name.as_str()) {
                        state.active_env = None;
                    }
                    state.dialogs.env_editor.selected = None;
                    state.dialogs.env_editor.request_v1_loaded = None;
                    state.dialogs.env_editor.request_v1_revision = None;
                    state.dialogs.env_editor.request_v1_delete_revision = None;
                    state.dialogs.env_editor.request_v1_error = None;
                    state.assets.load(root.clone());
                }
                Err(error) => {
                    state.dialogs.env_editor.request_v1_error = Some(error);
                }
            }
        }
        if confirmed || cancelled || !keep_open {
            state.dialogs.env_editor.pending_delete = None;
        }
        return;
    }

    let mut confirmed = false;
    let mut cancelled = false;
    let mut keep_open = true;

    Window::new("Delete Environment")
        .id(egui::Id::new("env-editor-delete-confirm"))
        .collapsible(false)
        .resizable(false)
        .open(&mut keep_open)
        .show(ctx, |ui| {
            ui.label(format!("Delete \"{name}\"? This cannot be undone."));
            ui.horizontal(|ui| {
                if ui.button("Delete").clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });

    if confirmed {
        let file = state
            .workspace
            .as_ref()
            .and_then(|w| w.environment(&name))
            .map(|e| e.file.clone());
        if let Some(file) = file {
            let _ = std::fs::remove_file(secrets_path(&file));
            match std::fs::remove_file(&file) {
                Ok(()) => {
                    if state.active_env.as_deref() == Some(name.as_str()) {
                        state.active_env = None;
                    }
                    if state.dialogs.env_editor.selected.as_deref() == Some(name.as_str()) {
                        state.dialogs.env_editor.selected = None;
                    }
                    reload_workspace(state);
                }
                Err(e) => state.status = Some(StatusMessage::error(e.to_string())),
            }
        }
    }
    if confirmed || cancelled || !keep_open {
        state.dialogs.env_editor.pending_delete = None;
    }
}

fn reload_workspace(state: &mut AppState) {
    let Some(root) = state.workspace.as_ref().map(|w| w.root.clone()) else {
        return;
    };
    match Workspace::load(&root) {
        Ok(ws) => state.workspace = Some(ws),
        Err(e) => state.status = Some(StatusMessage::error(e.to_string())),
    }
}

#[cfg(test)]
mod request_v1_tests {
    use super::*;

    #[test]
    fn request_v1_manager_opens_and_edits_environment_files_safely() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("project.json"), r#"{"formatVersion":1}"#).unwrap();
        let revision = create_request_v1_environment(root.path(), "dev").unwrap();

        let mut state = AppState::new();
        state.assets.load(root.path().to_path_buf());
        state.dialogs.env_editor.open(Some("dev".to_string()));
        let context = egui::Context::default();
        let _ = context.run_ui(egui::RawInput::default(), |ui| show(ui.ctx(), &mut state));
        assert!(state.dialogs.env_editor.open);
        assert!(state.status.is_none());
        assert_eq!(
            state.dialogs.env_editor.request_v1_loaded.as_deref(),
            Some("dev")
        );

        let updated = save_request_v1_environment(
            root.path(),
            "dev",
            &revision,
            r#"{"baseUrl":"https://api.example.test"}"#,
        )
        .unwrap();
        assert_ne!(updated, revision);
        assert!(save_request_v1_environment(
            root.path(),
            "dev",
            &revision,
            r#"{"baseUrl":"https://stale.example.test"}"#,
        )
        .unwrap_err()
        .contains("revision_conflict"));
        assert!(list_request_v1_environments(root.path())
            .unwrap()
            .contains(&"dev".to_string()));

        std::fs::write(root.path().join(".forge-environment"), "dev\n").unwrap();
        assert!(
            delete_request_v1_environment(root.path(), "dev", &updated, false)
                .unwrap_err()
                .contains("selected by a project scope")
        );
        delete_request_v1_environment(root.path(), "dev", &updated, true).unwrap();
        assert!(!root.path().join("environments/dev.json").exists());
    }
}
