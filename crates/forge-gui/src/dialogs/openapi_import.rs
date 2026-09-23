//! Import OpenAPI (File menu): pick a JSON/YAML spec, choose which
//! operations to bring in, and generate a whole collection — requests,
//! optional contract-test assertions and the spec-to-collection binding.

use std::path::{Path, PathBuf};

use egui::{RichText, Window};

use forge_core::openapi::{
    build_binding, contract_checks, operation_to_request, parse_spec, ParsedSpec,
};
use forge_core::store::{
    create_collection, create_request, save_collection_meta, Workspace, SPECS_DIR,
};

use crate::state::{AppState, StatusMessage};
use crate::widgets::method_badge::method_color;

/// Transient state of the OpenAPI-import dialog, owned by
/// [`crate::dialogs::DialogManager`].
#[derive(Default)]
pub struct OpenApiImportState {
    open: bool,
    spec_path: Option<PathBuf>,
    spec: Option<Result<ParsedSpec, String>>,
    collection_name: String,
    generate_contract: bool,
    copy_spec: bool,
    /// Parallel to `spec.operations`: whether each op is selected for import.
    selected: Vec<bool>,
}

impl OpenApiImportState {
    /// Open a file picker for a JSON/YAML OpenAPI document and parse it
    /// immediately so the dialog can show a summary/operations table.
    pub fn open(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("OpenAPI spec", &["json", "yaml", "yml"])
            .pick_file()
        else {
            return;
        };
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string());
        let parsed = text.and_then(|t| parse_spec(&t).map_err(|e| e.to_string()));
        self.collection_name = match &parsed {
            Ok(spec) if !spec.title.is_empty() => spec.title.clone(),
            _ => path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        self.selected = match &parsed {
            Ok(spec) => vec![true; spec.operations.len()],
            Err(_) => Vec::new(),
        };
        self.spec_path = Some(path);
        self.spec = Some(parsed);
        self.generate_contract = true;
        self.copy_spec = true;
        self.open = true;
    }
}

/// Render the dialog if open; no-op otherwise.
pub fn show(ctx: &egui::Context, state: &mut AppState) {
    if !state.dialogs.openapi_import.open {
        return;
    }
    let workspace = state.workspace.clone();
    let Some(root) = workspace
        .as_ref()
        .map(|workspace| workspace.root.clone())
        .or_else(|| state.assets.project_root())
    else {
        state.dialogs.openapi_import.open = false;
        state.status = Some(StatusMessage::error("Open a workspace before importing"));
        return;
    };

    let mut window_open = true;
    let mut import_clicked = false;
    let mut cancel_clicked = false;

    Window::new("Import OpenAPI")
        .id(egui::Id::new("openapi-import-dialog"))
        .collapsible(false)
        .resizable(true)
        .default_size([680.0, 520.0])
        .open(&mut window_open)
        .show(ctx, |ui| {
            let dialog = &mut state.dialogs.openapi_import;
            match &dialog.spec {
                None => {
                    ui.weak("No spec loaded.");
                }
                Some(Err(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                Some(Ok(spec)) => {
                    ui.label(format!(
                        "{} {} — {} server(s), {} operation(s)",
                        spec.title,
                        spec.version,
                        spec.servers.len(),
                        spec.operations.len()
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Collection name:");
                        ui.text_edit_singleline(&mut dialog.collection_name);
                    });
                    ui.checkbox(
                        &mut dialog.generate_contract,
                        "Generate contract assertions",
                    );
                    ui.checkbox(&mut dialog.copy_spec, "Copy spec into workspace specs/ dir");
                    if root.join("project.json").is_file() && !dialog.copy_spec {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "Request-v1 projects require a project-relative spec. Enable Copy spec to keep the contract portable.",
                        );
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Select all").clicked() {
                            dialog.selected.iter_mut().for_each(|s| *s = true);
                        }
                        if ui.button("Select none").clicked() {
                            dialog.selected.iter_mut().for_each(|s| *s = false);
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("openapi_import-sa-1")
                        .max_height(280.0)
                        .show(ui, |ui| {
                            egui::Grid::new("openapi-ops-grid")
                                .num_columns(4)
                                .striped(true)
                                .spacing([10.0, 4.0])
                                .show(ui, |ui| {
                                    ui.strong("");
                                    ui.strong("Method");
                                    ui.strong("Path");
                                    ui.strong("Operation");
                                    ui.end_row();
                                    for (i, op) in spec.operations.iter().enumerate() {
                                        if let Some(sel) = dialog.selected.get_mut(i) {
                                            ui.checkbox(sel, "");
                                        }
                                        ui.label(
                                            RichText::new(op.method.as_str())
                                                .color(method_color(op.method))
                                                .monospace()
                                                .strong(),
                                        );
                                        ui.monospace(&op.path);
                                        let label = if op.summary.is_empty() {
                                            op.id.clone()
                                        } else {
                                            format!("{} ({})", op.summary, op.id)
                                        };
                                        ui.label(label);
                                        ui.end_row();
                                    }
                                });
                        });
                }
            }

            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cancel_clicked = true;
                }
                let can_import = matches!(&state.dialogs.openapi_import.spec, Some(Ok(_)))
                    && state.dialogs.openapi_import.selected.iter().any(|s| *s)
                    && (!root.join("project.json").is_file()
                        || state.dialogs.openapi_import.copy_spec
                        || state
                            .dialogs
                            .openapi_import
                            .spec_path
                            .as_ref()
                            .is_some_and(|path| path.starts_with(&root)))
                    && !state
                        .dialogs
                        .openapi_import
                        .collection_name
                        .trim()
                        .is_empty();
                if ui
                    .add_enabled(can_import, egui::Button::new("Import"))
                    .clicked()
                {
                    import_clicked = true;
                }
            });
        });

    if import_clicked {
        if let Err(e) = do_import(&root, &mut state.dialogs.openapi_import) {
            state.status = Some(StatusMessage::error(e));
        } else {
            state.dialogs.openapi_import.open = false;
            reload_workspace(state);
            state.status = Some(StatusMessage::info("OpenAPI spec imported"));
        }
    }
    if cancel_clicked || !window_open {
        state.dialogs.openapi_import.open = false;
    }
}

