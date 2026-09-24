//! Editor layout and toolbar rendering.

use super::*;

pub(super) fn editor_column_widths(available_width: f32) -> (f32, f32) {
    let editor_area_width = available_width.max(0.0);
    let usable_width = (editor_area_width - EDITOR_COLUMN_GAP).max(0.0);
    let catalog_width = (editor_area_width * 0.34)
        .clamp(250.0, 330.0)
        .min((usable_width - 280.0).max(0.0))
        .min(usable_width);
    let request_width = usable_width - catalog_width;
    (catalog_width, request_width)
}

pub(super) fn zoom_editor_font_size(current: f32, zoom_delta: f32) -> f32 {
    (current * zoom_delta).clamp(9.0, 24.0)
}

fn request_form(ui: &mut egui::Ui, d: &mut V1EditorState) -> bool {
    let snapshot = d.snapshot();
    let mut document = match forge_core::reqv1::RequestDocument::parse(&d.text) {
        Ok(document) => document,
        Err(error) => {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Fix the JSON before using the form: {error}"),
            );
            ui.weak("Your JSON text is preserved. Switch to JSON to correct it.");
            return false;
        }
    };
    let mut changed = false;
    egui::ScrollArea::vertical()
        .id_salt("v1-request-form")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("v1-form-method")
                    .selected_text(document.request.method.as_str())
                    .show_ui(ui, |ui| {
                        for method in Method::ALL {
                            changed |= ui
                                .selectable_value(
                                    &mut document.request.method,
                                    method,
                                    method.as_str(),
                                )
                                .changed();
                        }
                    });
                changed |= ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        TextEdit::singleline(&mut document.request.url)
                            .hint_text("https://api.example.com/resource"),
                    )
                    .changed();
            });

            ui.collapsing("Query parameters", |ui| {
                changed |= header_rows(ui, "query", &mut document.request.query);
            });
            ui.collapsing("Headers", |ui| {
                changed |= header_rows(ui, "header", &mut document.request.headers);
            });
            ui.collapsing("Body", |ui| {
                changed |= body_form(ui, d, &mut document.request.body);
            });
            ui.collapsing("Advanced request settings", |ui| {
                let mut timeout_mode = match document.request.settings.timeout_ms {
                    None => "Default",
                    Some(0) => "Disabled",
                    Some(_) => "Custom",
                };
                ui.horizontal(|ui| {
                    ui.label("Timeout");
                    egui::ComboBox::from_id_salt("v1-timeout-mode")
                        .selected_text(timeout_mode)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut timeout_mode, "Default", "Project default");
                            ui.selectable_value(&mut timeout_mode, "Custom", "Custom");
                            ui.selectable_value(&mut timeout_mode, "Disabled", "Disabled");
                        });
                    let previous = match document.request.settings.timeout_ms {
                        None => "Default",
                        Some(0) => "Disabled",
                        Some(_) => "Custom",
                    };
                    if timeout_mode != previous {
                        document.request.settings.timeout_ms = match timeout_mode {
                            "Default" => None,
                            "Disabled" => Some(0),
                            _ => Some(
                                document
                                    .request
                                    .settings
                                    .timeout_ms
                                    .filter(|value| *value > 0)
                                    .unwrap_or(30_000),
                            ),
                        };
                        changed = true;
                    }
                    let mut timeout = document
                        .request
                        .settings
                        .timeout_ms
                        .filter(|value| *value > 0)
                        .unwrap_or(30_000);
                    if timeout_mode == "Custom"
                        && ui
                            .add(
                                egui::DragValue::new(&mut timeout)
                                    .range(1..=600_000)
                                    .suffix(" ms"),
                            )
                            .changed()
                    {
                        document.request.settings.timeout_ms = Some(timeout);
                        changed = true;
                    }
                });

                let mut follow_redirects =
                    document.request.settings.follow_redirects.unwrap_or(true);
                if ui
                    .checkbox(&mut follow_redirects, "Follow redirects")
                    .changed()
                {
                    document.request.settings.follow_redirects = Some(follow_redirects);
                    changed = true;
                }
                let mut encode_url = document.request.settings.encode_url.unwrap_or(true);
                if ui.checkbox(&mut encode_url, "Encode URL").changed() {
                    document.request.settings.encode_url = Some(encode_url);
                    changed = true;
                }
                let mut limit_redirects = document.request.settings.max_redirects.is_some();
                let mut max_redirects = document.request.settings.max_redirects.unwrap_or(10);
                ui.horizontal(|ui| {
                    if ui
                        .checkbox(&mut limit_redirects, "Limit redirects")
                        .changed()
                    {
                        document.request.settings.max_redirects =
                            limit_redirects.then_some(max_redirects);
                        changed = true;
                    }
                    if limit_redirects
                        && ui
                            .add(
                                egui::DragValue::new(&mut max_redirects)
                                    .range(0..=100)
                                    .suffix(" redirects"),
                            )
                            .changed()
                    {
                        document.request.settings.max_redirects = Some(max_redirects);
                        changed = true;
                    }
                    if ui.small_button("Default").clicked() {
                        document.request.settings.max_redirects = None;
                        changed = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.weak(format!(
                        "Auth: {}",
                        document
                            .auth
                            .as_ref()
                            .map(|auth| auth.as_str())
                            .unwrap_or("inherited")
                    ));
                    if ui.button("Open auth tools").clicked() {
                        d.result_tab = ResultTab::Auth;
                    }
                    if ui.button("Edit remaining fields in JSON").clicked() {
                        d.request_view = RequestView::Json;
                    }
                });
            });
        });
    if changed {
        match serialize_request(&document) {
            Ok(text) => {
                d.record_undo(snapshot);
                d.text = text;
                validate_editor_json(d);
            }
            Err(error) => d.catalog_error = Some(error),
        }
    }
    changed
}

