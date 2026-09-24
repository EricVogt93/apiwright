//! Run results and diagnostic presentation.

use super::*;

pub(super) fn scale_results_typography(style: &mut egui::Style, editor_font_size: f32) {
    let scale = editor_font_size / crate::state::DEFAULT_EDITOR_FONT_SIZE;
    for text_style in [
        egui::TextStyle::Body,
        egui::TextStyle::Button,
        egui::TextStyle::Small,
        egui::TextStyle::Monospace,
        egui::TextStyle::Heading,
    ] {
        if let Some(font) = style.text_styles.get_mut(&text_style) {
            font.size *= scale;
        }
    }
}

pub(super) fn results_pane(ui: &mut egui::Ui, d: &mut V1EditorState, editor_font_size: f32) {
    scale_results_typography(ui.style_mut(), editor_font_size);
    if let Some(mode) = d.last_run_mock {
        let environment = d
            .last_run_environment
            .as_deref()
            .unwrap_or("no environment");
        let age = d
            .last_run_at
            .map(|ran_at| {
                let seconds = ran_at.elapsed().as_secs();
                if seconds < 60 {
                    format!("{seconds}s ago")
                } else {
                    format!("{}m ago", seconds / 60)
                }
            })
            .unwrap_or_default();
        ui.horizontal(|ui| {
            let mode = if mode { "Mock" } else { "HTTP" };
            if d.in_flight {
                ui.weak(format!("Running: {mode} · {environment}"));
            } else {
                ui.weak(format!("Last run: {mode} · {environment} · {age}"));
            }
            if run_results_are_stale(d) {
                ui.colored_label(ui.visuals().warn_fg_color, "Results are out of date");
            }
        });
    }
    if d.results.len() > 1 {
        let mut selected = None;
        ui.horizontal_wrapped(|ui| {
            ui.strong("Runs:");
            for (index, item) in d.results.iter().enumerate() {
                if ui
                    .selectable_label(d.selected_result == index, &item.label)
                    .clicked()
                {
                    selected = Some(index);
                }
            }
        });
        if let Some(index) = selected {
            d.selected_result = index;
            d.last_response = d.results[index].response.clone();
            d.clear_preview();
        }
        ui.separator();
    }
    // Tab strip with a pass/fail count on the Tests tab.
    let (passed, total) = selected_result(d)
        .map(|r| {
            (
                r.assertions.iter().filter(|a| a.passed).count(),
                r.assertions.len(),
            )
        })
        .unwrap_or((0, 0));
    let tests_label = if total > 0 {
        format!("Tests ({passed}/{total})")
    } else {
        "Tests".to_string()
    };
    let auth_label = if d.project_auth.is_some() {
        "Auth · active"
    } else {
        "Auth"
    };

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = if ui.available_width() < 640.0 {
            12.0
        } else {
            18.0
        };
        let compact = ui.available_width() < 640.0;
        let tabs = [
            (ResultTab::Result, "Response".to_string()),
            (ResultTab::Assertions, tests_label),
            (ResultTab::Auth, auth_label.to_string()),
            (ResultTab::Runtime, "Runtime".to_string()),
            (ResultTab::Diagnostics, "Diagnostics".to_string()),
        ];
        for (which, label) in tabs.iter().take(if compact { 4 } else { tabs.len() }) {
            let active = d.result_tab == *which;
            let text = RichText::new(label).color(if active {
                ui.visuals().hyperlink_color
            } else {
                ui.visuals().weak_text_color()
            });
            let response = ui
                .add(egui::Label::new(text).sense(egui::Sense::click()))
                .on_hover_text(result_tab_help(*which));
            if active {
                ui.painter().line_segment(
                    [
                        egui::pos2(response.rect.left(), response.rect.bottom() + 4.0),
                        egui::pos2(response.rect.right(), response.rect.bottom() + 4.0),
                    ],
                    egui::Stroke::new(2.0, ui.visuals().hyperlink_color),
                );
            }
            if response.clicked() {
                d.result_tab = *which;
            }
        }
        if compact {
            let overflow = &tabs[4..];
            let selected = overflow
                .iter()
                .find(|(tab, _)| *tab == d.result_tab)
                .map(|(_, label)| format!("{}  {label}", icons::ELLIPSIS))
                .unwrap_or_else(|| format!("{}  More", icons::ELLIPSIS));
            ui.menu_button(selected, |ui| {
                for (tab, label) in overflow {
                    if ui
                        .selectable_label(d.result_tab == *tab, label)
                        .on_hover_text(result_tab_help(*tab))
                        .clicked()
                    {
                        d.result_tab = *tab;
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("Show additional response detail tabs");
        }
    });
    ui.add_space(4.0);
    ui.separator();

    egui::ScrollArea::vertical()
        .id_salt("v1-results")
        .auto_shrink([false, false])
        .show(ui, |ui| match d.result_tab {
            ResultTab::Result => result_summary(ui, d),
            ResultTab::Assertions => assertion_results(ui, d),
            ResultTab::Auth => auth_pane(ui, d),
            ResultTab::Runtime => runtime_pane(ui, d),
            ResultTab::Diagnostics => diagnostics_pane(ui, d),
        });
}

fn run_results_are_stale(d: &V1EditorState) -> bool {
    if d.body_draft_error.is_some() {
        return true;
    }
    if d.last_run_mock.is_some_and(|mode| mode != d.mock) {
        return true;
    }
    let current_environment = d.env_name.clone().or_else(|| {
        d.root
            .as_deref()
            .zip(d.file.as_deref())
            .and_then(|(root, file)| {
                forge_core::reqv1::effective_environment(root, file)
                    .ok()
                    .flatten()
                    .map(|selection| selection.value)
            })
    });
    if current_environment != d.last_run_environment {
        return true;
    }
    let current = effective_document(d)
        .and_then(|document| serialize_request(&document))
        .ok();
    current != d.last_run_request
}

pub(super) fn result_tab_help(tab: ResultTab) -> &'static str {
    match tab {
        ResultTab::Result => "Formatted response body, headers and status",
        ResultTab::Assertions => "Pass/fail results from the last run",
        ResultTab::Auth => "Authentication source, refresh policy and token status",
        ResultTab::Runtime => "Execution duration, environment and transport details",
        ResultTab::Diagnostics => "Validation, OpenAPI and execution diagnostics",
    }
}

pub(super) fn result_summary(ui: &mut egui::Ui, d: &mut V1EditorState) {
    let Some(item) = selected_item(d) else {
        ui.allocate_ui_with_layout(
            ui.available_size(),
            egui::Layout::centered_and_justified(egui::Direction::TopDown),
            |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(icons::RUN).size(22.0).weak());
                    ui.strong("No response yet");
                });
            },
        );
        return;
    };
    let r = &item.result;
    if !item.matrix.is_empty() {
        ui.monospace(serde_json::Value::Object(item.matrix.clone()).to_string());
    }
    let (label, color) = match r.status {
        RunStatus::Passed => ("PASSED", egui::Color32::from_rgb(0x49, 0x9C, 0x54)),
        RunStatus::Failed => ("FAILED", egui::Color32::from_rgb(0xC7, 0x5A, 0x3B)),
        RunStatus::Error => ("ERROR", ui.visuals().error_fg_color),
        RunStatus::Skipped => ("SKIPPED", egui::Color32::from_rgb(0xC7, 0x7D, 0x2E)),
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(color).strong());
        if let Some(http) = &r.http {
            ui.label(format!(
                "{} · {} ms · {} bytes",
                http.status, http.time_ms, http.bytes
            ));
        }
    });
    if let Some(reason) = &r.skip_reason {
        ui.label(reason);
    }
    let (passed, total) = (
        r.assertions.iter().filter(|a| a.passed).count(),
        r.assertions.len(),
    );
    if total > 0 {
        ui.label(format!("{passed}/{total} assertion(s) passed"));
    }
    if !r.runtime.is_empty() {
        ui.label(format!("{} runtime value(s) extracted", r.runtime.len()));
    }
    let Some(response) = d.last_response.clone() else {
        return;
    };
    ui.add_space(6.0);
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        ui.strong(format!("HTTP {}", response.status));
        ui.weak(format!(
            "{} ms · {} bytes",
            response.time_ms,
            response.body.len()
        ));
        ui.checkbox(&mut d.response_raw, "Raw");
        if ui.button(format!("{}  Copy", icons::COPY)).clicked() {
            ui.ctx().copy_text(response.text().into_owned());
        }
    });
    for issue in openapi_response_issues(d, &response) {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!("{} {issue}", icons::WARNING),
        );
    }
    let response_text = response.text().into_owned();
    let json = response.json();
    let markup = response_is_markup(&response, &response_text);
    let mut body = if d.response_raw {
        response_text
    } else if let Some(value) = json.as_ref() {
        serde_json::to_string_pretty(value).unwrap_or(response_text)
    } else if markup {
        pretty_markup(&response_text)
    } else {
        response_text
    };
    let language = if json.is_some() {
        Lang::Json
    } else if markup {
        Lang::Xml
    } else {
        Lang::Plain
    };
    egui::ScrollArea::both()
        .id_salt("v1-response-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            code_editor_numbered(
                ui,
                "v1-response-json",
                &mut body,
                language,
                None,
                true,
                8,
                false,
            );
        });
}

