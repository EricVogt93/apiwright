//! Reusable asset catalog and parameter forms.

use super::*;

pub(super) fn palette(
    ui: &mut egui::Ui,
    d: &mut V1EditorState,
    bridge: &Bridge,
    insert: &mut Option<PendingInsert>,
) {
    let assets = d
        .index
        .as_ref()
        .map(|index| index.assets.clone())
        .unwrap_or_default();
    catalog_filters(ui, d);
    let query = d.catalog_query.trim().to_ascii_lowercase();
    let intent_filter = d.catalog_intent.clone();
    let intent = intent_filter.as_deref();
    let has_selection = match d.catalog_view {
        CatalogView::Builtins => d.selected_builtin.is_some(),
        CatalogView::Project => d.selected_project.is_some(),
    };
    ui.add_space(6.0);
    ui.separator();
    let list_height = if has_selection {
        (ui.available_height() * 0.4).clamp(170.0, 320.0)
    } else {
        (ui.available_height() - 12.0).max(180.0)
    };
    egui::ScrollArea::vertical()
        .id_salt("catalog-results")
        .max_height(list_height)
        .auto_shrink([false, false])
        .show(ui, |ui| match d.catalog_view {
            CatalogView::Builtins => builtin_list(ui, d, &query, intent),
            CatalogView::Project => project_asset_list(ui, d, &assets, &query, intent, insert),
        });

    if !has_selection {
        return;
    }
    ui.add_space(6.0);
    ui.separator();
    ui.label(RichText::new("CONFIGURE").small().strong().weak());
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .id_salt("catalog-detail")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| match d.catalog_view {
                    CatalogView::Builtins => {
                        let definition = d
                            .selected_builtin
                            .as_deref()
                            .and_then(find_builtin)
                            .copied();
                        if let Some(definition) = definition {
                            builtin_form(ui, d, bridge, insert, definition);
                        } else {
                            ui.weak("Select a built-in to configure it.");
                        }
                    }
                    CatalogView::Project => {
                        let asset = d.selected_project.as_ref().and_then(|selected| {
                            assets.iter().find(|asset| &asset.rel_path == selected)
                        });
                        if let Some(asset) = asset {
                            project_asset_form(ui, d, bridge, asset, insert);
                        } else {
                            ui.weak("Select a project asset to configure it.");
                        }
                    }
                });
        });
}

pub(super) fn catalog_filters(ui: &mut egui::Ui, d: &mut V1EditorState) {
    ui.add_sized(
        [ui.available_width(), 32.0],
        TextEdit::singleline(&mut d.catalog_query)
            .hint_text(format!("{}  Search catalog", icons::SEARCH)),
    )
    .on_hover_text("Search reusable behavior by title, intent or description");
    ui.add_space(6.0);
    let width = ((ui.available_width() - 8.0) / 2.0).max(100.0);
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("catalog-source")
            .width(width)
            .selected_text(match d.catalog_view {
                CatalogView::Builtins => "Built-ins",
                CatalogView::Project => "Project assets",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut d.catalog_view, CatalogView::Builtins, "Built-ins");
                ui.selectable_value(&mut d.catalog_view, CatalogView::Project, "Project assets");
            })
            .response
            .on_hover_text("Catalog source");
        egui::ComboBox::from_id_salt("catalog-intent")
            .width(width)
            .selected_text(d.catalog_intent.as_deref().unwrap_or("All intents"))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut d.catalog_intent, None, "All intents");
                for intent in CATALOG_INTENTS {
                    let label = intent_label(intent);
                    ui.selectable_value(&mut d.catalog_intent, Some(label.to_string()), label);
                }
            })
            .response
            .on_hover_text("Filter by intent");
    });
}

pub(super) const CATALOG_INTENTS: [BuiltinIntent; 5] = [
    BuiltinIntent::Validate,
    BuiltinIntent::Prepare,
    BuiltinIntent::Capture,
    BuiltinIntent::Generate,
    BuiltinIntent::Simulate,
];

pub(super) fn builtin_matches(
    definition: &BuiltinDefinition,
    query: &str,
    intent: Option<&str>,
) -> bool {
    intent.is_none_or(|intent| intent == intent_label(definition.intent))
        && (query.is_empty()
            || definition.title.to_ascii_lowercase().contains(query)
            || definition.description.to_ascii_lowercase().contains(query)
            || definition.name.to_ascii_lowercase().contains(query)
            || intent_label(definition.intent)
                .to_ascii_lowercase()
                .contains(query))
}

pub(super) fn project_asset_matches(asset: &AssetEntry, query: &str, intent: Option<&str>) -> bool {
    let asset_intent = asset
        .metadata
        .as_ref()
        .map(|metadata| intent_label(metadata.intent))
        .or_else(|| asset_intent(asset.kind));
    intent.is_none_or(|filter| asset_intent == Some(filter))
        && (query.is_empty()
            || asset.rel_path.to_ascii_lowercase().contains(query)
            || asset
                .alias
                .as_deref()
                .is_some_and(|alias| alias.to_ascii_lowercase().contains(query))
            || asset.metadata.as_ref().is_some_and(|metadata| {
                metadata.title.to_ascii_lowercase().contains(query)
                    || metadata.description.to_ascii_lowercase().contains(query)
            }))
}

pub(super) fn catalog_section_header(ui: &mut egui::Ui, label: &str, count: usize) {
    ui.add_space(5.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(label.to_uppercase()).small().strong().weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(count.to_string()).small().weak());
        });
    });
}