fn header_rows(ui: &mut egui::Ui, id: &str, rows: &mut Vec<HeaderSpec>) -> bool {
    let count = rows.len();
    let mut changed = false;
    let mut remove = None;
    for (index, row) in rows.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            changed |= ui.checkbox(&mut row.enabled, "").changed();
            changed |= ui
                .add_sized(
                    [ui.available_width() * 0.34, 26.0],
                    TextEdit::singleline(&mut row.name).hint_text("Name"),
                )
                .changed();
            changed |= ui
                .add_sized(
                    [ui.available_width() - 38.0, 26.0],
                    TextEdit::singleline(&mut row.value).hint_text("Value"),
                )
                .changed();
            if ui.small_button("×").on_hover_text("Remove row").clicked() {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        rows.remove(index);
        changed = true;
    }
    if ui.small_button(format!("+ Add {id}")).clicked() {
        rows.push(HeaderSpec {
            name: String::new(),
            value: String::new(),
            enabled: true,
        });
        changed = true;
    }
    if count == 0 {
        ui.weak(format!("No {id}s configured."));
    }
    changed
}

fn body_form(ui: &mut egui::Ui, d: &mut V1EditorState, body: &mut Option<BodySpec>) -> bool {
    let current_mode = body_mode(body.as_ref());
    let mut mode = current_mode.to_string();
    let mut changed = false;
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("v1-body-mode")
            .selected_text(&mode)
            .show_ui(ui, |ui| {
                for choice in [
                    "No body",
                    "JSON",
                    "Text",
                    "Form",
                    "Data asset",
                    "Multipart",
                    "Binary file",
                ] {
                    ui.selectable_value(&mut mode, choice.to_string(), choice);
                }
            });
        if ui.small_button("Choose data…").clicked() {
            d.open_catalog(CatalogContext::Body);
        }
    });
    if mode != current_mode {
        if let Some(previous) = body.take() {
            d.body_mode_drafts
                .insert(current_mode.to_string(), previous);
        }
        *body = if mode == "No body" {
            None
        } else {
            d.body_mode_drafts
                .remove(&mode)
                .or_else(|| empty_body_for_mode(&mode))
        };
        changed = true;
        d.body_draft = None;
        d.body_draft_origin = None;
        d.body_draft_error = None;
    }

    if let Some(BodySpec::Inline(inline)) = body.as_mut() {
        match inline.body_type {
            BodyType::Json => {
                let value = inline.value.clone().unwrap_or(serde_json::Value::Null);
                let value_json =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".into());
                let origin = format!("inline:{value_json}");
                if d.body_draft_origin.as_deref() != Some(origin.as_str()) {
                    d.body_draft = Some(value_json.clone());
                    d.body_draft_origin = Some(origin);
                }
                if let Some(draft) = d.body_draft.as_mut() {
                    let response = ui.add(
                        TextEdit::multiline(draft)
                            .code_editor()
                            .desired_rows(7)
                            .desired_width(f32::INFINITY),
                    );
                    if response.changed() {
                        changed = true;
                        match serde_json::from_str::<serde_json::Value>(draft) {
                            Ok(value) => {
                                inline.value = Some(value.clone());
                                let value_json =
                                    serde_json::to_string_pretty(&value).unwrap_or_default();
                                d.body_draft_origin = Some(format!("inline:{value_json}"));
                                d.body_draft_error = None;
                            }
                            Err(error) => {
                                d.body_draft_error =
                                    Some(format!("Body value is not valid JSON: {error}"));
                            }
                        }
                    }
                }
            }
            BodyType::Text => {
                ui.label("Plain text body");
                let mut text = inline
                    .value
                    .as_ref()
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_default();
                if ui
                    .add(
                        TextEdit::multiline(&mut text)
                            .desired_rows(7)
                            .desired_width(f32::INFINITY),
                    )
                    .changed()
                {
                    inline.value = Some(serde_json::Value::String(text));
                    changed = true;
                }
            }
            BodyType::Form => {
                changed |= form_body_fields(ui, inline);
            }
            BodyType::None => {
                ui.weak("No request body.");
            }
        }
    } else if let Some(BodySpec::Ref(reference)) = body.as_mut() {
        changed |= ui
            .add_sized(
                [ui.available_width(), 28.0],
                TextEdit::singleline(&mut reference.reference)
                    .hint_text("assets/data/example.json"),
            )
            .changed();
        ui.weak("Choose project data to fill this reference automatically.");
    } else if let Some(BodySpec::Multipart(multipart)) = body.as_mut() {
        changed |= multipart_body_form(ui, multipart);
    } else if let Some(BodySpec::Binary(binary)) = body.as_mut() {
        ui.horizontal(|ui| {
            ui.label("File");
            changed |= ui
                .add_sized(
                    [ui.available_width() - 90.0, 28.0],
                    TextEdit::singleline(&mut binary.file).hint_text("path/to/file.bin"),
                )
                .changed();
            if ui.small_button("Browse…").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    binary.file = path.display().to_string();
                    changed = true;
                }
            }
        });
        changed |= optional_text_field(ui, "Content type", &mut binary.content_type);
    } else {
        ui.weak("No request body.");
    }
    if let Some(error) = &d.body_draft_error {
        ui.colored_label(ui.visuals().error_fg_color, error);
        if ui.small_button("Reset body edits").clicked() {
            if let Some(BodySpec::Inline(inline)) = body.as_ref() {
                if inline.body_type == BodyType::Json {
                    let value_json = serde_json::to_string_pretty(
                        &inline.value.clone().unwrap_or(serde_json::Value::Null),
                    )
                    .unwrap_or_else(|_| "null".into());
                    d.body_draft = Some(value_json.clone());
                    d.body_draft_origin = Some(format!("inline:{value_json}"));
                } else {
                    d.body_draft = None;
                    d.body_draft_origin = None;
                }
            } else {
                d.body_draft = None;
                d.body_draft_origin = None;
            }
            d.body_draft_error = None;
            changed = true;
        }
    }
    changed
}