pub(super) fn response_is_markup(response: &ResponseView, text: &str) -> bool {
    let starts_with_markup = text.trim_start().to_ascii_lowercase();
    response.header("content-type").is_some_and(|content_type| {
        let content_type = content_type.to_ascii_lowercase();
        content_type.contains("html") || content_type.contains("xml")
    }) || starts_with_markup.starts_with("<!doctype")
        || starts_with_markup.starts_with("<html")
        || starts_with_markup.starts_with("<?xml")
}

pub(super) fn pretty_markup(input: &str) -> String {
    const VOID: &[&str] = &[
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
        "source", "track", "wbr",
    ];
    let mut output = String::new();
    let mut depth = 0usize;
    let mut rest = input.trim();
    while !rest.is_empty() {
        if rest.starts_with('<') {
            let Some(end) = markup_tag_end(rest) else {
                push_markup_line(&mut output, depth, rest);
                break;
            };
            let tag = &rest[..=end];
            let closing = tag.starts_with("</");
            if closing {
                depth = depth.saturating_sub(1);
            }
            push_markup_line(&mut output, depth, tag);
            let name = tag
                .trim_start_matches(['<', '/'])
                .split(|character: char| {
                    character.is_whitespace() || character == '>' || character == '/'
                })
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if !closing
                && !tag.starts_with("<!")
                && !tag.starts_with("<?")
                && !tag.ends_with("/>")
                && !VOID.contains(&name.as_str())
            {
                depth += 1;
            }
            rest = rest[end + 1..].trim_start();
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            push_markup_line(&mut output, depth, rest[..end].trim());
            rest = rest[end..].trim_start();
        }
    }
    output
}

