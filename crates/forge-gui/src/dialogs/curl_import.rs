//! Import curl (File menu / `Ctrl+Shift+V`): paste a curl command line, get a
//! live preview, pick where it lands in the workspace, then create the
//! request.

use egui::{RichText, TextEdit, Ui, Window};

use forge_core::convert::parse_curl;
use forge_core::model::{AuthConfig, RequestDef};
use forge_core::store::{create_request, TreeNode, Workspace};

use crate::state::{AppState, StatusMessage};
use crate::widgets::method_badge::method_color;

/// One selectable target directory in the collection/folder picker.
struct TargetDir {
    /// Display label, indented to show nesting (e.g. `Petstore / Auth`).
    label: String,
    path: std::path::PathBuf,
}

/// Transient state of the curl-import dialog, owned by
/// [`crate::dialogs::DialogManager`].
#[derive(Default)]
pub struct CurlImportState {
    open: bool,
    command: String,
    name: String,
    target_idx: usize,
}

impl CurlImportState {
    /// Open the dialog with an empty paste area.
    pub fn open(&mut self) {
        self.open = true;
        self.command.clear();
        self.name.clear();
        self.target_idx = 0;
    }
}

fn target_dirs(workspace: &Workspace) -> Vec<TargetDir> {
    let mut out = Vec::new();
    for col in &workspace.collections {
        out.push(TargetDir {
            label: col.meta.name.clone(),
            path: col.dir.clone(),
        });
        collect_folders(&col.children, &col.meta.name, &mut out);
    }
    out
}

fn collect_folders(children: &[TreeNode], prefix: &str, out: &mut Vec<TargetDir>) {
    for child in children {
        if let TreeNode::Folder(f) = child {
            let label = format!("{prefix} / {}", child.display_name());
            out.push(TargetDir {
                label: label.clone(),
                path: f.dir.clone(),
            });
            collect_folders(&f.children, &label, out);
        }
    }
}

/// Render the dialog if open; no-op otherwise.
pub fn show(ctx: &egui::Context, state: &mut AppState) {
    if !state.dialogs.curl_import.open {
        return;
    }
    let workspace = state.workspace.clone();
    let Some(root) = workspace
        .as_ref()
        .map(|workspace| workspace.root.clone())
        .or_else(|| state.assets.project_root())
    else {
        // Nothing sensible to import into; drop the dialog rather than show
        // a picker with no options.
        state.dialogs.curl_import.open = false;
        state.status = Some(StatusMessage::error("Open a workspace before importing"));
        return;
    };

    let request_v1 = root.join("project.json").is_file();
    let targets = if request_v1 {
        let requests = root.join("requests");
        let path = state
            .assets
            .selected_directory()
            .filter(|directory| directory.starts_with(&requests))
            .unwrap_or(requests);
        vec![TargetDir {
            label: path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string(),
            path,
        }]
    } else {
        workspace.as_ref().map(target_dirs).unwrap_or_default()
    };
    let parsed = parse_curl(&state.dialogs.curl_import.command);
    if let Ok(def) = &parsed {
        if state.dialogs.curl_import.name.is_empty() {
            state.dialogs.curl_import.name = def.name.clone();
        }
    }

    let mut window_open = true;
    let mut import_clicked = false;
    let mut cancel_clicked = false;

    Window::new("Import curl")
        .id(egui::Id::new("curl-import-dialog"))
        .collapsible(false)
        .resizable(true)
        .default_size([560.0, 420.0])
        .open(&mut window_open)
        .show(ctx, |ui| {
            ui.label("Paste a curl command:");
            ui.add(
                TextEdit::multiline(&mut state.dialogs.curl_import.command)
                    .desired_rows(6)
                    .font(egui::FontSelection::from(egui::FontId::monospace(14.0)))
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(8.0);
            ui.separator();
            ui.label("Preview:");
            match &parsed {
                Ok(def) => preview(ui, def),
                Err(e) => {
                    ui.colored_label(ui.visuals().error_fg_color, e.to_string());
                }
            }
            ui.add_space(8.0);
            ui.separator();

            if targets.is_empty() {
                ui.weak(if request_v1 {
                    "This project has no request directory to import into."
                } else {
                    "No collections yet — create one first from the Collections panel."
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label("Target:");
                    let idx = state.dialogs.curl_import.target_idx.min(targets.len() - 1);
                    egui::ComboBox::from_id_salt("curl-import-target")
                        .selected_text(
                            targets
                                .get(idx)
                                .map(|t| t.label.as_str())
                                .unwrap_or_default(),
                        )
                        .show_ui(ui, |ui| {
                            for (i, t) in targets.iter().enumerate() {
                                ui.selectable_value(
                                    &mut state.dialogs.curl_import.target_idx,
                                    i,
                                    &t.label,
                                );
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.text_edit_singleline(&mut state.dialogs.curl_import.name);
                });
            }

            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cancel_clicked = true;
                }
                let can_import = parsed.is_ok()
                    && (request_v1 || !targets.is_empty())
                    && !state.dialogs.curl_import.name.trim().is_empty();
                if ui
                    .add_enabled(can_import, egui::Button::new("Import"))
                    .clicked()
                {
                    import_clicked = true;
                }
            });
        });

    if import_clicked {
        if let Ok(mut def) = parsed {
            def.name = state.dialogs.curl_import.name.trim().to_string();
            let idx = state
                .dialogs
                .curl_import
                .target_idx
                .min(targets.len().saturating_sub(1));
            let target = targets.get(idx).map(|target| target.path.clone());
            if let Some(target) = target {
                let result = if request_v1 {
                    create_request_v1(&root, &target, &def)
                } else {
                    create_request(&target, &def).map_err(|error| error.to_string())
                };
                match result {
                    Ok(file) => {
                        if request_v1 {
                            state.assets.load(root.clone());
                            match state.dialogs.v1_editor.open_file(file, None) {
                                Ok(()) => {
                                    state.dialogs.curl_import.open = false;
                                    state.status =
                                        Some(StatusMessage::info("Imported curl request"));
                                }
                                Err(error) => state.status = Some(StatusMessage::error(error)),
                            }
                        } else if let Some(workspace) = &workspace {
                            let rel_id = workspace.rel_id(&file);
                            reload_and_open(state, rel_id, def);
                            state.dialogs.curl_import.open = false;
                        }
                    }
                    Err(error) => state.status = Some(StatusMessage::error(error)),
                }
            }
        }
    }
    if cancel_clicked || !window_open {
        state.dialogs.curl_import.open = false;
    }
}

fn create_request_v1(
    root: &std::path::Path,
    directory: &std::path::Path,
    definition: &RequestDef,
) -> Result<std::path::PathBuf, String> {
    let requests_root = root.join("requests");
    if !directory.starts_with(&requests_root) {
        return Err("request target must be inside the project's requests directory".to_string());
    }
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let canonical_project = root.canonicalize().map_err(|error| error.to_string())?;
    let canonical_root = requests_root
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let directory = directory
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !canonical_root.starts_with(&canonical_project) || !directory.starts_with(&canonical_root) {
        return Err("request target resolves outside the project".to_string());
    }

    let mut id = slug(&definition.name);
    if id.is_empty() {
        id = "request".to_string();
    }
    let mut file = directory.join(format!("{id}.request.json"));
    let mut suffix = 2;
    while file.exists() {
        file = directory.join(format!("{id}-{suffix}.request.json"));
        suffix += 1;
    }
    let mut definition = definition.clone();
    if definition.auth.is_inherit() {
        definition.auth = AuthConfig::None;
    }
    let document =
        forge_core::reqv1::migrate_request(&definition, id).map_err(|error| error.to_string())?;
    forge_core::reqv1::save_request_document(
        &file,
        document,
        forge_core::reqv1::AssertionDocument::default(),
        forge_core::reqv1::HookDocument::default(),
        true,
    )
    .map_err(|error| error.to_string())?;
    Ok(file)
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

fn preview(ui: &mut Ui, def: &RequestDef) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(def.method.as_str())
                .color(method_color(def.method))
                .monospace()
                .strong(),
        );
        ui.monospace(&def.url);
    });
    ui.label(format!(
        "{} header(s)",
        def.headers.iter().filter(|h| h.is_active()).count()
    ));
    ui.label(format!("Body: {}", body_kind_label(&def.body)));
}