fn do_import(root: &Path, dialog: &mut OpenApiImportState) -> Result<(), String> {
    let Some(Ok(spec)) = &dialog.spec else {
        return Err("no spec loaded".to_string());
    };
    let Some(spec_source) = &dialog.spec_path else {
        return Err("no spec file".to_string());
    };

    let mut copied_spec = None;
    let spec_rel_path = if dialog.copy_spec {
        let specs_dir = root.join(SPECS_DIR);
        std::fs::create_dir_all(&specs_dir).map_err(|e| e.to_string())?;
        let canonical_root = root.canonicalize().map_err(|error| error.to_string())?;
        let canonical_specs = specs_dir
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !canonical_specs.starts_with(&canonical_root) {
            return Err("specs directory resolves outside the project".to_string());
        }
        let file_name = spec_source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "spec.yaml".to_string());
        let (dest, stored_name) = available_spec_path(&specs_dir, &file_name);
        std::fs::copy(spec_source, &dest).map_err(|e| e.to_string())?;
        copied_spec = Some(dest);
        format!("{SPECS_DIR}/{stored_name}")
    } else {
        let canonical_root = root.canonicalize().map_err(|error| error.to_string())?;
        let canonical_spec = spec_source
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let relative = canonical_spec.strip_prefix(&canonical_root).map_err(|_| {
            "the selected spec is outside this project; enable Copy spec to import it safely"
                .to_string()
        })?;
        relative.to_string_lossy().replace('\\', "/")
    };

    if root.join("project.json").is_file() {
        let result = import_request_v1(root, dialog, spec, &spec_rel_path);
        if result.is_err() {
            if let Some(path) = copied_spec {
                let _ = std::fs::remove_file(path);
            }
        }
        return result;
    }

    let col_dir =
        create_collection(root, dialog.collection_name.trim()).map_err(|e| e.to_string())?;

    let mut pairs: Vec<(String, String)> = Vec::new();
    for (i, op) in spec.operations.iter().enumerate() {
        if !dialog.selected.get(i).copied().unwrap_or(false) {
            continue;
        }
        let mut req = operation_to_request(op);
        if dialog.generate_contract {
            req.assertions = contract_checks(op, None)
                .into_iter()
                .map(|check| {
                    let mut def: forge_core::model::AssertionDef = check.into();
                    def.note = "contract".to_string();
                    def
                })
                .collect();
        }
        let file = create_request(&col_dir, &req).map_err(|e| e.to_string())?;
        let rel_to_collection = file
            .strip_prefix(&col_dir)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        pairs.push((rel_to_collection, op.id.clone()));
    }

    let mut meta = forge_core::model::CollectionMeta::new(dialog.collection_name.trim());
    meta.openapi = Some(build_binding(&spec_rel_path, &pairs));
    save_collection_meta(&col_dir, &meta).map_err(|e| e.to_string())?;

    Ok(())
}