pub(super) fn builtin_list(
    ui: &mut egui::Ui,
    d: &mut V1EditorState,
    query: &str,
    intent: Option<&str>,
) {
    let mut found = false;
    for group in CATALOG_INTENTS {
        let count = builtin_catalog()
            .iter()
            .filter(|definition| {
                definition.intent == group && builtin_matches(definition, query, intent)
            })
            .count();
        if count == 0 {
            continue;
        }
        found = true;
        catalog_section_header(ui, intent_label(group), count);
        for definition in builtin_catalog().iter().filter(|definition| {
            definition.intent == group && builtin_matches(definition, query, intent)
        }) {
            let selected = d.selected_builtin.as_deref() == Some(definition.name);
            let row_width = ui.available_width();
            let show_target = row_width >= 260.0;
            let title_width = if show_target {
                (row_width - 106.0).max(100.0)
            } else {
                row_width
            };
            let response = ui
                .horizontal(|ui| {
                    let response = ui
                        .allocate_ui_with_layout(
                            egui::vec2(title_width, ui.spacing().interact_size.y),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| ui.selectable_label(selected, definition.title),
                        )
                        .inner;
                    if show_target {
                        ui.label(
                            RichText::new(target_label(definition.target))
                                .small()
                                .weak(),
                        );
                    }
                    response
                })
                .inner
                .on_hover_text(definition.description);
            if response.clicked() {
                select_builtin(d, definition);
            }
        }
    }
    if !found {
        ui.weak("No matching built-ins.");
    }
}

pub(super) fn project_asset_group(asset: &AssetEntry) -> &'static str {
    asset
        .metadata
        .as_ref()
        .map(|metadata| intent_label(metadata.intent))
        .or_else(|| asset_intent(asset.kind))
        .unwrap_or(match asset.kind {
            AssetKind::Data => "Data",
            _ => "Other",
        })
}

pub(super) fn project_asset_list(
    ui: &mut egui::Ui,
    d: &mut V1EditorState,
    assets: &[AssetEntry],
    query: &str,
    intent: Option<&str>,
    insert: &mut Option<PendingInsert>,
) {
    const GROUPS: [&str; 7] = [
        "Validate", "Prepare", "Capture", "Generate", "Simulate", "Data", "Other",
    ];
    let selected = d.selected_project.clone();
    let mut select_project = None;
    let mut found = false;
    for group in GROUPS {
        let count = assets
            .iter()
            .filter(|asset| {
                project_asset_group(asset) == group && project_asset_matches(asset, query, intent)
            })
            .count();
        if count == 0 {
            continue;
        }
        found = true;
        catalog_section_header(ui, group, count);
        for asset in assets.iter().filter(|asset| {
            project_asset_group(asset) == group && project_asset_matches(asset, query, intent)
        }) {
            palette_row(
                ui,
                asset,
                selected.as_deref() == Some(&asset.rel_path),
                &mut d.expanded,
                insert,
                &mut select_project,
            );
        }
    }
    if !found {
        ui.weak("No matching project assets.");
    }
    if let Some(rel_path) = select_project {
        if let Some(asset) = assets.iter().find(|asset| asset.rel_path == rel_path) {
            select_project_asset(d, asset);
        }
    }
}

pub(super) fn builtin_form(
    ui: &mut egui::Ui,
    d: &mut V1EditorState,
    bridge: &Bridge,
    insert: &mut Option<PendingInsert>,
    definition: BuiltinDefinition,
) {
    let heading = ui.label(RichText::new(definition.title).strong());
    if std::mem::take(&mut d.scroll_to_catalog_form) {
        heading.scroll_to_me(Some(egui::Align::TOP));
    }
    ui.weak(format!(
        "{} · {}",
        intent_label(definition.intent),
        target_label(definition.target)
    ));
    ui.weak(definition.description);
    let mut form_changed = false;
    for parameter in definition.parameters {
        form_changed |= parameter_form(ui, d, &ParameterDefinition::builtin(parameter));
    }
    if form_changed {
        d.clear_preview();
    }
    if let Some(error) = &d.catalog_error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    if let Some(notice) = &d.catalog_notice {
        ui.weak(notice);
    }

    let preview_phase = match definition.target {
        BuiltinTarget::Pipeline(phase) => Some(phase),
        BuiltinTarget::Binding => None,
    };
    let has_preview_context = preview_phase.is_some_and(|phase| {
        phase == forge_core::reqv1::model::PipelinePhase::BeforeRequest || d.last_response.is_some()
    });
    let preview_sources_supported = preview_supports_sources(&d.catalog_inputs);
    ui.horizontal(|ui| {
        let action = if d.editing_assertion.is_some() {
            "Save assertion"
        } else if d.editing_hook.is_some() {
            "Save hook"
        } else {
            "Insert configured reference"
        };
        if ui.button(action).clicked() {
            match builtin_snippet(&definition, &d.catalog_inputs) {
                Ok(snippet) => {
                    d.catalog_error = None;
                    *insert = Some(PendingInsert {
                        target: match definition.target {
                            BuiltinTarget::Binding => InsertTarget::Binding,
                            BuiltinTarget::Pipeline(_)
                                if definition.intent == BuiltinIntent::Validate =>
                            {
                                InsertTarget::Assertion
                            }
                            BuiltinTarget::Pipeline(_) => InsertTarget::Pipeline,
                        },
                        suggested_name: definition.name.to_string(),
                        snippet,
                    });
                }
                Err(error) => d.catalog_error = Some(error),
            }
        }
        if ui
            .add_enabled(
                has_preview_context && preview_sources_supported && !d.preview_in_flight,
                egui::Button::new("Preview"),
            )
            .on_hover_text("Evaluate locally; never sends an HTTP request")
            .clicked()
        {
            preview_now(d, bridge, &definition);
        }
        if d.preview_in_flight {
            ui.spinner();
        }
    });
    if preview_phase.is_none() {
        ui.weak("Binding generators have no request/response preview.");
    } else if !preview_sources_supported {
        ui.weak("Preview cannot resolve these inputs; use a matrix run or runtime sequence.");
    } else if !has_preview_context {
        ui.weak("Run the request once to preview an afterResponse asset.");
    }
    preview_pane(ui, d);
    ui.collapsing("Example", |ui| {
        ui.monospace(definition.example);
    });
}