pub(super) fn markup_tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    for (index, character) in tag.char_indices().skip(1) {
        match character {
            '\'' | '"' if quote.is_none() => quote = Some(character),
            character if quote == Some(character) => quote = None,
            '>' if quote.is_none() => return Some(index),
            _ => {}
        }
    }
    None
}

pub(super) fn push_markup_line(output: &mut String, depth: usize, text: &str) {
    if text.is_empty() {
        return;
    }
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(&"  ".repeat(depth));
    output.push_str(text);
}

pub(super) fn openapi_response_issues(d: &V1EditorState, response: &ResponseView) -> Vec<String> {
    let (Some(spec), Ok(document)) = (
        d.openapi.as_ref(),
        forge_core::reqv1::RequestDocument::parse(&d.text),
    ) else {
        return Vec::new();
    };
    let Some(operation) = spec.find_operation(document.request.method, &document.request.url)
    else {
        return vec!["Response cannot be matched to an OpenAPI operation".to_string()];
    };
    let Some(declared) = declared_response(&operation.responses, response.status) else {
        return vec![format!(
            "Status {} is not declared by OpenAPI",
            response.status
        )];
    };
    let mut issues = Vec::new();
    if let Some(expected) = &declared.content_type {
        let actual = response
            .header("content-type")
            .and_then(|value| value.split(';').next());
        if actual.is_none_or(|actual| !actual.eq_ignore_ascii_case(expected)) {
            issues.push(format!(
                "Response content type is {}, expected {expected}",
                actual.unwrap_or("missing")
            ));
        }
    }
    if let Some(schema) = &declared.schema {
        match response.json() {
            Some(body) => {
                let schema = forge_core::openapi::scrub_schema(schema.clone());
                if let Err(errors) = forge_core::assert::schema::validate(&schema, &body) {
                    issues.push(format!("Response body: {}", errors.join(", ")));
                }
            }
            None => issues.push("Response body is not valid JSON".to_string()),
        }
    }
    issues
}

pub(super) fn declared_response(responses: &[SpecResponse], status: u16) -> Option<&SpecResponse> {
    let exact = status.to_string();
    responses
        .iter()
        .find(|response| response.status == exact)
        .or_else(|| {
            let class = format!("{}XX", status / 100);
            responses
                .iter()
                .find(|response| response.status.eq_ignore_ascii_case(&class))
        })
        .or_else(|| {
            responses
                .iter()
                .find(|response| response.status == "default")
        })
}

pub(super) fn runtime_pane(ui: &mut egui::Ui, d: &V1EditorState) {
    match selected_result(d) {
        Some(r) if !r.runtime.is_empty() => {
            for (k, v) in &r.runtime {
                ui.label(RichText::new(format!("{k} = {v}")).monospace());
            }
        }
        Some(_) => {
            ui.weak("No runtime values extracted.");
        }
        None => {
            ui.weak("No run yet.");
        }
    }
}

pub(super) fn diagnostics_pane(ui: &mut egui::Ui, d: &V1EditorState) {
    // Validate/parse messages first, then the run's diagnostics.
    for msg in &d.diagnostics {
        ui.colored_label(ui.visuals().error_fg_color, msg);
    }
    if let Some(r) = selected_result(d) {
        for diag in &r.diagnostics {
            let color = if diag.severity == forge_core::reqv1::Severity::Error {
                ui.visuals().error_fg_color
            } else {
                ui.visuals().warn_fg_color
            };
            ui.colored_label(color, format!("[{}] {}", diag.code, diag.message));
        }
    }
    if d.diagnostics.is_empty()
        && selected_result(d)
            .map(|r| r.diagnostics.is_empty())
            .unwrap_or(true)
    {
        ui.weak("No diagnostics.");
    }
}

pub(super) fn selected_item(d: &V1EditorState) -> Option<&V1RunItem> {
    d.results.get(d.selected_result)
}

pub(super) fn selected_result(d: &V1EditorState) -> Option<&RunResult> {
    selected_item(d).map(|item| &item.result)
}