fn body_mode(body: Option<&BodySpec>) -> &'static str {
    match body {
        None => "No body",
        Some(BodySpec::Inline(inline)) => match inline.body_type {
            BodyType::Json => "JSON",
            BodyType::Text => "Text",
            BodyType::Form => "Form",
            BodyType::None => "No body",
        },
        Some(BodySpec::Ref(_)) => "Data asset",
        Some(BodySpec::Multipart(_)) => "Multipart",
        Some(BodySpec::Binary(_)) => "Binary file",
    }
}

fn empty_body_for_mode(mode: &str) -> Option<BodySpec> {
    match mode {
        "JSON" => Some(BodySpec::Inline(InlineBody {
            body_type: BodyType::Json,
            value: Some(serde_json::Value::Object(serde_json::Map::new())),
        })),
        "Text" => Some(BodySpec::Inline(InlineBody {
            body_type: BodyType::Text,
            value: Some(serde_json::Value::String(String::new())),
        })),
        "Form" => Some(BodySpec::Inline(InlineBody {
            body_type: BodyType::Form,
            value: Some(serde_json::Value::Object(serde_json::Map::new())),
        })),
        "Data asset" => Some(BodySpec::Ref(forge_core::reqv1::model::RefBody {
            reference: String::new(),
            body_type: Some(BodyType::Json),
        })),
        "Multipart" => Some(BodySpec::Multipart(
            forge_core::reqv1::model::MultipartBody {
                body_type: forge_core::reqv1::model::MultipartBodyType::Multipart,
                parts: Vec::new(),
            },
        )),
        "Binary file" => Some(BodySpec::Binary(forge_core::reqv1::model::BinaryBody {
            body_type: forge_core::reqv1::model::BinaryBodyType::Binary,
            file: String::new(),
            content_type: None,
        })),
        _ => None,
    }
}

fn form_body_fields(ui: &mut egui::Ui, inline: &mut InlineBody) -> bool {
    let mut changed = false;
    ui.label("Form fields");
    let mut object = inline
        .value
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    let keys = object.keys().cloned().collect::<Vec<_>>();
    let mut remove = None;
    let mut updates = Vec::new();
    for key in keys {
        let Some(value) = object.get(&key) else {
            continue;
        };
        if let Some(value) = value.as_str() {
            let mut name = key.clone();
            let mut value = value.to_string();
            let mut remove_row = false;
            ui.horizontal(|ui| {
                ui.add_sized(
                    [ui.available_width() * 0.38, 26.0],
                    TextEdit::singleline(&mut name),
                );
                ui.add_sized(
                    [ui.available_width() - 40.0, 26.0],
                    TextEdit::singleline(&mut value),
                );
                remove_row = ui.small_button("−").clicked();
            });
            if remove_row {
                remove = Some(key);
            } else if name != key
                || object.get(&key).and_then(serde_json::Value::as_str) != Some(value.as_str())
            {
                updates.push((key, name, value));
            }
        } else {
            ui.horizontal(|ui| {
                ui.weak(&key);
                ui.label(egui::RichText::new(value.to_string()).monospace().weak());
            });
        }
    }
    if let Some(key) = remove {
        object.remove(&key);
        changed = true;
    }
    for (old, name, value) in updates {
        object.remove(&old);
        object.insert(name, serde_json::Value::String(value));
        changed = true;
    }
    if ui.button("+ Add field").clicked() {
        let mut name = "field".to_string();
        let mut suffix = 2;
        while object.contains_key(&name) {
            name = format!("field{suffix}");
            suffix += 1;
        }
        object.insert(name, serde_json::Value::String(String::new()));
        changed = true;
    }
    if changed {
        inline.value = Some(serde_json::Value::Object(object));
    }
    changed
}

fn multipart_body_form(
    ui: &mut egui::Ui,
    multipart: &mut forge_core::reqv1::model::MultipartBody,
) -> bool {
    use forge_core::reqv1::model::MultipartPart;

    let mut changed = false;
    let mut remove = None;
    for (index, part) in multipart.parts.iter_mut().enumerate() {
        let mut browse = false;
        ui.group(|ui| match part {
            MultipartPart::Text {
                name,
                value,
                filename,
                content_type,
                enabled,
            } => {
                ui.horizontal(|ui| {
                    changed |= ui.checkbox(enabled, "Enabled").changed();
                    ui.strong("Text field");
                    if ui.small_button("Remove").clicked() {
                        remove = Some(index);
                    }
                });
                changed |= ui
                    .add(TextEdit::singleline(name).hint_text("Field name"))
                    .changed();
                changed |= ui
                    .add(
                        TextEdit::multiline(value)
                            .desired_rows(2)
                            .hint_text("Value"),
                    )
                    .changed();
                changed |= optional_text_field(ui, "Filename", filename);
                changed |= optional_text_field(ui, "Content type", content_type);
            }
            MultipartPart::File {
                name,
                file,
                filename,
                content_type,
                enabled,
            } => {
                ui.horizontal(|ui| {
                    changed |= ui.checkbox(enabled, "Enabled").changed();
                    ui.strong("File field");
                    if ui.small_button("Remove").clicked() {
                        remove = Some(index);
                    }
                });
                changed |= ui
                    .add(TextEdit::singleline(name).hint_text("Field name"))
                    .changed();
                ui.horizontal(|ui| {
                    changed |= ui
                        .add(TextEdit::singleline(file).hint_text("path/to/file"))
                        .changed();
                    browse = ui.small_button("Browse…").clicked();
                });
                changed |= optional_text_field(ui, "Filename", filename);
                changed |= optional_text_field(ui, "Content type", content_type);
            }
        });
        if browse {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                if let MultipartPart::File { file, .. } = part {
                    *file = path.display().to_string();
                    changed = true;
                }
            }
        }
    }
    if let Some(index) = remove {
        multipart.parts.remove(index);
        changed = true;
    }
    ui.horizontal(|ui| {
        if ui.button("+ Text field").clicked() {
            multipart.parts.push(MultipartPart::Text {
                name: String::new(),
                value: String::new(),
                filename: None,
                content_type: None,
                enabled: true,
            });
            changed = true;
        }
        if ui.button("+ File field").clicked() {
            multipart.parts.push(MultipartPart::File {
                name: String::new(),
                file: String::new(),
                filename: None,
                content_type: None,
                enabled: true,
            });
            changed = true;
        }
    });
    changed
}