pub(super) fn preview_supports_sources(inputs: &BTreeMap<String, ParameterInput>) -> bool {
    inputs.values().all(|input| {
        !matches!(
            input.source,
            ParameterSource::Matrix | ParameterSource::Runtime
        )
    })
}

pub(super) fn intent_label(intent: BuiltinIntent) -> &'static str {
    match intent {
        BuiltinIntent::Validate => "Validate",
        BuiltinIntent::Prepare => "Prepare",
        BuiltinIntent::Capture => "Capture",
        BuiltinIntent::Generate => "Generate",
        BuiltinIntent::Simulate => "Simulate",
    }
}

pub(super) fn target_label(target: BuiltinTarget) -> &'static str {
    match target {
        BuiltinTarget::Binding => "Binding",
        BuiltinTarget::Pipeline(forge_core::reqv1::model::PipelinePhase::BeforeRequest) => {
            "beforeRequest"
        }
        BuiltinTarget::Pipeline(forge_core::reqv1::model::PipelinePhase::AfterResponse) => {
            "afterResponse"
        }
        BuiltinTarget::Pipeline(forge_core::reqv1::model::PipelinePhase::OnError) => "onError",
        BuiltinTarget::Pipeline(forge_core::reqv1::model::PipelinePhase::Finally) => "finally",
    }
}

pub(super) fn asset_intent(kind: AssetKind) -> Option<&'static str> {
    match kind {
        AssetKind::Assertion => Some("Validate"),
        AssetKind::Hook => Some("Prepare"),
        AssetKind::Extractor => Some("Capture"),
        AssetKind::Generator => Some("Generate"),
        AssetKind::Mock => Some("Simulate"),
        AssetKind::Data | AssetKind::Executable => None,
    }
}

pub(super) fn select_builtin(d: &mut V1EditorState, definition: &BuiltinDefinition) {
    d.catalog_view = CatalogView::Builtins;
    d.selected_builtin = Some(definition.name.to_string());
    d.selected_project = None;
    d.editing_assertion = None;
    d.editing_hook = None;
    d.scroll_to_catalog_form = true;
    d.catalog_error = None;
    d.catalog_notice = None;
    d.clear_preview();
    d.catalog_inputs.clear();
    let example = serde_json::from_str::<serde_json::Value>(definition.example)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    for parameter in definition.parameters {
        let parameter = ParameterDefinition::builtin(parameter);
        let value = parameter
            .default
            .clone()
            .or_else(|| {
                parameter
                    .required
                    .then(|| example.get(&parameter.name).cloned())
                    .flatten()
            })
            .map(|value| display_parameter_value(parameter.kind, &value))
            .unwrap_or_default();
        d.catalog_inputs.insert(
            parameter.name,
            ParameterInput {
                source: ParameterSource::Literal,
                value,
            },
        );
    }
}

pub(super) fn parameter_form(
    ui: &mut egui::Ui,
    d: &mut V1EditorState,
    parameter: &ParameterDefinition,
) -> bool {
    let mut changed = false;
    ui.add_space(4.0);
    ui.label(if parameter.required {
        RichText::new(format!("{} *", parameter.label))
    } else {
        RichText::new(&parameter.label)
    });

    let current_source = d
        .catalog_inputs
        .get(&parameter.name)
        .map(|input| input.source)
        .unwrap_or_default();
    let suggestions = parameter_suggestions(d, current_source);
    let input = d.catalog_inputs.entry(parameter.name.clone()).or_default();

    let previous_source = input.source;
    let previous_value = input.value.clone();
    let source_response =
        egui::ComboBox::from_id_salt(("catalog-source", definition_id(parameter)))
            .selected_text(input.source.label())
            .show_ui(ui, |ui| {
                for &source in parameter_sources(parameter.kind) {
                    ui.selectable_value(&mut input.source, source, source.label());
                }
            });
    changed |= source_response.response.changed();
    if input.source != previous_source {
        input.value.clear();
        changed = true;
    }

    if input.source == ParameterSource::Literal && !parameter.options.is_empty() {
        let option_response =
            egui::ComboBox::from_id_salt(("catalog-option", definition_id(parameter)))
                .selected_text(if input.value.is_empty() {
                    "(select)"
                } else {
                    &input.value
                })
                .show_ui(ui, |ui| {
                    for option in &parameter.options {
                        ui.selectable_value(&mut input.value, option.clone(), option);
                    }
                });
        changed |= option_response.response.changed();
    } else {
        changed |= ui
            .add(TextEdit::singleline(&mut input.value).hint_text(
                if input.source == ParameterSource::Literal {
                    parameter.example.as_str()
                } else {
                    "namespace path"
                },
            ))
            .changed();
        if !suggestions.is_empty() {
            egui::ComboBox::from_id_salt(("catalog-suggestion", definition_id(parameter)))
                .selected_text("Suggestions")
                .show_ui(ui, |ui| {
                    for suggestion in &suggestions {
                        if ui.selectable_label(false, suggestion).clicked() {
                            input.value.clone_from(suggestion);
                            changed = true;
                            ui.close();
                        }
                    }
                });
        }
    }
    if input.source == ParameterSource::Runtime {
        ui.weak("Last run output; available only to a later sequence step.");
    }
    changed || input.value != previous_value
}

