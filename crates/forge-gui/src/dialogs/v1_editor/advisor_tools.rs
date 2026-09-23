//! Advisor context, redaction, and sidebar.

use super::*;

pub(super) fn advisor_sidebar(ui: &mut egui::Ui, d: &mut V1EditorState, bridge: &Bridge) {
    let ready =
        !d.advisor_config.endpoint.trim().is_empty() && !d.advisor_config.model.trim().is_empty();
    let active_file = d
        .file
        .as_deref()
        .and_then(|file| {
            d.root
                .as_deref()
                .and_then(|root| file.strip_prefix(root).ok())
                .or(Some(file))
        })
        .map(|file| file.display().to_string())
        .unwrap_or_else(|| "Unsaved request".to_string());
    ui.horizontal_wrapped(|ui| {
        ui.strong("Context");
        ui.monospace(&active_file)
            .on_hover_text("The currently open request is always included and redacted.");
    });
    ui.add_space(8.0);
    egui::CollapsingHeader::new("Connection")
        .default_open(!ready)
        .show(ui, |ui| {
            ui.label("OpenAI-compatible base URL");
            ui.add(
                TextEdit::singleline(&mut d.advisor_config.endpoint)
                    .hint_text("http://localhost:11434/v1"),
            );
            ui.label("Model");
            ui.add(TextEdit::singleline(&mut d.advisor_config.model).hint_text("model-name"));
            ui.label("API key variable (optional)")
                .on_hover_text("Only the variable name is saved locally; never the key value.");
            ui.add(
                TextEdit::singleline(&mut d.advisor_config.api_key_env).hint_text("OPENAI_API_KEY"),
            );
            if ui.small_button("Save connection").clicked() {
                match d
                    .root
                    .as_deref()
                    .ok_or_else(|| "no project root".to_string())
                    .and_then(|root| crate::advisor::save(root, &d.advisor_config))
                {
                    Ok(()) => d.advisor_error = None,
                    Err(error) => d.advisor_error = Some(error),
                }
            }
        });
    ui.add_space(8.0);
    ui.label("Question");
    ui.add(
        TextEdit::multiline(&mut d.advisor_question)
            .desired_rows(4)
            .hint_text("What should the advisor review?"),
    );
    ui.add_enabled_ui(d.last_response.is_some(), |ui| {
        ui.checkbox(&mut d.advisor_include_response, "Include last response");
    });
    ui.label("Redacted context").on_hover_text(
        "The current request is always included. Sensitive values are redacted before sending.",
    );
    ui.add_space(6.0);
    let asking = d.active_advisor.is_some();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(ready && !asking, egui::Button::new("Ask advisor"))
            .clicked()
        {
            start_advisor(d, bridge);
        }
        if asking {
            ui.spinner();
        }
        if d.advisor_answer.is_some() && ui.small_button("Copy").clicked() {
            ui.ctx()
                .copy_text(d.advisor_answer.clone().unwrap_or_default());
        }
    });
    if let Some(error) = &d.advisor_error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    if let Some(answer) = &d.advisor_answer {
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .id_salt("advisor-answer")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(egui::Label::new(answer).wrap().selectable(true));
            });
    }
}

pub(super) fn start_advisor(d: &mut V1EditorState, bridge: &Bridge) {
    let Some(root) = d.root.clone() else {
        d.advisor_error = Some("no project root".to_string());
        return;
    };
    if let Err(error) = crate::advisor::save(&root, &d.advisor_config) {
        d.advisor_error = Some(error);
        return;
    }
    let context = match advisor_context(d) {
        Ok(context) => context,
        Err(error) => {
            d.advisor_error = Some(error);
            return;
        }
    };
    d.next_advisor_id += 1;
    let advisor_id = d.next_advisor_id;
    d.active_advisor = Some(advisor_id);
    d.advisor_answer = None;
    d.advisor_error = None;
    if let Err(error) = bridge.send(Cmd::AskAdvisor {
        advisor_id,
        root,
        config: d.advisor_config.clone(),
        question: d.advisor_question.clone(),
        context,
    }) {
        d.active_advisor = None;
        d.advisor_error = Some(error);
    }
}

