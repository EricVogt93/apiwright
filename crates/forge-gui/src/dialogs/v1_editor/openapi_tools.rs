//! OpenAPI browsing, request assistance, and suite generation.

use super::*;

pub(super) fn generated_suite_sidebar(
    ui: &mut egui::Ui,
    d: &mut V1EditorState,
    selected_directory: Option<&Path>,
    kind: forge_core::reqv1::OpenApiSuiteKind,
) -> bool {
    let (title, action, tooltip) = match kind {
        forge_core::reqv1::OpenApiSuiteKind::Contract => (
            "Contract tests",
            "Generate contract tests",
            "Creates runnable requests with status, content-type and response-schema assertions.",
        ),
        forge_core::reqv1::OpenApiSuiteKind::Api => (
            "API tests",
            "Generate API tests",
            "Creates one complete request per operation, assertion sidecars and an ordered sequence.",
        ),
        forge_core::reqv1::OpenApiSuiteKind::K6 => (
            "k6 performance",
            "Generate k6 suite",
            "Creates smoke, load, stress, spike and soak profiles. Mutating methods are disabled by default.",
        ),
    };
    ui.strong(title);
    let target = selected_directory
        .map(Path::to_path_buf)
        .or_else(|| {
            d.file
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
        })
        .or_else(|| d.root.as_ref().map(|root| root.join("requests")));
    if let Some(target) = &target {
        let shown = d
            .root
            .as_deref()
            .and_then(|root| target.strip_prefix(root).ok())
            .unwrap_or(target)
            .join(kind.folder());
        ui.monospace(shown.display().to_string());
    }
    ui.add_space(10.0);

    let ready = d.openapi.is_some() && d.root.is_some() && target.is_some();
    let clicked = ui
        .add_enabled(ready, egui::Button::new(action))
        .on_hover_text(tooltip)
        .clicked();
    if !ready {
        ui.weak("OpenAPI and a project folder are required.");
    }
    if let Some(error) = &d.suite_error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    if let Some(notice) = &d.suite_notice {
        ui.colored_label(ui.visuals().hyperlink_color, notice);
    }
    if !clicked {
        return false;
    }

    let result = forge_core::reqv1::generate_openapi_suite(
        d.root.as_deref().expect("ready checked"),
        target.as_deref().expect("ready checked"),
        d.openapi.as_ref().expect("ready checked"),
        kind,
    );
    match result {
        Ok(generated) => {
            let warning = if generated.warnings.is_empty() {
                String::new()
            } else {
                format!(" · {} warning(s)", generated.warnings.len())
            };
            d.suite_notice = Some(format!(
                "Generated {} request(s), {} file(s){warning}",
                generated.requests, generated.files
            ));
            d.suite_error = None;
            true
        }
        Err(error) => {
            d.suite_notice = None;
            d.suite_error = Some(error);
            false
        }
    }
}

