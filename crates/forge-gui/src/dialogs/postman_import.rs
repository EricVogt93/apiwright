//! Import Postman (File menu): pick a Postman collection or environment
//! export (both are .json — the parser detects which one it is), preview
//! what will be imported, which scripts will remain quarantined, and which
//! unsupported features are dropped before writing into the workspace.

use std::path::{Path, PathBuf};

use egui::Window;

use forge_core::convert::{
    load_import_quarantine, parse_postman, parse_postman_environment, sync_import_quarantine,
    ImportedCollection, ImportedItem, PostmanError,
};
use forge_core::model::{Environment, FolderMeta, SecretValues};
use forge_core::store::{
    create_collection, create_environment, create_folder, create_request, save_collection_meta,
    save_environment, save_folder_meta, save_secrets, Workspace,
};

use crate::state::{AppState, StatusMessage};

/// What the picked file turned out to contain.
#[allow(clippy::large_enum_variant)] // one short-lived instance per dialog
enum Parsed {
    Collection(ImportedCollection),
    Environment(Environment, SecretValues),
}

/// Transient state of the Postman-import dialog, owned by
/// [`crate::dialogs::DialogManager`].
#[derive(Default)]
pub struct PostmanImportState {
    open: bool,
    parsed: Option<Result<Parsed, String>>,
    name: String,
}

impl PostmanImportState {
    /// Open a file picker for a Postman JSON export and parse it
    /// immediately. Collection vs environment is auto-detected.
    pub fn open(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Postman export", &["json"])
            .pick_file()
        else {
            return;
        };
        self.parsed = Some(parse_file(&path));
        self.name = match &self.parsed {
            Some(Ok(Parsed::Collection(c))) => c.name.clone(),
            Some(Ok(Parsed::Environment(e, _))) => e.name.clone(),
            _ => String::new(),
        };
        self.open = true;
    }
}

fn parse_file(path: &PathBuf) -> Result<Parsed, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    match parse_postman(&text) {
        Ok(c) => Ok(Parsed::Collection(c)),
        // Only fall through to the environment parser when the file just
        // isn't a collection; real JSON errors should surface as-is.
        Err(PostmanError::NotACollection) => match parse_postman_environment(&text) {
            Ok((env, secrets)) => Ok(Parsed::Environment(env, secrets)),
            Err(_) => Err("Not a Postman collection or environment export".to_string()),
        },
        Err(e) => Err(e.to_string()),
    }
}