pub(super) fn advisor_context(d: &V1EditorState) -> Result<String, String> {
    let mut request: serde_json::Value =
        serde_json::from_str(&d.text).map_err(|error| format!("invalid request JSON: {error}"))?;
    redact_sensitive_json(&mut request);
    let file = d
        .file
        .as_deref()
        .and_then(|file| {
            d.root
                .as_deref()
                .and_then(|root| file.strip_prefix(root).ok())
                .or(Some(file))
        })
        .map(|file| file.display().to_string())
        .unwrap_or_else(|| "unsaved request".to_string());
    let mut sections = vec![format!(
        "Workspace context:\nroot={}\nactive_file={}\nThe active file is authoritative; use the surrounding project files below only as supporting context.",
        d.root.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "unknown".into()),
        file,
    ), format!(
        "Current file: {file}\nRequest document:\n{}",
        serde_json::to_string_pretty(&request).map_err(|error| error.to_string())?
    )];

    // Keep the advisor useful without asking the user to curate a file list.
    // These are the files that define how this request behaves at runtime.
    for (label, value) in [
        ("Assertions", serde_json::to_value(&d.assertions).ok()),
        ("Hooks", serde_json::to_value(&d.hooks).ok()),
        ("Auth", serde_json::to_value(&d.project_auth).ok()),
    ] {
        if let Some(mut value) = value {
            redact_sensitive_json(&mut value);
            sections.push(format!(
                "Active file {label} sidecar:\n{}",
                serde_json::to_string_pretty(&value).unwrap_or_default()
            ));
        }
    }

    if let Some(root) = &d.root {
        for relative in ["project.json", "forge.json"] {
            let path = root.join(relative);
            if let Ok(text) = std::fs::read_to_string(&path) {
                sections.push(format!(
                    "Project metadata ({relative}):\n{}",
                    truncate_text(&text, 8_000)
                ));
            }
        }
        if let Some(source) = &d.openapi_source {
            if let Ok(text) = std::fs::read_to_string(source) {
                sections.push(format!(
                    "OpenAPI source ({}):\n{}",
                    source.strip_prefix(root).unwrap_or(source).display(),
                    truncate_text(&text, 16_000)
                ));
            }
        }
        let mut related = Vec::new();
        let current = d.file.as_deref();
        if let Some(entries) = d
            .file
            .as_deref()
            .and_then(Path::parent)
            .and_then(|dir| std::fs::read_dir(dir).ok())
        {
            for path in entries
                .flatten()
                .map(|entry| entry.path())
                .take(20)
                .filter(|path| Some(path.as_path()) != current && path.is_file())
                .filter(|path| {
                    matches!(
                        path.extension().and_then(|e| e.to_str()),
                        Some("json" | "yaml" | "yml" | "js")
                    )
                })
            {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                related.push(format!(
                    "{}:\n{}",
                    path.strip_prefix(root).unwrap_or(&path).display(),
                    truncate_text(&text, 6_000)
                ));
            }
        }
        if !related.is_empty() {
            sections.push(format!(
                "Related files in the active folder:\n{}",
                related.join("\n\n")
            ));
        }
    }

    if let (Some(spec), Ok(document)) = (
        d.openapi.as_ref(),
        forge_core::reqv1::RequestDocument::parse(&d.text),
    ) {
        if let Some(operation) = spec.find_operation(document.request.method, &document.request.url)
        {
            let contract = serde_json::json!({
                "title": spec.title,
                "version": spec.version,
                "operationId": operation.id,
                "method": operation.method.as_str(),
                "path": operation.path,
                "summary": operation.summary,
                "pathParameters": operation.path_params,
                "queryParameters": operation.query_params,
                "headerParameters": operation.header_params,
                "requestContentType": operation.request_content_type,
                "requestSchema": operation.request_schema,
                "responses": operation.responses.iter().map(|response| serde_json::json!({
                    "status": response.status,
                    "contentType": response.content_type,
                    "schema": response.schema,
                })).collect::<Vec<_>>(),
            });
            sections.push(format!(
                "Matching OpenAPI operation:\n{}",
                serde_json::to_string_pretty(&contract).map_err(|error| error.to_string())?
            ));
        }
    }

    if d.advisor_include_response {
        if let Some(response) = &d.last_response {
            let headers = response
                .headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        if sensitive_name(name) {
                            "***".to_string()
                        } else {
                            value.clone()
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let body = if let Some(mut json) = response.json() {
                redact_sensitive_json(&mut json);
                serde_json::to_string_pretty(&json).unwrap_or_default()
            } else {
                response.text().into_owned()
            };
            sections.push(format!(
                "Last response:\nstatus={}\ntimeMs={}\nheaders={}\nbody={} ",
                response.status,
                response.time_ms,
                serde_json::to_string(&headers).unwrap_or_default(),
                truncate_text(&body, 12_000)
            ));
        }
    }
    Ok(truncate_text(&sections.join("\n\n"), 48_000))
}

pub(super) fn redact_sensitive_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            let masks_header = object
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(sensitive_name);
            for (name, value) in object {
                if sensitive_name(name) || (masks_header && name == "value") {
                    *value = serde_json::Value::String("***".to_string());
                } else {
                    redact_sensitive_json(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_sensitive_json(value);
            }
        }
        _ => {}
    }
}

pub(super) fn sensitive_name(name: &str) -> bool {
    let name = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "authorization",
        "apikey",
        "password",
        "secret",
        "token",
        "cookie",
    ]
    .iter()
    .any(|sensitive| name.contains(sensitive))
}

pub(super) fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}\n… truncated")
    } else {
        truncated
    }
}