pub(super) fn openapi_sidebar(ui: &mut egui::Ui, d: &mut V1EditorState) {
    if let Some(error) = &d.openapi_error {
        ui.colored_label(ui.visuals().error_fg_color, error);
        return;
    }
    let Some(spec) = &d.openapi else {
        ui.strong("No OpenAPI spec found").on_hover_text(
            "Set a source in folder properties or add openapi.json/yaml below the project.",
        );
        return;
    };

    let title = spec.title.clone();
    let version = spec.version.clone();
    let servers = spec.servers.clone();
    let operations = spec.operations.clone();
    let docs_url = openapi_docs_url(spec).map(str::to_string);
    ui.strong(title);
    ui.label(RichText::new(format!("v{version} · {} operations", operations.len())).weak());
    if let Some(server) = servers.first() {
        ui.monospace(server);
    }
    ui.horizontal(|ui| {
        if let Some(source) = &d.openapi_source {
            if ui.small_button("Open spec").clicked() {
                let _ = open::that(source);
            }
        }
        if let Some(url) = docs_url {
            if ui.small_button("Open Swagger UI").clicked() {
                let _ = open::that(url);
            }
        }
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(icons::SEARCH);
        ui.add(
            TextEdit::singleline(&mut d.openapi_query)
                .hint_text("Filter operations")
                .desired_width(ui.available_width()),
        );
    });
    egui::ComboBox::from_id_salt("openapi-operation-filter")
        .selected_text(d.openapi_filter.label())
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut d.openapi_filter, OpenApiFilter::All, "All operations");
            ui.separator();
            for method in Method::ALL {
                ui.selectable_value(
                    &mut d.openapi_filter,
                    OpenApiFilter::Method(method),
                    method.as_str(),
                );
            }
            ui.separator();
            for filter in [
                OpenApiFilter::Headers,
                OpenApiFilter::Query,
                OpenApiFilter::Path,
                OpenApiFilter::Body,
            ] {
                ui.selectable_value(&mut d.openapi_filter, filter, filter.label());
            }
        });

    let query = d.openapi_query.trim().to_ascii_lowercase();
    let mut filtered = operations
        .iter()
        .filter(|operation| {
            d.openapi_filter.matches(operation) && operation_matches_query(operation, &query)
        })
        .cloned()
        .collect::<Vec<_>>();
    filtered.sort_by(|left, right| {
        method_rank(left.method)
            .cmp(&method_rank(right.method))
            .then_with(|| left.path.cmp(&right.path))
    });

    ui.add_space(6.0);
    egui::ScrollArea::vertical()
        .id_salt("openapi-operations")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut last_method = None;
            for operation in &filtered {
                if last_method != Some(operation.method) {
                    if last_method.is_some() {
                        ui.add_space(6.0);
                    }
                    ui.label(RichText::new(operation.method.as_str()).small().strong());
                    ui.separator();
                    last_method = Some(operation.method);
                }
                let auto_covered = d.auto_covered_operations.contains(&operation.id)
                    || request_covers_operation(&d.text, operation);
                let covered = auto_covered || d.marked_operations.contains(&operation.id);
                egui::Frame::NONE
                    .fill(ui.visuals().faint_bg_color)
                    .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                    .corner_radius(6)
                    .inner_margin(egui::Margin::same(9))
                    .show(ui, |ui| {
                        let mark_response = ui
                            .horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{}  {}",
                                        operation.method.as_str(),
                                        operation.path
                                    ))
                                    .monospace()
                                    .strong()
                                    .color(method_color(operation.method)),
                                );
                                ui.label(
                                    RichText::new(format!("[{}]", operation_rule(operation)))
                                        .small()
                                        .monospace()
                                        .weak(),
                                )
                                .on_hover_text(operation_rule_help(operation));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let color = if covered {
                                            if ui.visuals().dark_mode {
                                                crate::theme::darcula::OK
                                            } else {
                                                crate::theme::light::OK
                                            }
                                        } else {
                                            ui.visuals().weak_text_color()
                                        };
                                        ui.add_sized(
                                            [24.0, 24.0],
                                            egui::Button::new(
                                                RichText::new(icons::CHECK).strong().color(color),
                                            )
                                            .frame(false),
                                        )
                                    },
                                )
                                .inner
                            })
                            .inner
                            .on_hover_text(if auto_covered {
                                "Covered by the current request"
                            } else if covered {
                                "Marked as covered · click to clear"
                            } else {
                                "Mark as covered"
                            });
                        if mark_response.clicked() && !auto_covered {
                            if !d.marked_operations.remove(&operation.id) {
                                d.marked_operations.insert(operation.id.clone());
                            }
                            if let Some(root) = &d.root {
                                if let Err(error) =
                                    save_marked_operations(root, &d.marked_operations)
                                {
                                    d.diagnostics = vec![error];
                                }
                            }
                        }
                        if !operation.summary.is_empty() {
                            ui.label(&operation.summary);
                        }
                        ui.horizontal_wrapped(|ui| {
                            if ui.small_button("Add to request").clicked() {
                                apply_openapi_to_editor(d, operation, false);
                            }
                            if ui.small_button("Generate custom value").clicked() {
                                apply_openapi_to_editor(d, operation, true);
                            }
                        });
                    });
                ui.add_space(6.0);
            }
            if filtered.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No matching operations");
                });
            }
        });
}

pub(super) fn method_rank(method: Method) -> usize {
    Method::ALL
        .iter()
        .position(|candidate| *candidate == method)
        .unwrap_or(usize::MAX)
}

pub(super) fn operation_rule(operation: &SpecOperation) -> &'static str {
    if operation.request_schema.is_some() {
        "sch"
    } else if !operation.path_params.is_empty()
        || operation.query_params.iter().any(|(_, required)| *required)
        || operation
            .header_params
            .iter()
            .any(|(_, required)| *required)
    {
        "req"
    } else {
        "opt"
    }
}

pub(super) fn operation_rule_help(operation: &SpecOperation) -> &'static str {
    match operation_rule(operation) {
        "sch" => "Request body is constrained by a schema",
        "req" => "Operation has required parameters",
        _ => "Operation has no required input",
    }
}