fn body_kind_label(body: &forge_core::model::BodyDef) -> &'static str {
    use forge_core::model::BodyDef;
    match body {
        BodyDef::None => "none",
        BodyDef::Json { .. } => "JSON",
        BodyDef::Xml { .. } => "XML",
        BodyDef::Raw { .. } => "raw",
        BodyDef::FormUrlencoded { .. } => "form url-encoded",
        BodyDef::Multipart { .. } => "multipart",
        BodyDef::GraphQl { .. } => "GraphQL",
        BodyDef::Binary { .. } => "binary",
    }
}

/// Reload the workspace from disk (picking up the newly written request
/// file) and open a tab for it.
fn reload_and_open(state: &mut AppState, rel_id: String, def: RequestDef) {
    if let Some(root) = state.workspace.as_ref().map(|w| w.root.clone()) {
        match Workspace::load(&root) {
            Ok(ws) => state.workspace = Some(ws),
            Err(e) => state.status = Some(StatusMessage::error(e.to_string())),
        }
    }
    state.open_tab(rel_id, def);
    state.status = Some(StatusMessage::info("Imported curl command"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_import_creates_request_v1_documents_with_collision_free_names() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("project.json"), r#"{"formatVersion":1}"#).unwrap();
        let requests = root.path().join("requests");
        std::fs::create_dir_all(&requests).unwrap();
        let mut definition = parse_curl(
            "curl -X POST 'https://example.test/items?active=true' -H 'content-type: application/json' -d '{\"id\":1}'",
        )
        .unwrap();
        definition.name = "Curl request".to_string();

        let first = create_request_v1(root.path(), &requests, &definition).unwrap();
        let second = create_request_v1(root.path(), &requests, &definition).unwrap();

        assert_eq!(first.file_name().unwrap(), "curl-request.request.json");
        assert_eq!(second.file_name().unwrap(), "curl-request-2.request.json");
        let document = forge_core::reqv1::load_request_document(&first).unwrap();
        assert_eq!(document.request.method, definition.method);
        assert_eq!(
            document.request.url,
            "https://example.test/items?active=true"
        );
        use forge_core::reqv1::model::{BodySpec, BodyType};
        assert!(matches!(
            document.request.body,
            Some(BodySpec::Inline(body)) if body.body_type == BodyType::Json
        ));
    }
}