/// Render the dialog if open; no-op otherwise.
pub fn show(ctx: &egui::Context, state: &mut AppState) {
    if !state.dialogs.postman_import.open {
        return;
    }
    let Some(root) = state
        .workspace
        .as_ref()
        .map(|workspace| workspace.root.clone())
        .or_else(|| state.assets.project_root())
    else {
        state.dialogs.postman_import.open = false;
        state.status = Some(StatusMessage::error("Open a workspace before importing"));
        return;
    };

    let mut window_open = true;
    let mut import_clicked = false;
    let mut cancel_clicked = false;

    Window::new("Import Postman")
        .id(egui::Id::new("postman-import-dialog"))
        .collapsible(false)
        .resizable(true)
        .default_size([560.0, 420.0])
        .open(&mut window_open)
        .show(ctx, |ui| {
            let dialog = &mut state.dialogs.postman_import;
            match &dialog.parsed {
                None => {
                    ui.weak("No file loaded.");
                }
                Some(Err(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                Some(Ok(Parsed::Collection(import))) => {
                    ui.label(format!(
                        "Collection \"{}\" — {} request(s), {} public variable(s), {} secret variable(s)",
                        import.name,
                        import.request_count(),
                        import.variables.len(),
                        import.secret_variables.len()
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Import as:");
                        ui.text_edit_singleline(&mut dialog.name);
                    });
                    if !import.quarantine.is_empty() {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!(
                                "{} JavaScript event(s) will be preserved in import quarantine and remain non-executable.",
                                import.quarantine.len()
                            ),
                        );
                    }
                    if root.join("project.json").is_file() {
                        let plan = forge_core::reqv1::plan_imported_collection(
                            import,
                            &root,
                            dialog.name.trim(),
                        );
                        ui.weak(format!(
                            "Request-v1 preview: {} request(s) ready, {} blocked; {} secret variable(s) stay outside committed files.",
                            plan.requests.len(),
                            plan.blocked.len(),
                            import.secret_variables.len()
                        ));
                        show_blocked(ui, &plan.blocked);
                        show_warnings(ui, &plan.warnings);
                    }
                    show_skipped(ui, &import.skipped);
                }
                Some(Ok(Parsed::Environment(env, secrets))) => {
                    ui.label(format!(
                        "Environment \"{}\" — {} variable(s), {} secret value(s)",
                        env.name,
                        env.variables.len(),
                        secrets.len()
                    ));
                    if !secrets.is_empty() {
                        if root.join("project.json").is_file() {
                            ui.weak(
                                "Secret values go into the gitignored project .env.local file, \
                                 never into the committed environment JSON. Reference them as ${secret.NAME}.",
                            );
                        } else {
                            ui.weak(
                                "Secret values go to the gitignored .secrets.json sibling file, \
                                 never into the committed environment file.",
                            );
                        }
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Import as:");
                        ui.text_edit_singleline(&mut dialog.name);
                    });
                    if root.join("project.json").is_file()
                        && v1_environment_path(&root, dialog.name.trim()).exists()
                    {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "That Request-v1 environment file already exists.",
                        );
                    }
                }
            }

            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cancel_clicked = true;
                }
                let can_import = match &state.dialogs.postman_import.parsed {
                    Some(Ok(Parsed::Collection(import)))
                        if root.join("project.json").is_file() =>
                    {
                        let name = state.dialogs.postman_import.name.trim();
                        let plan = forge_core::reqv1::plan_imported_collection(
                                import,
                                &root,
                                name,
                            );
                        !name.is_empty()
                            && (!plan.requests.is_empty()
                                || plan.environment.is_some()
                                || !import.quarantine.is_empty())
                    }
                    Some(Ok(Parsed::Environment(_, _)))
                        if root.join("project.json").is_file() =>
                    {
                        let name = state.dialogs.postman_import.name.trim();
                        !name.is_empty() && !v1_environment_path(&root, name).exists()
                    }
                    Some(Ok(_)) => !state.dialogs.postman_import.name.trim().is_empty(),
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
        let result = state
            .dialogs
            .quarantine
            .save_pending()
            .and_then(|()| do_import(&root, &mut state.dialogs.postman_import));
        match result {
            Err(e) => state.status = Some(StatusMessage::error(e)),
            Ok(msg) => {
                state.dialogs.postman_import.open = false;
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
        state.dialogs.postman_import.open = false;
    }
}

pub(super) fn show_skipped(ui: &mut egui::Ui, skipped: &[String]) {
    if skipped.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.colored_label(
        ui.visuals().warn_fg_color,
        format!("{} item(s) can't be imported:", skipped.len()),
    );
    egui::ScrollArea::vertical()
        .id_salt("postman_import-sa-1")
        .max_height(160.0)
        .show(ui, |ui| {
            for note in skipped {
                ui.weak(note);
            }
        });
}

pub(super) fn show_blocked(ui: &mut egui::Ui, blocked: &[String]) {
    if blocked.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.colored_label(
        ui.visuals().warn_fg_color,
        format!("{} request(s) will be skipped:", blocked.len()),
    );
    egui::ScrollArea::vertical()
        .id_salt("postman_import-v1-blocked")
        .max_height(120.0)
        .show(ui, |ui| {
            for note in blocked {
                ui.weak(note);
            }
        });
}

pub(super) fn show_warnings(ui: &mut egui::Ui, warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    ui.add_space(4.0);
    ui.colored_label(
        ui.visuals().warn_fg_color,
        format!(
            "{} detail(s) are not represented in request-v1:",
            warnings.len()
        ),
    );
    egui::ScrollArea::vertical()
        .id_salt("import-v1-warnings")
        .max_height(90.0)
        .show(ui, |ui| {
            for warning in warnings {
                ui.weak(warning);
            }
        });
}

fn do_import(root: &Path, dialog: &mut PostmanImportState) -> Result<String, String> {
    let name = dialog.name.trim().to_string();
    match dialog.parsed.as_ref() {
        Some(Ok(Parsed::Collection(import))) => {
            let plan = forge_core::reqv1::plan_imported_collection(import, root, &name);
            let count = if root.join("project.json").is_file() {
                plan.requests.len()
            } else {
                import.request_count()
            };
            let request_v1 = root.join("project.json").is_file();
            let blocked = if request_v1 { plan.blocked.len() } else { 0 };
            let notes = if request_v1 {
                plan.warnings.len() + import.skipped.len()
            } else {
                import.skipped.len()
            };
            import_collection(root, import, &name)?;
            Ok(format!(
                "Imported {count} request(s) from Postman; {blocked} request(s) blocked, {notes} conversion note(s) shown in the preview"
            ))
        }
        Some(Ok(Parsed::Environment(env, secrets))) => {
            if root.join("project.json").is_file() {
                let secret_count = secrets.values().filter(|value| !value.is_empty()).count();
                import_v1_environment(root, &name, env, secrets)?;
                return Ok(format!(
                    "Imported Request-v1 environment \"{name}\"; {secret_count} secret value(s) are in .env.local and use ${{secret.NAME}} references"
                ));
            }
            let file = create_environment(root, &name).map_err(|e| e.to_string())?;
            let mut env = env.clone();
            env.name = name.clone();
            save_environment(&file, &env).map_err(|e| e.to_string())?;
            if !secrets.is_empty() {
                save_secrets(&file, secrets).map_err(|e| e.to_string())?;
            }
            Ok(format!("Imported Postman environment \"{name}\""))
        }
        _ => Err("nothing to import".to_string()),
    }
}

pub(crate) fn import_collection(
    root: &Path,
    import: &ImportedCollection,
    name: &str,
) -> Result<(), String> {
    load_import_quarantine(root).map_err(|error| error.to_string())?;
    if root.join("project.json").is_file() {
        import_collection_v1(root, import, name)?;
        sync_import_quarantine(root, name, import.quarantine.iter().cloned())
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    let col_dir = create_collection(root, name).map_err(|e| e.to_string())?;
    let result = (|| {
        let order = write_items(&col_dir, &import.items)?;
        let mut meta = forge_core::model::CollectionMeta::new(name);
        meta.description = import.description.clone();
        meta.variables = import.variables.clone();
        meta.auth = import.auth.clone();
        meta.hooks = import.hooks.clone();
        meta.order = order;
        save_collection_meta(&col_dir, &meta).map_err(|e| e.to_string())?;
        sync_import_quarantine(root, name, import.quarantine.iter().cloned())
            .map_err(|error| error.to_string())?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&col_dir);
        return Err(error);
    }
    Ok(())
}

fn import_collection_v1(
    root: &Path,
    import: &ImportedCollection,
    name: &str,
) -> Result<(), String> {
    let plan = forge_core::reqv1::plan_imported_collection(import, root, name);
    if plan.requests.is_empty() && plan.environment.is_none() && import.quarantine.is_empty() {
        return Err(format!(
            "No requests can be imported without losing data{}",
            plan.blocked
                .first()
                .map(|blocked| format!(": {blocked}"))
                .unwrap_or_default()
        ));
    }

    let mut written = Vec::new();
    let mut environment_written = false;
    let environment_path = plan
        .environment
        .as_ref()
        .map(|environment| root.join(&environment.path));
    let result = (|| {
        if let Some(environment) = &plan.environment {
            let parent = environment
                .path
                .parent()
                .map(|parent| root.join(parent))
                .ok_or_else(|| "environment path has no parent".to_string())?;
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            forge_core::reqv1::write_project_file(
                root,
                forge_core::reqv1::ProjectFileKind::Environment,
                &environment.path.to_string_lossy(),
                "new",
                environment.document.clone(),
            )
            .map_err(|error| error.to_string())?;
            environment_written = true;
        }
        for request in &plan.requests {
            let path = root.join(&request.path);
            let parent = path
                .parent()
                .ok_or_else(|| "request path has no parent".to_string())?;
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            forge_core::reqv1::save_request_document(
                &path,
                request.document.clone(),
                forge_core::reqv1::AssertionDocument::default(),
                forge_core::reqv1::HookDocument::default(),
                true,
            )
            .map_err(|error| error.to_string())?;
            written.push(path);
        }
        {
            let secret_values = plan
                .secrets
                .iter()
                .filter(|(_, value)| !value.is_empty())
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<forge_core::model::SecretValues>();
            if !secret_values.is_empty() {
                forge_core::reqv1::save_file_secrets(root, &secret_values)?;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        for path in written {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(forge_core::reqv1::assertions_path(&path));
            let _ = std::fs::remove_file(forge_core::reqv1::hooks_path(&path));
        }
        if let Some(path) = environment_path.filter(|_| environment_written) {
            let _ = std::fs::remove_file(&path);
        }
        return Err(error);
    }
    Ok(())
}

pub(super) fn import_v1_environment(
    root: &Path,
    name: &str,
    environment: &forge_core::model::Environment,
    secrets: &forge_core::model::SecretValues,
) -> Result<PathBuf, String> {
    let path = v1_environment_path(root, name);
    let mut values = serde_json::Map::new();
    for (key, variable) in &environment.variables {
        if !variable.secret {
            if let Some(value) = &variable.value {
                values.insert(key.clone(), serde_json::Value::String(value.clone()));
            }
        }
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "environment path escapes the project".to_string())?;
    forge_core::reqv1::write_project_file(
        root,
        forge_core::reqv1::ProjectFileKind::Environment,
        &relative.to_string_lossy(),
        "new",
        serde_json::Value::Object(values),
    )
    .map_err(|error| error.to_string())?;

    let secret_values = secrets
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<forge_core::model::SecretValues>();
    if !secret_values.is_empty() {
        if let Err(error) = forge_core::reqv1::save_file_secrets(root, &secret_values) {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
    }
    Ok(path)
}

pub(super) fn v1_environment_path(root: &Path, name: &str) -> PathBuf {
    root.join("environments")
        .join(format!("{}.json", safe_file_slug(name)))
}

fn safe_file_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            separator = false;
        } else if !slug.is_empty() && !separator {
            slug.push('-');
            separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "environment".to_string()
    } else {
        slug
    }
}

/// Write folders/requests into `dir`, returning the child entry names in
/// Postman order for the parent's `order` array.
fn write_items(dir: &Path, items: &[ImportedItem]) -> Result<Vec<String>, String> {
    let mut order = Vec::new();
    for item in items {
        match item {
            ImportedItem::Request(def) => {
                let file = create_request(dir, def).map_err(|e| e.to_string())?;
                order.push(file_name(&file));
            }
            ImportedItem::Folder {
                name,
                description,
                auth,
                hooks,
                items,
            } => {
                let sub = create_folder(dir, name).map_err(|e| e.to_string())?;
                let sub_order = write_items(&sub, items)?;
                let meta = FolderMeta {
                    name: name.clone(),
                    description: description.clone(),
                    auth: auth.clone(),
                    hooks: hooks.clone(),
                    order: sub_order,
                    ..FolderMeta::default()
                };
                save_folder_meta(&sub, &meta).map_err(|e| e.to_string())?;
                order.push(file_name(&sub));
            }
        }
    }
    Ok(order)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(crate) fn reload_workspace(state: &mut AppState) -> Result<(), String> {
    let Some(root) = state
        .workspace
        .as_ref()
        .map(|workspace| workspace.root.clone())
        .or_else(|| state.assets.project_root())
    else {
        return Err("No workspace is open".to_string());
    };
    state.assets.load(root.clone());
    if state.workspace.is_some() {
        let workspace = Workspace::load(&root).map_err(|error| error.to_string())?;
        state.workspace = Some(workspace);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use forge_core::store::Workspace;

    use super::*;

    /// The core converter owns the loss report; this verifies that request-v1
    /// workspaces receive executable v1 requests and environment variables.
    #[test]
    fn import_collection_writes_request_v1_tree_and_environment() {
        let fixture = include_str!("../../../forge-core/tests/fixtures/postman_collection.json");
        let mut import = parse_postman(fixture).expect("fixture should parse");
        // Collection hooks use Postman's pm API and are previewed as blocked.
        import.hooks = Default::default();

        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::create(dir.path(), "WS").expect("create workspace");

        import_collection(&ws.root, &import, "Payments").expect("import should succeed");

        let index = forge_core::reqv1::ProjectIndex::scan(dir.path()).expect("project index");
        assert!(!index.requests.is_empty());
        assert_eq!(
            index.environments.first().map(String::as_str),
            Some("payments")
        );
        let environment: serde_json::Value =
            forge_core::store::load_json(&dir.path().join("environments/payments.json"))
                .expect("request-v1 environment");
        assert_eq!(environment["baseUrl"], "https://api.example.com");
        assert!(index
            .requests
            .iter()
            .any(|request| request.rel_path.ends_with("login.request.json")));
    }

    #[test]
    fn import_postman_environment_keeps_secret_values_out_of_environment_json() {
        let fixture = include_str!("../../../forge-core/tests/fixtures/postman_environment.json");
        let (environment, secrets) = forge_core::convert::parse_postman_environment(fixture)
            .expect("environment fixture should parse");
        let dir = tempfile::tempdir().expect("tempdir");
        let _workspace = Workspace::create(dir.path(), "WS").expect("create workspace");

        let path = import_v1_environment(dir.path(), "Staging", &environment, &secrets)
            .expect("environment should import");

        let committed: serde_json::Value =
            forge_core::store::load_json(&path).expect("request-v1 environment JSON");
        assert_eq!(committed["baseUrl"], "https://staging.example.com");
        assert!(committed.get("apiKey").is_none());
        assert!(forge_core::reqv1::load_file_secrets(dir.path()).contains_key("apiKey"));
    }

    #[test]
    fn import_collection_preserves_postman_scripts_in_quarantine() {
        let fixture = include_str!("../../../forge-core/tests/fixtures/postman_collection.json");
        let import = parse_postman(fixture).expect("fixture should parse");
        let dir = tempfile::tempdir().expect("tempdir");
        let _workspace = Workspace::create(dir.path(), "WS").expect("create workspace");

        import_collection(dir.path(), &import, "Payments").expect("import should succeed");

        let quarantine = forge_core::convert::load_import_quarantine(dir.path())
            .expect("quarantine loads")
            .expect("Postman scripts create quarantine");
        assert!(!quarantine.entries.is_empty());
    }
}