pub(super) fn request_covers_operation(text: &str, operation: &SpecOperation) -> bool {
    forge_core::reqv1::RequestDocument::parse(text)
        .ok()
        .is_some_and(|document| {
            document.request.method == operation.method
                && forge_core::openapi::path_matches_template(
                    &operation.path,
                    &forge_core::openapi::url_to_path(&document.request.url),
                )
        })
}

pub(super) fn scan_covered_operations(
    root: Option<&Path>,
    index: Option<&ProjectIndex>,
    spec: Option<&ParsedSpec>,
) -> BTreeSet<String> {
    let (Some(root), Some(index), Some(spec)) = (root, index, spec) else {
        return BTreeSet::new();
    };
    index
        .requests
        .iter()
        .filter_map(|request| std::fs::read_to_string(root.join(&request.rel_path)).ok())
        .filter_map(|text| forge_core::reqv1::RequestDocument::parse(&text).ok())
        .filter_map(|document| {
            spec.find_operation(document.request.method, &document.request.url)
                .map(|operation| operation.id.clone())
        })
        .collect()
}

pub(super) fn apply_openapi_to_editor(
    d: &mut V1EditorState,
    operation: &SpecOperation,
    generated: bool,
) {
    let result = apply_openapi_operation(&d.text, operation).and_then(|text| {
        if generated {
            apply_generated_openapi_values(&text, operation)
        } else {
            Ok(text)
        }
    });
    match result {
        Ok(text) => {
            d.text = text;
            d.dirty = true;
            d.diagnostics.clear();
            d.clear_preview();
            validate_editor_json(d);
        }
        Err(error) => {
            d.diagnostics = vec![error];
            d.result_tab = ResultTab::Diagnostics;
        }
    }
}

pub(super) fn operation_matches_query(operation: &SpecOperation, query: &str) -> bool {
    query.is_empty()
        || operation.path.to_ascii_lowercase().contains(query)
        || operation.summary.to_ascii_lowercase().contains(query)
        || operation.id.to_ascii_lowercase().contains(query)
        || operation
            .tags
            .iter()
            .any(|tag| tag.to_ascii_lowercase().contains(query))
}

pub(super) fn openapi_docs_url(spec: &ParsedSpec) -> Option<&str> {
    spec.raw
        .get("x-swagger-ui-url")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            spec.raw
                .pointer("/externalDocs/url")
                .and_then(serde_json::Value::as_str)
        })
        .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
}

pub(super) fn method_color(method: Method) -> egui::Color32 {
    match method {
        Method::Get => egui::Color32::from_rgb(0x61, 0xAF, 0xEF),
        Method::Post => egui::Color32::from_rgb(0x47, 0xC9, 0x82),
        Method::Put | Method::Patch => egui::Color32::from_rgb(0xE5, 0xC0, 0x7B),
        Method::Delete => egui::Color32::from_rgb(0xE0, 0x6C, 0x75),
        _ => egui::Color32::from_rgb(0xC6, 0x78, 0xDD),
    }
}

pub(super) fn openapi_assist(ui: &mut egui::Ui, d: &mut V1EditorState, response: &egui::Response) {
    if let Some(error) = &d.openapi_error {
        ui.colored_label(ui.visuals().error_fg_color, error);
        return;
    }
    let Some(spec) = &d.openapi else {
        ui.label(
            RichText::new("OpenAPI completion: add openapi.yaml or a spec under specs/.")
                .small()
                .weak(),
        );
        return;
    };
    let Some(document) = d.validated_document.clone() else {
        return;
    };
    let source = d
        .openapi_source
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let matched = spec
        .find_operation(document.request.method, &document.request.url)
        .cloned();
    let suggestions = if matched.is_none() {
        spec.suggest(&document.request.url)
            .into_iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut selected = None;

    if let Some(operation) = matched {
        let issues = openapi_request_issues(&document, &operation);
        ui.horizontal_wrapped(|ui| {
            if issues.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(0x47, 0xC9, 0x82),
                    format!(
                        "OpenAPI · {} {} · {source}",
                        operation.method.as_str(),
                        operation.path
                    ),
                );
            } else {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("{} OpenAPI: {}", icons::WARNING, issues.join(" · ")),
                );
                if ui.small_button("Apply fixes").clicked() {
                    selected = Some(operation.clone());
                }
            }
        });
    } else {
        let allowed = spec
            .operations
            .iter()
            .filter(|operation| {
                forge_core::openapi::path_matches_template(
                    &operation.path,
                    &forge_core::openapi::url_to_path(&document.request.url),
                )
            })
            .map(|operation| operation.method.as_str())
            .collect::<Vec<_>>();
        let message = if allowed.is_empty() {
            "Path is not declared in OpenAPI".to_string()
        } else {
            format!("Method not allowed; use {}", allowed.join(", "))
        };
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!("{} {message}", icons::WARNING),
        );
        if !suggestions.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.weak("Suggestions (Tab selects first):");
                for operation in &suggestions {
                    if ui
                        .small_button(format!("{} {}", operation.method.as_str(), operation.path))
                        .on_hover_text(&operation.summary)
                        .clicked()
                    {
                        selected = Some(operation.clone());
                    }
                }
            });
            if (response.has_focus() || response.lost_focus())
                && ui.input(|input| input.key_pressed(egui::Key::Tab))
            {
                selected = suggestions.first().cloned();
            }
        }
    }

    if let Some(operation) = selected {
        match apply_openapi_operation(&d.text, &operation) {
            Ok(text) => {
                d.text = text;
                d.dirty = true;
                d.diagnostics.clear();
                d.clear_preview();
            }
            Err(error) => {
                d.diagnostics = vec![error];
                d.result_tab = ResultTab::Diagnostics;
            }
        }
    }
}