pub(super) fn definition_id(parameter: &ParameterDefinition) -> &str {
    &parameter.name
}

pub(super) fn parameter_suggestions(d: &V1EditorState, source: ParameterSource) -> Vec<String> {
    let document = forge_core::reqv1::RequestDocument::parse(&d.text).ok();
    let mut suggestions = Vec::new();
    match source {
        ParameterSource::Binding => {
            if let Some(document) = &document {
                binding_paths(&document.bindings, &mut suggestions);
            }
        }
        ParameterSource::Matrix => {
            if let Some(document) = &document {
                binding_paths(&document.matrix, &mut suggestions);
            }
        }
        ParameterSource::Environment => {
            if let (Some(root), Some(file)) = (&d.root, &d.file) {
                if let Ok(environment) =
                    forge_core::reqv1::load_request_environment(root, file, d.env_name.as_deref())
                {
                    collect_value_paths(&environment, "", &mut suggestions);
                }
            }
        }
        ParameterSource::Runtime => {
            if let Some(result) = selected_result(d) {
                for (name, value) in &result.runtime {
                    suggestions.push(name.clone());
                    collect_value_paths(value, name, &mut suggestions);
                }
            }
        }
        ParameterSource::Secret => {
            if let Some(root) = &d.root {
                suggestions.extend(forge_core::reqv1::load_file_secrets(root).into_keys());
            }
        }
        ParameterSource::Literal => {}
    }
    suggestions.sort();
    suggestions.dedup();
    suggestions
}

pub(super) fn binding_paths(
    bindings: &BTreeMap<String, forge_core::reqv1::Binding>,
    paths: &mut Vec<String>,
) {
    for (name, binding) in bindings {
        paths.push(name.clone());
        if let forge_core::reqv1::Binding::Value(value) = binding {
            collect_value_paths(&value.value, name, paths);
        }
    }
}

pub(super) fn collect_value_paths(
    value: &serde_json::Value,
    prefix: &str,
    paths: &mut Vec<String>,
) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                paths.push(path.clone());
                collect_value_paths(value, &path, paths);
            }
        }
        serde_json::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                let path = format!("{prefix}.{index}");
                paths.push(path.clone());
                collect_value_paths(value, &path, paths);
            }
        }
        _ => {}
    }
}

pub(super) fn builtin_snippet(
    definition: &BuiltinDefinition,
    inputs: &BTreeMap<String, ParameterInput>,
) -> Result<String, String> {
    let with = builtin_with(definition, inputs)?;
    let reference = definition.reference;
    let with = serde_json::Value::Object(with);
    let snippet = match definition.target {
        BuiltinTarget::Binding => serde_json::json!({ "use": reference, "with": with }),
        BuiltinTarget::Pipeline(phase) => serde_json::json!({
            "phase": phase,
            "use": reference,
            "with": with,
        }),
    };
    serde_json::to_string_pretty(&snippet).map_err(|error| error.to_string())
}

pub(super) fn builtin_with(
    definition: &BuiltinDefinition,
    inputs: &BTreeMap<String, ParameterInput>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let parameters = definition
        .parameters
        .iter()
        .map(ParameterDefinition::builtin)
        .collect::<Vec<_>>();
    configured_with(&parameters, inputs)
}

pub(super) fn configured_with(
    parameters: &[ParameterDefinition],
    inputs: &BTreeMap<String, ParameterInput>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let mut with = serde_json::Map::new();
    for parameter in parameters {
        let input = inputs.get(&parameter.name).cloned().unwrap_or_default();
        let value = if input.source == ParameterSource::Literal {
            literal_value(parameter, &input.value)?
        } else {
            sourced_value(input.source, &input.value)
        };
        if let Some(value) = value {
            with.insert(parameter.name.clone(), value);
        } else if parameter.required {
            return Err(format!("{} is required", parameter.label));
        }
    }
    Ok(with)
}

pub(super) fn preview_now(d: &mut V1EditorState, bridge: &Bridge, definition: &BuiltinDefinition) {
    let BuiltinTarget::Pipeline(phase) = definition.target else {
        return;
    };
    if !preview_supports_sources(&d.catalog_inputs) {
        d.preview_error = Some(
            "Preview cannot resolve these inputs; use a matrix run or runtime sequence.".into(),
        );
        return;
    }
    let Some(root) = d.root.clone() else {
        d.preview_error = Some("No project root.".to_string());
        return;
    };
    let with = match builtin_with(definition, &d.catalog_inputs) {
        Ok(with) => with,
        Err(error) => {
            d.preview_error = Some(error);
            return;
        }
    };
    let file = d
        .file
        .clone()
        .unwrap_or_else(|| root.join("__unsaved__.request.json"));
    let preview_id = d.next_preview_id;
    d.next_preview_id += 1;
    d.active_preview = Some(preview_id);
    d.preview_in_flight = true;
    d.preview = None;
    d.preview_error = None;
    if let Err(error) = bridge.send(Cmd::PreviewV1Asset {
        preview_id,
        root,
        file,
        text: d.text.clone(),
        env_name: d.env_name.clone(),
        phase,
        uses: definition.reference.to_string(),
        with,
        response: d.last_response.clone(),
        allow_project_code: d.allow_project_code,
    }) {
        d.active_preview = None;
        d.preview_in_flight = false;
        d.preview_error = Some(error);
    }
}