fn optional_text_field(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) -> bool {
    let mut text = value.clone().unwrap_or_default();
    let changed = ui
        .horizontal(|ui| {
            ui.label(label);
            ui.add(TextEdit::singleline(&mut text).hint_text("Optional"))
                .changed()
        })
        .inner;
    if changed {
        *value = (!text.trim().is_empty()).then_some(text);
    }
    changed
}

/// Render the central v1 editor if open.
pub fn show(ui: &mut egui::Ui, state: &mut AppState, bridge: &Bridge) {
    if !state.dialogs.v1_editor.open {
        return;
    }
    let mut pending_insert: Option<PendingInsert> = None;
    let mut refresh_project = false;
    let mut zoom_target_hovered = false;
    let accent = state.theme.accent_color();
    let mut editor_font_size = state.editor_font_size;
    let auto_save = state.auto_save;
    let show_right_tools = !state.zen_mode || state.zen_right_revealed;
    let show_catalog = false;
    let configured_openapi = state.openapi.clone();
    let configured_openapi_source = state.openapi_source.clone();
    let configured_openapi_error = state.openapi_error.clone();
    let selected_project_dir = state.assets.selected_directory();
    let shared_env = &mut state.active_env;
    let d = &mut state.dialogs.v1_editor;
    d.env_name = shared_env.clone();
    d.auto_save = auto_save;
    if let Some(source) = configured_openapi_source {
        let source = PathBuf::from(source);
        if d.openapi_source.as_ref() != Some(&source)
            || (d.openapi.is_none() && configured_openapi.is_some())
            || d.openapi_error != configured_openapi_error
        {
            d.openapi_source = Some(source);
            d.openapi = configured_openapi;
            d.openapi_error = configured_openapi_error;
            d.auto_covered_operations =
                scan_covered_operations(d.root.as_deref(), d.index.as_ref(), d.openapi.as_ref());
        }
    }
    refresh_editor_validation(d, ui.ctx());
    let surface_stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    egui::Frame::NONE
        .fill(ui.visuals().panel_fill)
        .stroke(surface_stroke)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("REQUEST").small().strong().color(accent));
                ui.label(
                    RichText::new(
                        d.root
                            .as_deref()
                            .zip(d.file.as_deref())
                            .and_then(|(root, file)| file.strip_prefix(root).ok())
                            .unwrap_or_else(|| d.file.as_deref().unwrap_or_else(|| Path::new("")))
                            .display()
                            .to_string(),
                    )
                    .strong(),
                );
                if d.dirty {
                    ui.label(RichText::new(icons::DIRTY).color(accent));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(icons::CLOSE)
                        .on_hover_text("Close request")
                        .clicked()
                    {
                        d.request_close();
                    }
                });
            });
        });
    ui.add_space(8.0);

    if let Some(saved) = &d.save_conflict {
        ui.colored_label(
            ui.visuals().error_fg_color,
            "Save conflict: your edits are retained. Compare the saved version before reloading.",
        );
        ui.collapsing(
            "Compare saved version (including hooks and assertions)",
            |ui| {
                let mut saved_text = saved.as_str();
                ui.add(
                    TextEdit::multiline(&mut saved_text)
                        .code_editor()
                        .desired_rows(12)
                        .desired_width(f32::INFINITY),
                );
            },
        );
        if ui
            .button("Discard edits and reload saved version")
            .clicked()
        {
            if let Err(error) = d.reload_current_request(true) {
                d.diagnostics = vec![error];
            }
        }
    }

    if show_right_tools {
        let mut right_panel_open = d.right_panel_open;
        let collapsed_panel = egui::Panel::right("v1-right-tools-collapsed")
            .exact_size(38.0)
            .resizable(true);
        let expanded_panel = egui::Panel::right("v1-right-tools-expanded")
            .default_size(300.0)
            .resizable(true)
            .size_range(240.0..=460.0);
        let (requested_open, generated_suite) = egui::Panel::show_switched(
            ui,
            &mut right_panel_open,
            collapsed_panel,
            expanded_panel,
            |ui, expanded| right_sidebar(ui, d, bridge, selected_project_dir.as_deref(), expanded),
        )
        .inner;
        if let Some(open) = requested_open {
            right_panel_open = open;
        }
        d.right_panel_open = right_panel_open;
        refresh_project |= generated_suite;
    }

    egui::CentralPanel::no_frame().show(ui, |ui| {
        let body_size = ui.available_size();
        let (catalog_width, request_width) = if show_catalog {
            editor_column_widths(body_size.x)
        } else {
            (0.0, body_size.x)
        };
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
            if show_catalog {
                ui.allocate_ui_with_layout(
                    egui::vec2(catalog_width, body_size.y),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        egui::Frame::NONE
                            .fill(ui.visuals().panel_fill)
                            .stroke(surface_stroke)
                            .corner_radius(8)
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                ui.set_min_height((body_size.y - 24.0).max(0.0));
                                ui.label(RichText::new("Catalog").size(18.0).strong())
                                    .on_hover_text(
                                    "Configure reusable behavior once and insert typed references.",
                                );
                                ui.add_space(8.0);
                                palette(ui, d, bridge, &mut pending_insert);
                            });
                    },
                );
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);
            }
            ui.allocate_ui_with_layout(
                egui::vec2(request_width, body_size.y),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 48.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::Frame::NONE
                                .fill(ui.visuals().panel_fill)
                                .stroke(surface_stroke)
                                .corner_radius(8)
                                .inner_margin(egui::Margin::symmetric(10, 7))
                                .show(ui, |ui| {
                                    let compact_toolbar = ui.available_width() < 620.0;
                                    let can_run = !d.in_flight
                                        && d.root.is_some()
                                        && d.run_block_error().is_none();
                                    egui_extras::StripBuilder::new(ui)
                                        .size(egui_extras::Size::remainder())
                                        .size(egui_extras::Size::exact(TOOLBAR_MENU_CELL_WIDTH))
                                        .size(egui_extras::Size::exact(TOOLBAR_TRAILING_GUTTER))
                                        .clip(true)
                                        .horizontal(|mut strip| {
                                            strip.cell(|ui| {
                                                ui.horizontal(|ui| {
                                                    let run_label = if d.in_flight {
                                                        if compact_toolbar {
                                                            icons::STOP.to_string()
                                                        } else {
                                                            "Stop".to_string()
                                                        }
                                                    } else if document_has_matrix(&d.text)
                                                    {
                                                        if compact_toolbar {
                                                            icons::PLAY.to_string()
                                                        } else {
                                                            format!("{}  Run matrix", icons::PLAY)
                                                        }
                                                    } else {
                                                        if compact_toolbar {
                                                            icons::PLAY.to_string()
                                                        } else {
                                                            format!("{}  Run", icons::PLAY)
                                                        }
                                                    };
                                                    let run_button = if compact_toolbar {
                                                        egui::Button::new(run_label)
                                                            .min_size(egui::vec2(36.0, 28.0))
                                                    } else {
                                                        crate::theme::primary_button(run_label, accent)
                                                    };
                                                    if ui
                                                        .add_enabled(can_run || d.in_flight, run_button)
                                                        .on_hover_text(if d.in_flight {
                                                            "Cancel the active request, matrix, sequence or batch"
                                                        } else if document_has_matrix(&d.text) {
                                                            "Execute every case in the request matrix"
                                                        } else {
                                                        "Execute the active request"
                                                        })
                                                        .clicked()
                                                    {
                                                        if d.in_flight {
                                                            cancel_now(d, bridge);
                                                        } else {
                                                            run_now(d, bridge);
                                                        }
                                                    }
                                                    let previous_mock = d.mock;
                                                    let mode = if d.mock { "Mock" } else { "HTTP" };
                                                    egui::ComboBox::from_id_salt("v1-run-mode")
                                                        .selected_text(mode)
                                                        .width(70.0)
                                                        .show_ui(ui, |ui| {
                                                            ui.selectable_value(&mut d.mock, false, "HTTP");
                                                            ui.selectable_value(&mut d.mock, true, "Mock");
                                                        })
                                                        .response
                                                        .on_hover_text(if d.mock {
                                                            "Run against the request's local mock"
                                                        } else {
                                                            "Send an HTTP request to the configured host"
                                                        });
                                                    if d.mock != previous_mock {
                                                        d.clear_preview();
                                                    }
                                                    if !compact_toolbar && ui
                                                        .add_enabled(
                                                            !d.undo_stack.is_empty(),
                                                            egui::Button::new("Undo"),
                                                        )
                                                        .on_hover_text("Undo the last request or test edit")
                                                        .clicked()
                                                    {
                                                        d.undo_editor_change();
                                                    }
                                                    if !compact_toolbar && ui
                                                        .button("Catalog")
                                                        .on_hover_text("Find reusable checks, data and preparation steps")
                                                        .clicked()
                                                    {
                                                        d.open_catalog(CatalogContext::General);
                                                    }
                                                    if ui
                                                        .add_enabled(
                                                            d.body_draft_error.is_none()
                                                                && d.invalid_pipeline_draft().is_none(),
                                                            egui::Button::new(format!("{}  Save", icons::SAVE)),
                                                        )
                                                        .on_hover_text("Save the request and its assertion and hook sidecars")
                                                        .clicked()
                                                    {
                                                        refresh_project = save_now(d);
                                                    }
                                                    if !compact_toolbar {
                                                        if ui.button("Format")
                                                            .on_hover_text("Beautify the request JSON")
                                                            .clicked() {
                                                            format_request(d);
                                                        }
                                                        if ui.button("Validate")
                                                            .on_hover_text("Validate JSON, references and OpenAPI compatibility")
                                                            .clicked() {
                                                            validate_now(d);
                                                        }
                                                    }

                                                    let envs: Vec<String> = d
                                                        .index
                                                        .as_ref()
                                                        .map(|index| index.environments.clone())
                                                        .unwrap_or_default();
                                                    let inherited =
                                                d.root.as_deref().zip(d.file.as_deref()).and_then(
                                                    |(root, file)| {
                                                        forge_core::reqv1::effective_environment(
                                                            root, file,
                                                        )
                                                        .ok()
                                                        .flatten()
                                                    },
                                                );
                                                    let selected =
                                                        d.env_name.clone().unwrap_or_else(|| {
                                                            inherited
                                                                .as_ref()
                                                                .map(|selection| {
                                                                    format!(
                                                                        "{} · inherited",
                                                                        selection.value
                                                                    )
                                                                })
                                                                .unwrap_or_else(|| {
                                                                    "Automatic · none".to_string()
                                                                })
                                                        });
                                                    let previous_env = d.env_name.clone();
                                                    egui::ComboBox::from_id_salt("v1-env")
                                                        .selected_text(selected)
                                                        .width(if compact_toolbar { 112.0 } else { 160.0 })
                                                        .show_ui(ui, |ui| {
                                                            ui.selectable_value(
                                                                &mut d.env_name,
                                                                None,
                                                                "Automatic (properties)",
                                                            );
                                                            for env in &envs {
                                                                ui.selectable_value(
                                                                    &mut d.env_name,
                                                                    Some(env.clone()),
                                                                    env,
                                                                );
                                                            }
                                                        });
                                                    ui.response().on_hover_text("Override the environment inherited from project properties");
                                                    if d.env_name != previous_env {
                                                        *shared_env = d.env_name.clone();
                                                        d.clear_preview();
                                                    }
                                                    if d.in_flight {
                                                        ui.spinner();
                                                    }
                                                });
                                            });
                                            strip.cell(|ui| {
                                                ui.centered_and_justified(|ui| {
                                                    ui.menu_button(icons::ELLIPSIS, |ui| {
                                                        if compact_toolbar {
                                                            if ui
                                                                .add_enabled(
                                                                    !d.undo_stack.is_empty(),
                                                                    egui::Button::new("Undo"),
                                                                )
                                                                .clicked()
                                                            {
                                                                d.undo_editor_change();
                                                                ui.close();
                                                            }
                                                            if ui.button("Catalog").clicked() {
                                                                d.open_catalog(CatalogContext::General);
                                                                ui.close();
                                                            }
                                                            ui.separator();
                                                        }
                                                        if compact_toolbar {
                                                            if ui.button("Format")
                                                                .on_hover_text("Beautify the request JSON")
                                                                .clicked() {
                                                                format_request(d);
                                                                ui.close();
                                                            }
                                                            if ui.button("Validate")
                                                                .on_hover_text("Validate JSON, references and OpenAPI compatibility")
                                                                .clicked() {
                                                                validate_now(d);
                                                                ui.close();
                                                            }
                                                            ui.separator();
                                                        }
                                                        ui.checkbox(
                                                    &mut d.allow_project_code,
                                                    "Allow project code",
                                                )
                                                .on_hover_text(
                                                    "Executes reviewed project-owned JavaScript.",
                                                );
                                                        ui.separator();
                                                        if ui
                                                            .add_enabled(
                                                                can_run,
                                                                egui::Button::new("Run sequence…"),
                                                            )
                                                            .on_hover_text("Choose and execute an ordered request sequence")
                                                            .clicked()
                                                        {
                                                            run_sequence_now(d, bridge);
                                                            ui.close();
                                                        }
                                                    })
                                                    .response
                                                    .on_hover_text("Run mode and additional editor actions");
                                                });
                                            });
                                            strip.empty();
                                        });
                                });
                        },
                    );
                    ui.add_space(8.0);

                    let total_h = ui.available_height();
                    let top_h =
                        (total_h * d.split_ratio).clamp(180.0, (total_h - 120.0).max(180.0));
                    ui.allocate_ui(egui::vec2(ui.available_width(), top_h), |ui| {
                        if let Some(notice) = &d.catalog_notice {
                            ui.weak(notice);
                        }
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(d.editor_section == EditorSection::Request, "Request")
                                .clicked()
                            {
                                d.editor_section = EditorSection::Request;
                            }
                            if ui
                                .selectable_label(d.editor_section == EditorSection::Tests, "Tests")
                                .clicked()
                            {
                                d.editor_section = EditorSection::Tests;
                            }
                            if d.editor_section == EditorSection::Request {
                                ui.separator();
                                if ui
                                    .selectable_label(d.request_view == RequestView::Form, "Form")
                                    .clicked()
                                {
                                    d.request_view = RequestView::Form;
                                }
                                if ui
                                    .selectable_label(d.request_view == RequestView::Json, "JSON")
                                    .clicked()
                                {
                                    d.request_view = RequestView::Json;
                                }
                            }
                        });
                        let assist_height = if d.editor_section == EditorSection::Request
                            && d.request_view == RequestView::Json
                        {
                            request_editor_footer_height(d)
                        } else {
                            0.0
                        };
                        let editor_height = (ui.available_height() - assist_height).max(120.0);
                        let editor_content_height = (editor_height - 16.0).max(104.0);
                        let mut form_changed = false;
                        let mut editor_response = None;
                        let is_json_editor = d.editor_section == EditorSection::Request
                            && d.request_view == RequestView::Json;
                        let editor_snapshot = is_json_editor.then(|| d.snapshot());
                        let editor_frame = egui::Frame::NONE
                            .fill(ui.visuals().extreme_bg_color)
                            .stroke(surface_stroke)
                            .corner_radius(6)
                            .inner_margin(egui::Margin::same(8))
                            .show(ui, |ui| {
                                ui.set_min_height(editor_content_height);
                                match d.editor_section {
                                    EditorSection::Tests => {
                                        egui::ScrollArea::vertical()
                                            .id_salt("v1-tests-editor")
                                            .auto_shrink([false, false])
                                            .show(ui, |ui| {
                                                assertions_editor(ui, d);
                                                ui.add_space(12.0);
                                                ui.separator();
                                                ui.add_space(8.0);
                                                hooks_editor(ui, d);
                                            });
                                        ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover())
                                    }
                                    EditorSection::Request if d.request_view == RequestView::Form => {
                                        form_changed = request_form(ui, d);
                                        ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover())
                                    }
                                    EditorSection::Request => {
                                        let diagnostic = d.json_diagnostic.clone();
                                        egui_extras::StripBuilder::new(ui)
                                            .size(egui_extras::Size::remainder())
                                            .size(egui_extras::Size::exact(CODE_MINIMAP_WIDTH))
                                            .horizontal(|mut strip| {
                                                strip.cell(|ui| {
                                                    editor_response = Some(
                                                        egui::ScrollArea::both()
                                                            .id_salt("v1-json")
                                                            .max_height(editor_content_height)
                                                            .auto_shrink([false, false])
                                                            .show(ui, |ui| {
                                                                code_editor_numbered_diagnostic(
                                                                    ui,
                                                                    "v1-request-json",
                                                                    &mut d.text,
                                                                    Lang::Json,
                                                                    None,
                                                                    false,
                                                                    18,
                                                                    false,
                                                                    diagnostic.as_ref(),
                                                                )
                                                            })
                                                            .inner,
                                                    );
                                                });
                                                strip.cell(|ui| {
                                                    code_minimap(
                                                        ui,
                                                        &d.text,
                                                        diagnostic.as_ref(),
                                                        ui.available_height(),
                                                    );
                                                });
                                            });
                                        editor_response.unwrap_or_else(|| {
                                            ui.allocate_response(
                                                egui::Vec2::ZERO,
                                                egui::Sense::hover(),
                                            )
                                        })
                                    }
                                }
                            });
                        zoom_target_hovered |= ui.rect_contains_pointer(editor_frame.response.rect);
                        let response = editor_frame.inner;
                        if response.changed() || form_changed {
                            d.dirty = true;
                            d.clear_preview();
                            if let Some(snapshot) = editor_snapshot {
                                d.record_undo(snapshot);
                            }
                            if is_json_editor {
                                schedule_editor_validation(d, ui.ctx());
                            }
                        }
                        if d.editor_section == EditorSection::Request
                            && d.request_view == RequestView::Json
                        {
                            if let Some(diagnostic) = &d.json_diagnostic {
                                ui.colored_label(
                                    ui.visuals().error_fg_color,
                                    format!(
                                        "Line {}, column {}: {}",
                                        diagnostic.line, diagnostic.column, diagnostic.message
                                    ),
                                );
                            }
                        }
                        if d.editor_section == EditorSection::Request
                            && d.request_view == RequestView::Json
                        {
                            openapi_assist(ui, d, &response);
                        }
                    });

                    let splitter = ui.allocate_response(
                        egui::vec2(ui.available_width(), 8.0),
                        egui::Sense::drag(),
                    );
                    ui.painter().hline(
                        splitter.rect.x_range(),
                        splitter.rect.center().y,
                        ui.visuals().widgets.noninteractive.bg_stroke,
                    );
                    if splitter.dragged() && total_h > 1.0 {
                        d.split_ratio =
                            ((top_h + splitter.drag_delta().y) / total_h).clamp(0.2, 0.85);
                    }
                    if splitter.hovered() || splitter.dragged() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                    }

                    let results = ui.allocate_ui(
                        egui::vec2(ui.available_width(), ui.available_height()),
                        |ui| results_pane(ui, d, editor_font_size),
                    );
                    zoom_target_hovered |= ui.rect_contains_pointer(results.response.rect);
                },
            );
        });
    });

    if zoom_target_hovered {
        let zoom_delta = ui.input(|input| {
            if input.modifiers.ctrl {
                input.zoom_delta()
            } else {
                1.0
            }
        });
        if (zoom_delta - 1.0).abs() > f32::EPSILON {
            editor_font_size = zoom_editor_font_size(editor_font_size, zoom_delta);
            ui.ctx().request_repaint();
        }
    }

    if d.catalog_open {
        let mut catalog_open = d.catalog_open;
        let context = d.catalog_context;
        egui::Window::new(context.label())
            .id(egui::Id::new("v1-catalog-picker"))
            .open(&mut catalog_open)
            .resizable(true)
            .default_size([490.0, 610.0])
            .show(ui.ctx(), |ui| {
                if context != CatalogContext::General {
                    ui.weak(match context {
                        CatalogContext::Assertion => {
                            "Choose and configure a check for this request."
                        }
                        CatalogContext::Hook => {
                            "Choose and configure a preparation or capture step."
                        }
                        CatalogContext::Body => {
                            "Choose a data asset and use it as the request body."
                        }
                        CatalogContext::General => "Reusable request behavior and data.",
                    });
                }
                palette(ui, d, bridge, &mut pending_insert);
            });
        d.catalog_open = catalog_open;
    }

    if let Some(insert) = pending_insert {
        let target = insert.target;
        match apply_insert(d, insert) {
            Ok((text, notice)) => {
                if let Some(text) = text {
                    d.text = text;
                    validate_editor_json(d);
                    d.body_draft = None;
                    d.body_draft_origin = None;
                    d.body_draft_error = None;
                }
                d.dirty = true;
                d.catalog_error = None;
                d.catalog_notice = Some(notice);
                d.clear_preview();
                d.catalog_open = false;
                match target {
                    InsertTarget::Assertion | InsertTarget::Pipeline => {
                        d.editor_section = EditorSection::Tests;
                    }
                    InsertTarget::Body => {
                        d.editor_section = EditorSection::Request;
                        d.request_view = RequestView::Form;
                    }
                    InsertTarget::Binding | InsertTarget::Mock => {}
                }
            }
            Err(error) => d.catalog_error = Some(error),
        }
    }
    if d.close_prompt_open {
        let mut open = d.close_prompt_open;
        let action_label = d
            .pending_close_action
            .as_ref()
            .map(PendingEditorAction::prompt_label)
            .unwrap_or("close this request");
        egui::Window::new("Unsaved changes")
            .id(egui::Id::new("v1-unsaved-close"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.label(format!("This request has unsaved changes. Choose what to do before you {action_label}.") );
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            d.body_draft_error.is_none()
                                && d.invalid_pipeline_draft().is_none(),
                            egui::Button::new("Save and continue"),
                        )
                        .clicked()
                        && d.save_before_pending_action()
                    {
                        d.close_prompt_open = false;
                        if let Err(error) = d.finish_pending_editor_action(ui.ctx()) {
                            d.diagnostics = vec![error];
                        }
                    }
                    if ui.button("Discard and continue").clicked() {
                        d.discard_pending_editor_changes();
                        d.close_prompt_open = false;
                        if let Err(error) = d.finish_pending_editor_action(ui.ctx()) {
                            d.diagnostics = vec![error];
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        d.cancel_pending_editor_action();
                        state.pending_workspace = None;
                        state.pending_api_project = None;
                        state.open_request_after_workspace = false;
                    }
                });
            });
        if !open {
            d.cancel_pending_editor_action();
            state.pending_workspace = None;
            state.pending_api_project = None;
            state.open_request_after_workspace = false;
        }
    }
    if refresh_project {
        if let Some(root) = state.assets.project_root() {
            state.assets.load(root);
        }
    }
    if (editor_font_size - state.editor_font_size).abs() > f32::EPSILON {
        state.editor_font_size = editor_font_size;
        crate::dialogs::settings::apply_typography(ui.ctx(), state);
    }
}