pub(super) fn openapi_request_issues(
    document: &forge_core::reqv1::RequestDocument,
    operation: &SpecOperation,
) -> Vec<String> {
    let mut issues = Vec::new();
    for name in &operation.path_params {
        if document
            .request
            .url
            .contains(&format!("${{bindings.{name}}}"))
            && !document.bindings.contains_key(name)
        {
            issues.push(format!("missing path binding {name}"));
        }
    }
    for (name, required) in &operation.query_params {
        if *required
            && !document
                .request
                .query
                .iter()
                .any(|parameter| parameter.enabled && parameter.name.eq_ignore_ascii_case(name))
        {
            issues.push(format!("missing query {name}"));
        }
    }
    for (name, required) in &operation.header_params {
        if *required
            && !document
                .request
                .headers
                .iter()
                .any(|header| header.enabled && header.name.eq_ignore_ascii_case(name))
        {
            issues.push(format!("missing header {name}"));
        }
    }
    if let Some(content_type) = &operation.request_content_type {
        let matches = document.request.headers.iter().any(|header| {
            header.enabled
                && header.name.eq_ignore_ascii_case("content-type")
                && header
                    .value
                    .split(';')
                    .next()
                    .is_some_and(|value| value.eq_ignore_ascii_case(content_type))
        });
        if !matches {
            issues.push(format!("content type should be {content_type}"));
        }
    }
    if let (Some(schema), Some(BodySpec::Inline(body))) =
        (&operation.request_schema, &document.request.body)
    {
        if let Some(value) = &body.value {
            let schema = forge_core::openapi::scrub_schema(schema.clone());
            if let Err(errors) = forge_core::assert::schema::validate(&schema, value) {
                issues.push(format!("body: {}", errors.join(", ")));
            }
        }
    }
    issues
}

pub(super) fn apply_openapi_operation(
    text: &str,
    operation: &SpecOperation,
) -> Result<String, String> {
    let mut document = forge_core::reqv1::RequestDocument::parse(text)
        .map_err(|error| format!("fix the request JSON before completion: {error}"))?;
    document.request.method = operation.method;
    let prefix = request_url_prefix(&document.request.url);
    let mut path = operation.path.clone();
    for name in &operation.path_params {
        path = path.replace(&format!("{{{name}}}"), &format!("${{bindings.{name}}}"));
        document
            .bindings
            .entry(name.clone())
            .or_insert_with(|| Binding::Value(ValueBinding { value: "".into() }));
    }
    document.request.url = format!("{prefix}{path}");
    for (name, required) in &operation.query_params {
        if *required
            && !document
                .request
                .query
                .iter()
                .any(|parameter| parameter.name.eq_ignore_ascii_case(name))
        {
            document.request.query.push(HeaderSpec {
                name: name.clone(),
                value: String::new(),
                enabled: true,
            });
        }
    }
    for (name, required) in &operation.header_params {
        if *required
            && !document
                .request
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case(name))
        {
            document.request.headers.push(HeaderSpec {
                name: name.clone(),
                value: String::new(),
                enabled: true,
            });
        }
    }
    if let Some(content_type) = &operation.request_content_type {
        if !document
            .request
            .headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case("content-type"))
        {
            document.request.headers.push(HeaderSpec {
                name: "Content-Type".to_string(),
                value: content_type.clone(),
                enabled: true,
            });
        }
    }
    if document.request.body.is_none() {
        let value = operation.request_example.clone().or_else(|| {
            operation
                .request_schema
                .as_ref()
                .map(|schema| forge_core::openapi::example_from_schema(schema, 0))
        });
        if let Some(value) = value {
            document.request.body = Some(BodySpec::Inline(InlineBody {
                body_type: BodyType::Json,
                value: Some(value),
            }));
        }
    }
    let mut text = serde_json::to_string_pretty(&document).map_err(|error| error.to_string())?;
    text.push('\n');
    Ok(text)
}