pub(super) fn preview_pane(ui: &mut egui::Ui, d: &V1EditorState) {
    if let Some(error) = &d.preview_error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    let Some(preview) = &d.preview else {
        return;
    };

    ui.group(|ui| {
        ui.label(RichText::new("Preview").strong());
        if let (Some(before), Some(after)) = (&preview.request_before, &preview.request_after) {
            request_diff(ui, before, after);
        }
        for assertion in &preview.assertions {
            let (mark, color) = if assertion.passed {
                ("✓", egui::Color32::from_rgb(0x49, 0x9C, 0x54))
            } else {
                ("✗", ui.visuals().error_fg_color)
            };
            ui.colored_label(color, format!("{mark} {}", assertion.message));
            if !assertion.passed {
                if let Some(expected) = &assertion.expected {
                    ui.weak(format!("expected: {expected}"));
                }
                if let Some(actual) = &assertion.actual {
                    ui.weak(format!("actual: {actual}"));
                }
            }
        }
        if !preview.runtime_writes.is_empty() {
            ui.label(RichText::new("Runtime writes").strong());
            for (name, value) in &preview.runtime_writes {
                ui.monospace(format!("{name} = {value}"));
            }
        }
        if !preview.logs.is_empty() {
            ui.label(RichText::new("Logs (secrets masked)").strong());
            for line in &preview.logs {
                ui.monospace(line);
            }
        }
        for diagnostic in &preview.diagnostics {
            let color = if diagnostic.severity == forge_core::reqv1::Severity::Error {
                ui.visuals().error_fg_color
            } else {
                ui.visuals().warn_fg_color
            };
            ui.colored_label(
                color,
                format!("[{}] {}", diagnostic.code, diagnostic.message),
            );
        }
        if preview.request_before.is_none()
            && preview.assertions.is_empty()
            && preview.runtime_writes.is_empty()
            && preview.logs.is_empty()
            && preview.diagnostics.is_empty()
        {
            ui.weak("No observable changes.");
        }
    });
}

pub(super) fn request_diff(
    ui: &mut egui::Ui,
    before: &forge_core::reqv1::runner::CatalogRequestView,
    after: &forge_core::reqv1::runner::CatalogRequestView,
) {
    let mut changed = false;
    if before.url != after.url {
        changed = true;
        ui.label(RichText::new("URL").strong());
        ui.weak(format!("- {}", before.url));
        ui.label(format!("+ {}", after.url));
    }

    // ponytail: duplicate request headers collapse by case-insensitive name;
    // use a multiset diff if repeated header editing becomes a real use case.
    let header_map = |headers: &[(String, String)]| {
        headers
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), (name.clone(), value.clone())))
            .collect::<BTreeMap<_, _>>()
    };
    let before_headers = header_map(&before.headers);
    let after_headers = header_map(&after.headers);
    let header_names = before_headers
        .keys()
        .chain(after_headers.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in header_names {
        let before_header = before_headers.get(&key);
        let after_header = after_headers.get(&key);
        if before_header == after_header {
            continue;
        }
        changed = true;
        match (before_header, after_header) {
            (Some((name, value)), None) => {
                ui.weak(format!("- {name}: {value}"));
            }
            (None, Some((name, value))) => {
                ui.label(format!("+ {name}: {value}"));
            }
            (Some((name, before_value)), Some((_, after_value))) => {
                ui.label(format!("~ {name}: {before_value} → {after_value}"));
            }
            (None, None) => {}
        }
    }
    if !changed {
        ui.weak("No request changes.");
    }
}

pub(super) fn literal_value(
    parameter: &ParameterDefinition,
    input: &str,
) -> Result<Option<serde_json::Value>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    if !parameter.options.is_empty() && !parameter.options.iter().any(|option| option == input) {
        return Err(format!(
            "{} must be one of {}",
            parameter.label,
            parameter.options.join(", ")
        ));
    }
    let value = match parameter.kind {
        BuiltinParameterKind::String => serde_json::Value::String(input.to_string()),
        BuiltinParameterKind::Integer => serde_json::Value::Number(
            input
                .parse::<u64>()
                .map_err(|_| format!("{} must be a non-negative integer", parameter.label))?
                .into(),
        ),
        BuiltinParameterKind::Boolean => serde_json::Value::Bool(
            input
                .parse::<bool>()
                .map_err(|_| format!("{} must be true or false", parameter.label))?,
        ),
        BuiltinParameterKind::Json => serde_json::from_str(input)
            .map_err(|error| format!("{} is invalid JSON: {error}", parameter.label))?,
    };
    Ok(Some(value))
}

pub(super) fn display_parameter_value(
    kind: BuiltinParameterKind,
    value: &serde_json::Value,
) -> String {
    match kind {
        BuiltinParameterKind::String => value.as_str().unwrap_or_default().to_string(),
        _ => value.to_string(),
    }
}