pub(super) fn right_sidebar(
    ui: &mut egui::Ui,
    d: &mut V1EditorState,
    bridge: &Bridge,
    selected_directory: Option<&Path>,
    expanded: bool,
) -> (Option<bool>, bool) {
    if !expanded {
        let mut requested_open = None;
        ui.vertical_centered(|ui| {
            if ui
                .small_button(icons::CODE)
                .on_hover_text("OpenAPI")
                .clicked()
            {
                d.right_tool = RightTool::OpenApi;
                requested_open = Some(true);
            }
            if ui
                .small_button(icons::CHECK)
                .on_hover_text("Contract tests")
                .clicked()
            {
                d.right_tool = RightTool::ContractTests;
                requested_open = Some(true);
            }
            if ui
                .small_button(icons::RUN)
                .on_hover_text("API tests")
                .clicked()
            {
                d.right_tool = RightTool::ApiTests;
                requested_open = Some(true);
            }
            if ui
                .small_button(icons::PULSE)
                .on_hover_text("Load & performance")
                .clicked()
            {
                d.right_tool = RightTool::Performance;
                requested_open = Some(true);
            }
            if ui
                .small_button(icons::CONSOLE)
                .on_hover_text("AI Advisor")
                .clicked()
            {
                d.right_tool = RightTool::Advisor;
                requested_open = Some(true);
            }
        });
        return (requested_open, false);
    }

    let mut requested_open = None;
    let mut generated_suite = false;
    egui::Frame::NONE
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_height((ui.available_height() - 24.0).max(0.0));
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                for (tool, icon) in [
                    (RightTool::OpenApi, icons::CODE),
                    (RightTool::ContractTests, icons::CHECK),
                    (RightTool::ApiTests, icons::RUN),
                    (RightTool::Performance, icons::PULSE),
                    (RightTool::Advisor, icons::CONSOLE),
                ] {
                    let active = d.right_tool == tool;
                    if ui
                        .selectable_label(active, icon)
                        .on_hover_text(tool.label())
                        .clicked()
                    {
                        d.right_tool = tool;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(icons::TRIANGLE_RIGHT)
                        .on_hover_text("Collapse tool window")
                        .clicked()
                    {
                        requested_open = Some(false);
                    }
                });
            });
            ui.separator();
            ui.add_space(8.0);
            match d.right_tool {
                RightTool::OpenApi => openapi_sidebar(ui, d),
                RightTool::ContractTests => {
                    generated_suite |= generated_suite_sidebar(
                        ui,
                        d,
                        selected_directory,
                        forge_core::reqv1::OpenApiSuiteKind::Contract,
                    )
                }
                RightTool::ApiTests => {
                    generated_suite |= generated_suite_sidebar(
                        ui,
                        d,
                        selected_directory,
                        forge_core::reqv1::OpenApiSuiteKind::Api,
                    )
                }
                RightTool::Performance => {
                    generated_suite |= generated_suite_sidebar(
                        ui,
                        d,
                        selected_directory,
                        forge_core::reqv1::OpenApiSuiteKind::K6,
                    )
                }
                RightTool::Advisor => advisor_sidebar(ui, d, bridge),
            }
        });
    (requested_open, generated_suite)
}

pub(super) fn request_editor_footer_height(d: &V1EditorState) -> f32 {
    let diagnostic_height = if d.json_diagnostic.is_some() {
        28.0
    } else {
        0.0
    };
    let assist_height = if d.openapi_error.is_some() || d.openapi.is_none() {
        28.0
    } else if let (Some(spec), Some(document)) = (&d.openapi, &d.validated_document) {
        let matched = spec
            .find_operation(document.request.method, &document.request.url)
            .is_some();
        if matched || spec.suggest(&document.request.url).is_empty() {
            28.0
        } else {
            64.0
        }
    } else {
        0.0
    };
    diagnostic_height + assist_height
}