pub(super) fn apply_generated_openapi_values(
    text: &str,
    operation: &SpecOperation,
) -> Result<String, String> {
    let mut document = forge_core::reqv1::RequestDocument::parse(text)
        .map_err(|error| format!("fix the request JSON before completion: {error}"))?;
    for name in &operation.path_params {
        document.bindings.insert(
            name.clone(),
            Binding::Value(ValueBinding {
                value: "sample".into(),
            }),
        );
    }
    for (name, required) in &operation.query_params {
        if *required {
            if let Some(parameter) = document
                .request
                .query
                .iter_mut()
                .find(|parameter| parameter.name.eq_ignore_ascii_case(name))
            {
                parameter.value = "sample".to_string();
            }
        }
    }
    for (name, required) in &operation.header_params {
        if *required {
            if let Some(header) = document
                .request
                .headers
                .iter_mut()
                .find(|header| header.name.eq_ignore_ascii_case(name))
            {
                header.value = "sample".to_string();
            }
        }
    }
    if let Some(value) = operation.request_example.clone().or_else(|| {
        operation
            .request_schema
            .as_ref()
            .map(|schema| forge_core::openapi::example_from_schema(schema, 0))
    }) {
        document.request.body = Some(BodySpec::Inline(InlineBody {
            body_type: BodyType::Json,
            value: Some(value),
        }));
    }
    serialize_request(&document)
}

pub(super) fn marked_operations_path(root: &Path) -> PathBuf {
    root.join(".forge-local/openapi-covered.json")
}

pub(super) fn load_marked_operations(root: &Path) -> Result<BTreeSet<String>, String> {
    let path = marked_operations_path(root);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("cannot parse {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

pub(super) fn save_marked_operations(
    root: &Path,
    operations: &BTreeSet<String>,
) -> Result<(), String> {
    let path = marked_operations_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(operations).map_err(|error| error.to_string())?;
    std::fs::write(&path, format!("{text}\n"))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

pub(super) fn request_url_prefix(url: &str) -> &str {
    if let Some(rest) = url.strip_prefix("${") {
        return rest
            .find('}')
            .map(|end| &url[..end + 3])
            .unwrap_or_default();
    }
    if let Some(rest) = url.strip_prefix("{{") {
        return rest
            .find("}}")
            .map(|end| &url[..end + 4])
            .unwrap_or_default();
    }
    if let Some(scheme) = url.find("://") {
        let host = scheme + 3;
        return url[host..]
            .find('/')
            .map(|slash| &url[..host + slash])
            .unwrap_or(url);
    }
    ""
}

pub(super) fn discover_openapi(
    root: &std::path::Path,
) -> (Option<PathBuf>, Option<ParsedSpec>, Option<String>) {
    let mut candidates = [
        "openapi.json",
        "openapi.yaml",
        "openapi.yml",
        "swagger.json",
        "swagger.yaml",
        "swagger.yml",
    ]
    .into_iter()
    .map(|name| root.join(name))
    .filter(|path| path.is_file())
    .collect::<Vec<_>>();

    let specs = root.join("specs");
    let mut pending = specs
        .is_dir()
        .then_some(specs)
        .into_iter()
        .collect::<Vec<_>>();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "json" | "yaml" | "yml"
                    )
                })
            {
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    candidates.dedup();

    let mut error = None;
    for path in candidates {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|cause| cause.to_string())
            .and_then(|text| {
                forge_core::openapi::parse_spec(&text).map_err(|cause| cause.to_string())
            });
        match parsed {
            Ok(spec) => return (Some(path), Some(spec), None),
            Err(cause) if error.is_none() => {
                error = Some((path, cause));
            }
            Err(_) => {}
        }
    }
    match error {
        Some((path, cause)) => (
            Some(path.clone()),
            None,
            Some(format!("invalid OpenAPI spec {}: {cause}", path.display())),
        ),
        None => (None, None, None),
    }
}