pub(super) fn parameter_input_from_value(
    kind: BuiltinParameterKind,
    value: &serde_json::Value,
) -> ParameterInput {
    if let Some((namespace, path)) = value
        .as_str()
        .and_then(|value| value.strip_prefix("${"))
        .and_then(|value| value.strip_suffix('}'))
        .and_then(|value| value.split_once('.'))
    {
        let source = match namespace {
            "bindings" => Some(ParameterSource::Binding),
            "env" => Some(ParameterSource::Environment),
            "runtime" => Some(ParameterSource::Runtime),
            "matrix" => Some(ParameterSource::Matrix),
            "secret" => Some(ParameterSource::Secret),
            _ => None,
        };
        if let Some(source) = source.filter(|_| !path.is_empty()) {
            return ParameterInput {
                source,
                value: path.to_string(),
            };
        }
    }
    ParameterInput {
        source: ParameterSource::Literal,
        value: display_parameter_value(kind, value),
    }
}

pub(super) fn select_project_asset(d: &mut V1EditorState, asset: &AssetEntry) {
    let Some(metadata) = &asset.metadata else {
        return;
    };
    d.catalog_view = CatalogView::Project;
    d.selected_builtin = None;
    d.selected_project = Some(asset.rel_path.clone());
    d.editing_assertion = None;
    d.editing_hook = None;
    d.scroll_to_catalog_form = true;
    d.catalog_inputs.clear();
    d.catalog_error = None;
    d.catalog_notice = None;
    d.clear_preview();
    let example = metadata.example.as_object().cloned().unwrap_or_default();
    for parameter in &metadata.parameters {
        let parameter = ParameterDefinition::project(parameter);
        let value = parameter
            .default
            .clone()
            .or_else(|| {
                parameter
                    .required
                    .then(|| example.get(&parameter.name).cloned())
                    .flatten()
            })
            .map(|value| display_parameter_value(parameter.kind, &value))
            .unwrap_or_default();
        d.catalog_inputs.insert(
            parameter.name,
            ParameterInput {
                source: ParameterSource::Literal,
                value,
            },
        );
    }
}

pub(super) fn project_asset_form(
    ui: &mut egui::Ui,
    d: &mut V1EditorState,
    bridge: &Bridge,
    asset: &AssetEntry,
    insert: &mut Option<PendingInsert>,
) {
    let Some(metadata) = &asset.metadata else {
        return;
    };
    let base_ref = asset
        .alias
        .clone()
        .or_else(|| asset.prefix_ref.clone())
        .unwrap_or_else(|| asset.rel_path.clone());
    let heading = ui.label(RichText::new(&metadata.title).strong());
    if std::mem::take(&mut d.scroll_to_catalog_form) {
        heading.scroll_to_me(Some(egui::Align::TOP));
    }
    ui.weak(format!(
        "{} · {}",
        intent_label(metadata.intent),
        project_target_label(asset, metadata)
    ));
    if !metadata.description.is_empty() {
        ui.weak(&metadata.description);
    }
    let parameters = metadata
        .parameters
        .iter()
        .map(ParameterDefinition::project)
        .collect::<Vec<_>>();
    let mut changed = false;
    for parameter in &parameters {
        changed |= parameter_form(ui, d, parameter);
    }
    if changed {
        d.clear_preview();
    }
    if let Some(error) = &d.catalog_error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    if let Some(notice) = &d.catalog_notice {
        ui.weak(notice);
    }
    let phase = project_phase(asset, metadata);
    ui.horizontal(|ui| {
        let action = if d.editing_assertion.is_some() {
            "Save assertion"
        } else if d.editing_hook.is_some() {
            "Save hook"
        } else {
            "Insert configured reference"
        };
        if ui.button(action).clicked() {
            match project_snippet(asset, &base_ref, metadata, &parameters, &d.catalog_inputs) {
                Ok(snippet) => {
                    d.catalog_error = None;
                    *insert = Some(PendingInsert {
                        target: match asset.kind {
                            AssetKind::Generator => InsertTarget::Binding,
                            AssetKind::Assertion => InsertTarget::Assertion,
                            AssetKind::Mock => InsertTarget::Mock,
                            _ => InsertTarget::Pipeline,
                        },
                        suggested_name: suggested_name(&base_ref),
                        snippet,
                    });
                }
                Err(error) => d.catalog_error = Some(error),
            }
        }
        let can_preview = phase.is_some_and(|phase| {
            phase == forge_core::reqv1::model::PipelinePhase::BeforeRequest
                || d.last_response.is_some()
        }) && preview_supports_sources(&d.catalog_inputs)
            && !d.preview_in_flight;
        if ui
            .add_enabled(can_preview, egui::Button::new("Preview"))
            .clicked()
        {
            match configured_with(&parameters, &d.catalog_inputs) {
                Ok(with) => preview_project_now(d, bridge, phase.unwrap(), base_ref.clone(), with),
                Err(error) => d.catalog_error = Some(error),
            }
        }
        if d.preview_in_flight {
            ui.spinner();
        }
    });
    preview_pane(ui, d);
    if !metadata.example.is_null() {
        ui.collapsing("Example", |ui| {
            ui.monospace(metadata.example.to_string());
        });
    }
}