fn import_request_v1(
    root: &Path,
    dialog: &OpenApiImportState,
    spec: &ParsedSpec,
    spec_rel_path: &str,
) -> Result<(), String> {
    let requests = root.join("requests");
    std::fs::create_dir_all(&requests).map_err(|error| error.to_string())?;
    let canonical_root = root.canonicalize().map_err(|error| error.to_string())?;
    let canonical_requests = requests.canonicalize().map_err(|error| error.to_string())?;
    if !canonical_requests.starts_with(&canonical_root) {
        return Err("requests directory resolves outside the project".to_string());
    }
    let base = slug(&dialog.collection_name);
    let base = if base.is_empty() { "openapi" } else { &base };
    let mut target = canonical_requests.join(base);
    let mut suffix = 2;
    while target.exists() {
        target = requests.join(format!("{base}-{suffix}"));
        suffix += 1;
    }
    std::fs::create_dir_all(&target).map_err(|error| error.to_string())?;

    let result = (|| {
        forge_core::reqv1::set_openapi(&target, spec_rel_path)?;
        for (index, operation) in spec.operations.iter().enumerate() {
            if !dialog.selected.get(index).copied().unwrap_or(false) {
                continue;
            }
            let mut definition = operation_to_request(operation);
            if definition.auth.is_inherit() {
                definition.auth = forge_core::model::AuthConfig::None;
            }
            if dialog.generate_contract {
                definition.assertions = contract_checks(operation, None)
                    .into_iter()
                    .map(|check| {
                        let mut assertion: forge_core::model::AssertionDef = check.into();
                        assertion.note = "contract".to_string();
                        assertion
                    })
                    .collect();
            }
            let id = slug(&operation.id);
            let id = if id.is_empty() {
                format!("operation-{}", index + 1)
            } else {
                id
            };
            definition.name = if operation.summary.trim().is_empty() {
                operation.id.clone()
            } else {
                operation.summary.clone()
            };
            let document = forge_core::reqv1::migrate_request(&definition, &id)
                .map_err(|error| format!("cannot import operation {}: {error}", operation.id))?;
            let file = forge_core::reqv1::available_path(&target, &id, ".request.json");
            forge_core::reqv1::save_request_document(
                &file,
                document,
                forge_core::reqv1::AssertionDocument::default(),
                forge_core::reqv1::HookDocument::default(),
                true,
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&target);
        return Err(error);
    }
    Ok(())
}

fn available_spec_path(directory: &Path, file_name: &str) -> (PathBuf, String) {
    let original = Path::new(file_name);
    let stem = original
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "spec".to_string());
    let extension = original
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let mut candidate = file_name.to_string();
    let mut suffix = 2;
    while directory.join(&candidate).exists() {
        candidate = format!("{stem}-{suffix}{extension}");
        suffix += 1;
    }
    (directory.join(&candidate), candidate)
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut separator = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            separator = false;
        } else if !separator && !out.is_empty() {
            out.push('-');
            separator = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn reload_workspace(state: &mut AppState) {
    let Some(root) = state
        .workspace
        .as_ref()
        .map(|workspace| workspace.root.clone())
        .or_else(|| state.assets.project_root())
    else {
        return;
    };
    state.assets.load(root.clone());
    if state.workspace.is_some() {
        match Workspace::load(&root) {
            Ok(ws) => state.workspace = Some(ws),
            Err(e) => state.status = Some(StatusMessage::error(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_v1_import_keeps_a_project_relative_contract_selection() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("project.json"), r#"{"formatVersion":1}"#).unwrap();
        let spec = parse_spec(
            r#"{
              "openapi":"3.0.0",
              "info":{"title":"Pets","version":"1.0.0"},
              "paths":{"/pets":{"get":{"operationId":"listPets","summary":"List pets","responses":{"200":{"description":"ok"}}}}}
            }"#,
        )
        .unwrap();
        let dialog = OpenApiImportState {
            collection_name: "Pets API".to_string(),
            generate_contract: false,
            selected: vec![true; spec.operations.len()],
            ..OpenApiImportState::default()
        };

        import_request_v1(root.path(), &dialog, &spec, "specs/pets.json").unwrap();

        let request = root.path().join("requests/pets-api/listpets.request.json");
        let document = forge_core::reqv1::load_request_document(&request).unwrap();
        assert_eq!(document.meta.name, "List pets");
        assert_eq!(document.request.method, forge_core::model::Method::Get);
        let selection = forge_core::reqv1::effective_openapi(root.path(), &request)
            .unwrap()
            .unwrap();
        assert_eq!(selection.value, "specs/pets.json");
        assert_eq!(
            std::fs::read_to_string(root.path().join("requests/pets-api/.forge-openapi"))
                .unwrap()
                .trim(),
            "specs/pets.json"
        );
    }
}