pub(super) fn project_phase(
    asset: &AssetEntry,
    metadata: &ProjectAssetMetadata,
) -> Option<forge_core::reqv1::model::PipelinePhase> {
    metadata.phase.or(match asset.kind {
        AssetKind::Hook => Some(forge_core::reqv1::model::PipelinePhase::BeforeRequest),
        AssetKind::Assertion | AssetKind::Extractor => {
            Some(forge_core::reqv1::model::PipelinePhase::AfterResponse)
        }
        _ => None,
    })
}

pub(super) fn project_target_label(
    asset: &AssetEntry,
    metadata: &ProjectAssetMetadata,
) -> &'static str {
    match asset.kind {
        AssetKind::Generator => "Binding",
        AssetKind::Mock => "Mock",
        _ => project_phase(asset, metadata)
            .map(|phase| target_label(BuiltinTarget::Pipeline(phase)))
            .unwrap_or("Executable"),
    }
}

pub(super) fn project_snippet(
    asset: &AssetEntry,
    reference: &str,
    metadata: &ProjectAssetMetadata,
    parameters: &[ParameterDefinition],
    inputs: &BTreeMap<String, ParameterInput>,
) -> Result<String, String> {
    let with = serde_json::Value::Object(configured_with(parameters, inputs)?);
    let snippet = match asset.kind {
        AssetKind::Generator | AssetKind::Mock => {
            serde_json::json!({"use": reference, "with": with})
        }
        _ => {
            let phase = project_phase(asset, metadata)
                .ok_or_else(|| "asset metadata needs a pipeline phase".to_string())?;
            serde_json::json!({"phase": phase, "use": reference, "with": with})
        }
    };
    serde_json::to_string_pretty(&snippet).map_err(|error| error.to_string())
}

pub(super) fn preview_project_now(
    d: &mut V1EditorState,
    bridge: &Bridge,
    phase: forge_core::reqv1::model::PipelinePhase,
    uses: String,
    with: serde_json::Map<String, serde_json::Value>,
) {
    let Some(root) = d.root.clone() else {
        return;
    };
    let file = d
        .file
        .clone()
        .unwrap_or_else(|| root.join("__unsaved__.request.json"));
    let preview_id = d.next_preview_id;
    d.next_preview_id += 1;
    d.active_preview = Some(preview_id);
    d.preview_in_flight = true;
    d.preview = None;
    d.preview_error = None;
    if let Err(error) = bridge.send(Cmd::PreviewV1Asset {
        preview_id,
        root,
        file,
        text: d.text.clone(),
        env_name: d.env_name.clone(),
        phase,
        uses,
        with,
        response: d.last_response.clone(),
        allow_project_code: d.allow_project_code,
    }) {
        d.active_preview = None;
        d.preview_in_flight = false;
        d.preview_error = Some(error);
    }
}

pub(super) fn palette_row(
    ui: &mut egui::Ui,
    asset: &AssetEntry,
    selected: bool,
    expanded: &mut std::collections::HashSet<String>,
    insert: &mut Option<PendingInsert>,
    select_project: &mut Option<String>,
) {
    let base_ref = asset
        .alias
        .clone()
        .or_else(|| asset.prefix_ref.clone())
        .unwrap_or_else(|| asset.rel_path.clone());
    let browsable = asset.kind == AssetKind::Data && asset.data.is_some();

    ui.horizontal(|ui| {
        if browsable {
            let open = expanded.contains(&asset.rel_path);
            if ui.small_button(if open { "▾" } else { "▸" }).clicked() {
                if open {
                    expanded.remove(&asset.rel_path);
                } else {
                    expanded.insert(asset.rel_path.clone());
                }
            }
        } else {
            ui.add_space(16.0);
        }
        let name = asset
            .metadata
            .as_ref()
            .map(|metadata| metadata.title.as_str())
            .unwrap_or_else(|| asset.rel_path.rsplit('/').next().unwrap_or(&asset.rel_path));
        if ui
            .selectable_label(selected, name)
            .on_hover_text(&base_ref)
            .clicked()
            && asset.metadata.is_some()
        {
            *select_project = Some(asset.rel_path.clone());
        }
        let can_insert = asset.kind != AssetKind::Executable;
        if ui
            .add_enabled(can_insert, egui::Button::new("Use").small())
            .on_hover_text(if can_insert {
                "add to the request document"
            } else {
                "add metadata with a pipeline phase before inserting"
            })
            .clicked()
        {
            if let Ok(pending) = insert_for_asset(asset, &base_ref) {
                *insert = Some(pending);
            }
        }
    });

    if browsable && expanded.contains(&asset.rel_path) {
        if let Some(data) = &asset.data {
            ui.indent(&asset.rel_path, |ui| {
                json_nodes(ui, &base_ref, "", data, insert)
            });
        }
    }
}

pub(super) fn json_nodes(
    ui: &mut egui::Ui,
    base_ref: &str,
    pointer: &str,
    node: &serde_json::Value,
    insert: &mut Option<PendingInsert>,
) {
    use serde_json::Value;
    let children: Vec<(String, &Value)> = match node {
        Value::Object(m) => m.iter().map(|(k, v)| (escape_ptr(k), v)).collect(),
        Value::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v))
            .collect(),
        _ => return,
    };
    for (key, value) in children {
        let ptr = format!("{pointer}/{key}");
        let full = format!("{base_ref}#{ptr}");
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            let label = match value {
                Value::Object(m) => format!("{key} {{{}}}", m.len()),
                Value::Array(a) => format!("{key} [{}]", a.len()),
                other => format!("{key}: {}", short(other)),
            };
            ui.label(RichText::new(label).monospace().small());
            if ui
                .small_button("insert")
                .on_hover_text(full.clone())
                .clicked()
            {
                *insert = Some(PendingInsert {
                    target: InsertTarget::Binding,
                    suggested_name: suggested_name(&full),
                    snippet: format!("{{ \"ref\": \"{full}\" }}"),
                });
            }
        });
        if value.is_object() || value.is_array() {
            ui.indent(&ptr, |ui| json_nodes(ui, base_ref, &ptr, value, insert));
        }
    }
}

/// A ready-to-paste snippet for a whole asset: a binding for data, a pipeline
/// entry for executables (phase inferred from kind).
pub(super) fn snippet_for(asset: &AssetEntry, base_ref: &str) -> String {
    match asset.kind {
        AssetKind::Data => format!("{{ \"ref\": \"{base_ref}\" }}"),
        AssetKind::Generator => format!("{{ \"use\": \"{base_ref}\", \"with\": {{}} }}"),
        AssetKind::Hook => {
            format!("{{ \"phase\": \"beforeRequest\", \"use\": \"{base_ref}\", \"with\": {{}} }}")
        }
        AssetKind::Assertion | AssetKind::Extractor => {
            format!("{{ \"phase\": \"afterResponse\", \"use\": \"{base_ref}\", \"with\": {{}} }}")
        }
        AssetKind::Mock => format!("{{ \"use\": \"{base_ref}\", \"with\": {{}} }}"),
        AssetKind::Executable => format!("\"{base_ref}\""),
    }
}

pub(super) fn insert_for_asset(
    asset: &AssetEntry,
    base_ref: &str,
) -> Result<PendingInsert, String> {
    let target = match asset.kind {
        AssetKind::Data | AssetKind::Generator => InsertTarget::Binding,
        AssetKind::Assertion => InsertTarget::Assertion,
        AssetKind::Hook | AssetKind::Extractor => InsertTarget::Pipeline,
        AssetKind::Mock => InsertTarget::Mock,
        AssetKind::Executable => {
            return Err("generic executable assets need metadata with a pipeline phase".to_string())
        }
    };
    Ok(PendingInsert {
        target,
        suggested_name: suggested_name(base_ref),
        snippet: snippet_for(asset, base_ref),
    })
}

pub(super) fn suggested_name(reference: &str) -> String {
    reference
        .split('#')
        .next_back()
        .unwrap_or(reference)
        .trim_matches('/')
        .rsplit(['/', ':'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("asset")
        .trim_end_matches(".json")
        .to_string()
}

pub(super) fn apply_structured_insert(
    text: &str,
    insert: PendingInsert,
) -> Result<(String, String), String> {
    let mut document = forge_core::reqv1::RequestDocument::parse(text)
        .map_err(|error| format!("fix the request JSON before inserting: {error}"))?;
    let notice = match insert.target {
        InsertTarget::Binding => {
            let binding: Binding = serde_json::from_str(&insert.snippet)
                .map_err(|error| format!("invalid binding: {error}"))?;
            let name = document.insert_binding(&insert.suggested_name, binding);
            format!("Inserted binding “{name}”; reference it as ${{bindings.{name}}}.")
        }
        InsertTarget::Assertion => return Err("assertions belong in the assertion sidecar".into()),
        InsertTarget::Pipeline => return Err("hooks belong in the hook sidecar".to_string()),
        InsertTarget::Mock => {
            let mock: MockDef = serde_json::from_str(&insert.snippet)
                .map_err(|error| format!("invalid mock: {error}"))?;
            document.mock = Some(mock);
            "Set the request mock.".to_string()
        }
    };
    let mut text = serde_json::to_string_pretty(&document).map_err(|error| error.to_string())?;
    text.push('\n');
    Ok((text, notice))
}

pub(super) fn apply_insert(
    editor: &mut V1EditorState,
    insert: PendingInsert,
) -> Result<(Option<String>, String), String> {
    if matches!(insert.target, InsertTarget::Assertion) {
        let entry: PipelineEntry = serde_json::from_str(&insert.snippet)
            .map_err(|error| format!("invalid assertion: {error}"))?;
        let mut assertion: AssertionEntry = entry.into();
        if let Some(index) = editor.editing_assertion.take() {
            let current = editor
                .assertions
                .assertions
                .get_mut(index)
                .ok_or_else(|| "the assertion no longer exists".to_string())?;
            assertion.enabled = current.enabled;
            *current = assertion;
            return Ok((None, "Updated the assertion in its sidecar.".to_string()));
        }
        editor.assertions.push(assertion);
        return Ok((
            None,
            "Added the assertion outside the request document.".to_string(),
        ));
    }
    if matches!(insert.target, InsertTarget::Pipeline) {
        let mut hook: PipelineEntry = serde_json::from_str(&insert.snippet)
            .map_err(|error| format!("invalid hook: {error}"))?;
        if let Some(index) = editor.editing_hook.take() {
            let current = editor
                .hooks
                .hooks
                .get_mut(index)
                .ok_or_else(|| "the hook no longer exists".to_string())?;
            hook.enabled = current.enabled;
            *current = hook;
            return Ok((None, "Updated the hook in its sidecar.".to_string()));
        }
        editor.hooks.push(hook);
        return Ok((
            None,
            "Added the hook outside the request document.".to_string(),
        ));
    }
    apply_structured_insert(&editor.text, insert).map(|(text, notice)| (Some(text), notice))
}
