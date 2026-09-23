use super::*;

#[test]
fn saves_reject_external_changes_and_keep_valid_or_invalid_editor_buffers() {
    for invalid in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("project.json"), r#"{"formatVersion":1}"#).unwrap();
        let path = root.path().join("test.request.json");
        std::fs::write(&path, SKELETON).unwrap();
        let mut editor = V1EditorState::default();
        editor.open_file(path.clone(), None).unwrap();
        editor.text = if invalid {
            "{ unfinished local edit".into()
        } else {
            editor.text.replace("example.com", "local.example.test")
        };
        editor.dirty = true;
        let local_text = editor.text.clone();
        // A sidecar-only external edit must also cause a conflict.
        let sidecar = forge_core::reqv1::assertions_path(&path);
        let external = r#"{"formatVersion":1,"kind":"assertions","assertions":[]}"#;
        std::fs::write(&sidecar, external).unwrap();
        let external_revision = forge_core::reqv1::request_revision(&path).unwrap();
        assert!(!save_now(&mut editor));
        assert!(editor.dirty);
        assert_eq!(editor.text, local_text);
        assert!(editor.save_conflict.is_some());
        assert!(editor.diagnostics.join(" ").contains("revision conflict"));
        assert_eq!(
            forge_core::reqv1::request_revision(&path).unwrap(),
            external_revision
        );
        assert_eq!(std::fs::read_to_string(sidecar).unwrap(), external);
        editor.open_file(path.clone(), None).unwrap();
        assert!(save_now(&mut editor));
        assert!(!editor.dirty);
        assert!(editor.save_conflict.is_none());
        assert_eq!(
            editor.revision.as_deref(),
            Some(forge_core::reqv1::request_revision(&path).unwrap().as_str())
        );
        assert!(
            save_now(&mut editor),
            "a second save must use the updated revision"
        );
    }
}

#[test]
fn editor_columns_never_cross_the_sidebar_boundary() {
    for available_width in [360.0_f32, 480.0, 539.0, 540.0, 900.0] {
        let (catalog_width, request_width) = editor_column_widths(available_width);

        assert!(
            catalog_width + EDITOR_COLUMN_GAP + request_width <= available_width,
            "columns exceed {available_width}px: catalog={catalog_width}, request={request_width}"
        );
        assert!(
            request_width > TOOLBAR_MENU_CELL_WIDTH + TOOLBAR_TRAILING_GUTTER,
            "toolbar edge controls do not fit inside {request_width}px"
        );
    }
}

#[test]
fn editor_font_zoom_is_smooth_and_uses_setting_bounds() {
    let zoomed = zoom_editor_font_size(15.0, 1.01);
    assert!(zoomed > 15.0 && zoomed < 16.0);
    assert_eq!(zoom_editor_font_size(24.0, 2.0), 24.0);
    assert_eq!(zoom_editor_font_size(9.0, 0.5), 9.0);
}

#[test]
fn matched_openapi_assist_only_reserves_its_compact_row() {
    let spec = openapi_fixture();
    let text = apply_openapi_operation(SKELETON, &spec.operations[0]).unwrap();
    let editor = V1EditorState {
        validated_document: forge_core::reqv1::RequestDocument::parse(&text).ok(),
        openapi: Some(spec),
        ..V1EditorState::default()
    };

    assert_eq!(request_editor_footer_height(&editor), 28.0);
}

#[test]
fn openapi_suggestions_reserve_two_assist_rows() {
    let spec = openapi_fixture();
    let document = forge_core::reqv1::RequestDocument::parse(SKELETON).unwrap();
    let editor = V1EditorState {
        validated_document: Some(document),
        openapi: Some(spec),
        ..V1EditorState::default()
    };

    assert_eq!(request_editor_footer_height(&editor), 64.0);
}

#[test]
fn results_typography_scales_without_changing_the_default() {
    let mut style = egui::Style::default();
    let body_size = style.text_styles[&egui::TextStyle::Body].size;

    scale_results_typography(&mut style, crate::state::DEFAULT_EDITOR_FONT_SIZE);
    assert_eq!(style.text_styles[&egui::TextStyle::Body].size, body_size);

    scale_results_typography(&mut style, crate::state::DEFAULT_EDITOR_FONT_SIZE * 1.2);
    assert!((style.text_styles[&egui::TextStyle::Body].size - body_size * 1.2).abs() < 0.01);
}
use forge_core::reqv1::index::{AssetEntry, Usage};
use forge_core::reqv1::AssetKind;

fn asset(kind: AssetKind, alias: &str) -> AssetEntry {
    AssetEntry {
        path: String::new(),
        rel_path: "assets/x".to_string(),
        kind,
        alias: Some(alias.to_string()),
        prefix_ref: None,
        used_by: Vec::<Usage>::new(),
        data: None,
        metadata: None,
    }
}

fn openapi_fixture() -> ParsedSpec {
    forge_core::openapi::parse_spec(
        &serde_json::json!({
            "openapi": "3.0.3",
            "info": {"title": "Shop", "version": "1"},
            "paths": {
                "/pets/{petId}": {
                    "post": {
                        "operationId": "updatePet",
                        "parameters": [
                            {"name": "petId", "in": "path", "required": true, "schema": {"type": "string"}},
                            {"name": "expand", "in": "query", "required": true, "schema": {"type": "string"}},
                            {"name": "X-Tenant", "in": "header", "required": true, "schema": {"type": "string"}}
                        ],
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["name"],
                                        "properties": {"name": {"type": "string"}}
                                    }
                                }
                            }
                        },
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": {
                                    "application/json": {
                                        "schema": {
                                            "type": "object",
                                            "required": ["name"],
                                            "properties": {"name": {"type": "string"}}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap()
}

#[test]
fn openapi_completion_fills_path_parameters_and_body() {
    let spec = openapi_fixture();
    let operation = &spec.operations[0];
    let completed = apply_openapi_operation(SKELETON, operation).unwrap();
    let document = forge_core::reqv1::RequestDocument::parse(&completed).unwrap();

    assert_eq!(
        document.request.url,
        "https://example.com/pets/${bindings.petId}"
    );
    assert!(document.bindings.contains_key("petId"));
    assert!(document
        .request
        .query
        .iter()
        .any(|parameter| parameter.name == "expand"));
    assert!(document
        .request
        .headers
        .iter()
        .any(|header| header.name == "X-Tenant"));
    assert!(document.request.body.is_some());
    assert!(openapi_request_issues(&document, operation).is_empty());
}

#[test]
fn openapi_operation_filters_combine_method_shape_and_text() {
    let spec = openapi_fixture();
    let operation = &spec.operations[0];

    assert!(OpenApiFilter::Method(Method::Post).matches(operation));
    assert!(!OpenApiFilter::Method(Method::Get).matches(operation));
    assert!(OpenApiFilter::Headers.matches(operation));
    assert!(OpenApiFilter::Query.matches(operation));
    assert!(OpenApiFilter::Path.matches(operation));
    assert!(OpenApiFilter::Body.matches(operation));
    assert!(operation_matches_query(operation, "updatepet"));
    assert!(!operation_matches_query(operation, "missing"));
}

#[test]
fn local_openapi_specs_are_discovered_without_project_config() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("specs")).unwrap();
    std::fs::write(
        root.path().join("specs/shop.json"),
        serde_json::to_string(&openapi_fixture().raw).unwrap(),
    )
    .unwrap();

    let (source, spec, error) = discover_openapi(root.path());
    assert!(source.unwrap().ends_with("specs/shop.json"));
    assert_eq!(spec.unwrap().title, "Shop");
    assert!(error.is_none());
}

#[test]
fn response_contract_issues_use_the_discovered_operation() {
    let spec = openapi_fixture();
    let operation = &spec.operations[0];
    let text = apply_openapi_operation(SKELETON, operation).unwrap();
    let editor = V1EditorState {
        text,
        openapi: Some(spec),
        ..V1EditorState::default()
    };
    let response = ResponseView {
        status: 200,
        headers: vec![(
            "Content-Type".to_string(),
            "application/json; charset=utf-8".to_string(),
        )],
        body: br#"{"name":"Fido"}"#.to_vec(),
        time_ms: 12,
    };

    assert!(openapi_response_issues(&editor, &response).is_empty());
}

#[test]
fn snippet_shapes_per_kind() {
    assert!(snippet_for(&asset(AssetKind::Data, "data:users"), "data:users").contains("\"ref\""));
    assert!(snippet_for(
        &asset(AssetKind::Hook, "project:hooks/x"),
        "project:hooks/x"
    )
    .contains("\"phase\": \"beforeRequest\""));
    assert!(snippet_for(
        &asset(AssetKind::Assertion, "project:assertions/x"),
        "project:assertions/x"
    )
    .contains("afterResponse"));
}

#[test]
fn project_metadata_builds_a_typed_configured_snippet() {
    let mut asset = asset(AssetKind::Assertion, "project:assertions/user");
    let metadata = ProjectAssetMetadata {
        title: "User".to_string(),
        description: String::new(),
        intent: BuiltinIntent::Validate,
        phase: Some(forge_core::reqv1::model::PipelinePhase::AfterResponse),
        parameters: vec![ProjectAssetParameter {
            name: "expected".to_string(),
            label: "Expected".to_string(),
            kind: BuiltinParameterKind::Integer,
            required: true,
            default: None,
            options: Vec::new(),
            example: "201".to_string(),
        }],
        example: serde_json::json!({"expected": 201}),
    };
    asset.metadata = Some(metadata.clone());
    let parameters = metadata
        .parameters
        .iter()
        .map(ParameterDefinition::project)
        .collect::<Vec<_>>();
    let inputs = BTreeMap::from([(
        "expected".to_string(),
        ParameterInput {
            source: ParameterSource::Literal,
            value: "201".to_string(),
        },
    )]);

    let snippet = project_snippet(
        &asset,
        "project:assertions/user",
        &metadata,
        &parameters,
        &inputs,
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&snippet).unwrap();

    assert_eq!(value["with"]["expected"], 201);
    assert_eq!(value["phase"], "afterResponse");
}

#[test]
fn matrix_documents_get_a_distinct_run_action() {
    assert!(!document_has_matrix(SKELETON));
    let matrix = SKELETON.replace(
        "\"request\": {",
        "\"matrix\": {\"case\": {\"value\": [1, 2]}},\n  \"request\": {",
    );
    assert!(document_has_matrix(&matrix));
}

#[test]
fn structured_insert_uses_named_slots_instead_of_cursor_text() {
    let insert = PendingInsert {
        target: InsertTarget::Binding,
        suggested_name: "user-id".to_string(),
        snippet: r#"{"ref":"data:users#/0/id"}"#.to_string(),
    };
    let (text, notice) = apply_structured_insert(SKELETON, insert).unwrap();
    let document = forge_core::reqv1::RequestDocument::parse(&text).unwrap();

    assert!(document.bindings.contains_key("user_id"));
    assert!(notice.contains("${bindings.user_id}"));
}

#[test]
fn parameter_sources_stay_whole_string_expressions() {
    assert_eq!(
        sourced_value(ParameterSource::Binding, "user.id"),
        Some(serde_json::json!("${bindings.user.id}"))
    );
    assert_eq!(
        sourced_value(ParameterSource::Environment, "baseUrl"),
        Some(serde_json::json!("${env.baseUrl}"))
    );
    assert_eq!(
        sourced_value(ParameterSource::Runtime, "token"),
        Some(serde_json::json!("${runtime.token}"))
    );
    assert_eq!(
        sourced_value(ParameterSource::Matrix, "region"),
        Some(serde_json::json!("${matrix.region}"))
    );
    assert_eq!(
        sourced_value(ParameterSource::Secret, "apiKey"),
        Some(serde_json::json!("${secret.apiKey}"))
    );
    assert_eq!(sourced_value(ParameterSource::Literal, "200"), None);
}

#[test]
fn typed_parameters_hide_string_only_secret_source() {
    assert!(parameter_sources(BuiltinParameterKind::String).contains(&ParameterSource::Secret));
    for kind in [
        BuiltinParameterKind::Integer,
        BuiltinParameterKind::Boolean,
        BuiltinParameterKind::Json,
    ] {
        let sources = parameter_sources(kind);
        assert!(!sources.contains(&ParameterSource::Secret));
        assert!(sources.contains(&ParameterSource::Binding));
        assert!(sources.contains(&ParameterSource::Environment));
        assert!(sources.contains(&ParameterSource::Runtime));
        assert!(sources.contains(&ParameterSource::Matrix));
    }
}

#[test]
fn secret_suggestions_expose_names_without_values() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env.local"), "API_TOKEN=super-secret\n").unwrap();
    let editor = V1EditorState {
        root: Some(root.path().to_path_buf()),
        text: SKELETON.to_string(),
        ..V1EditorState::default()
    };

    let suggestions = parameter_suggestions(&editor, ParameterSource::Secret);

    assert_eq!(suggestions, ["API_TOKEN"]);
    assert!(!suggestions.iter().any(|value| value == "super-secret"));
}

#[test]
fn preview_rejects_scopes_the_bridge_cannot_supply() {
    let mut inputs = BTreeMap::from([(
        "expected".to_string(),
        ParameterInput {
            source: ParameterSource::Binding,
            value: "status".to_string(),
        },
    )]);
    assert!(preview_supports_sources(&inputs));

    inputs.get_mut("expected").unwrap().source = ParameterSource::Matrix;
    assert!(!preview_supports_sources(&inputs));
    inputs.get_mut("expected").unwrap().source = ParameterSource::Runtime;
    assert!(!preview_supports_sources(&inputs));
}

#[test]
fn configured_builtin_inserts_typed_or_sourced_parameters() {
    let definition = find_builtin("assert-status").unwrap();
    let mut inputs = BTreeMap::new();
    inputs.insert(
        "expected".to_string(),
        ParameterInput {
            source: ParameterSource::Literal,
            value: "201".to_string(),
        },
    );
    let literal: serde_json::Value =
        serde_json::from_str(&builtin_snippet(definition, &inputs).unwrap()).unwrap();
    assert_eq!(literal["with"]["expected"], serde_json::json!(201));

    inputs.get_mut("expected").unwrap().value = "-1".to_string();
    assert!(builtin_snippet(definition, &inputs)
        .unwrap_err()
        .contains("non-negative integer"));

    inputs.get_mut("expected").unwrap().source = ParameterSource::Binding;
    inputs.get_mut("expected").unwrap().value = "expectedStatus".to_string();
    let sourced: serde_json::Value =
        serde_json::from_str(&builtin_snippet(definition, &inputs).unwrap()).unwrap();
    assert_eq!(
        sourced["with"]["expected"],
        serde_json::json!("${bindings.expectedStatus}")
    );
    assert_eq!(sourced["use"], "builtin:assert-status@1");
    assert_eq!(sourced["phase"], "afterResponse");
}

#[test]
fn stale_preview_result_is_ignored() {
    let mut editor = V1EditorState {
        active_preview: Some(2),
        preview_in_flight: true,
        ..V1EditorState::default()
    };
    let preview = || CatalogPreview {
        request_before: None,
        request_after: None,
        assertions: Vec::new(),
        runtime_writes: BTreeMap::new(),
        logs: Vec::new(),
        diagnostics: Vec::new(),
    };

    editor.handle_preview(1, Ok(preview()));
    assert!(editor.preview.is_none());
    assert!(editor.preview_in_flight);
    assert_eq!(editor.active_preview, Some(2));

    editor.handle_preview(2, Ok(preview()));
    assert!(editor.preview.is_some());
    assert!(!editor.preview_in_flight);
    assert_eq!(editor.active_preview, None);
}

#[test]
fn opening_document_clears_stale_run_state() {
    let root = std::env::temp_dir().join(format!(
        "forge-v1-editor-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("request.json");
    std::fs::write(&file, SKELETON).unwrap();

    let mut editor = V1EditorState {
        active_run: Some(7),
        in_flight: true,
        ..V1EditorState::default()
    };
    editor.open_new(root.clone(), None);
    assert_eq!(editor.active_run, None);
    assert!(!editor.in_flight);
    editor.dirty = false;

    editor.active_run = Some(8);
    editor.in_flight = true;
    editor.open_file(file, None).unwrap();
    assert_eq!(editor.active_run, None);
    assert!(!editor.in_flight);
    assert!(
        !editor.right_panel_open,
        "requests without an OpenAPI contract should not open an empty assistant panel"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn manual_save_mode_keeps_unsaved_request_when_switching_or_creating() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("project.json"), r#"{"formatVersion":1}"#).unwrap();
    let requests = root.path().join("requests");
    std::fs::create_dir_all(&requests).unwrap();
    let current = requests.join("current.request.json");
    let next = requests.join("next.request.json");
    std::fs::write(&current, SKELETON).unwrap();
    std::fs::write(&next, SKELETON).unwrap();

    let mut editor = V1EditorState::default();
    editor.open_file(current.clone(), None).unwrap();
    editor.auto_save = false;
    editor.text = editor.text.replace("example.com", "draft.example.test");
    editor.dirty = true;
    let draft = editor.text.clone();

    let error = editor.open_file(next, None).unwrap_err();
    assert!(error.contains("unsaved edits"));
    assert_eq!(editor.file.as_ref(), Some(&current));
    assert_eq!(editor.text, draft);

    editor.open_new(root.path().to_path_buf(), None);
    assert_eq!(editor.file.as_ref(), Some(&current));
    assert_eq!(editor.text, draft);
    assert!(editor.dirty);

    assert!(editor.save());
    assert!(!editor.dirty);
    assert!(std::fs::read_to_string(current)
        .unwrap()
        .contains("draft.example.test"));
}

#[test]
fn new_requests_get_a_derived_collision_free_path() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("project.json"), "{}").unwrap();
    let requests = root.path().join("requests");
    std::fs::create_dir_all(&requests).unwrap();
    std::fs::write(requests.join("new.request.json"), "{}").unwrap();
    let mut editor = V1EditorState::default();

    editor.open_new(root.path().to_path_buf(), None);
    assert_eq!(
        editor.file.as_deref(),
        Some(requests.join("new-2.request.json").as_path())
    );

    save_now(&mut editor);
    assert!(requests.join("new-2.request.json").is_file());
}

#[test]
fn new_requests_use_the_selected_story_folder() {
    let root = tempfile::tempdir().unwrap();
    let story = root.path().join("requests/checkout");
    std::fs::create_dir_all(&story).unwrap();
    let mut editor = V1EditorState::default();

    editor.open_new_in(root.path().to_path_buf(), story.clone(), None);
    assert_eq!(
        editor.file.as_deref(),
        Some(story.join("new.request.json").as_path())
    );
}

#[test]
fn saving_a_new_request_never_overwrites_a_racing_file() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("project.json"), "{}").unwrap();
    let mut editor = V1EditorState::default();
    editor.open_new(root.path().to_path_buf(), None);
    let path = editor.file.clone().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "keep").unwrap();

    save_now(&mut editor);

    assert_eq!(std::fs::read_to_string(path).unwrap(), "keep");
    assert!(editor.diagnostics[0].contains("failed to save"));
}

#[test]
fn saving_invalid_json_never_overwrites_a_racing_file() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("project.json"), "{}").unwrap();
    let mut editor = V1EditorState::default();
    editor.open_new(root.path().to_path_buf(), None);
    editor.text = "{ invalid".to_string();
    let path = editor.file.clone().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "keep").unwrap();

    save_now(&mut editor);

    assert_eq!(std::fs::read_to_string(path).unwrap(), "keep");
    assert!(editor.diagnostics[0].contains("failed to save"));
}

#[test]
fn default_request_validates_without_environment_config() {
    let root = tempfile::tempdir().unwrap();
    forge_core::store::Workspace::create(root.path(), "Zero config").unwrap();
    let document = forge_core::reqv1::RequestDocument::parse(SKELETON).unwrap();

    let result = forge_core::reqv1::validate(
        &document,
        root.path(),
        &root.path().join("requests/new.request.json"),
        serde_json::json!({}),
        &|_| None,
    );

    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn assertion_insert_is_saved_beside_the_request() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("project.json"), "{}").unwrap();
    let mut editor = V1EditorState::default();
    editor.open_new(root.path().to_path_buf(), None);
    let request_path = editor.file.clone().unwrap();
    let insert = PendingInsert {
        target: InsertTarget::Assertion,
        suggested_name: "assert-status".to_string(),
        snippet:
            r#"{"phase":"afterResponse","use":"builtin:assert-status@1","with":{"expected":201}}"#
                .to_string(),
    };

    apply_insert(&mut editor, insert).unwrap();
    assert!(save_now(&mut editor));

    let request =
        forge_core::reqv1::RequestDocument::parse(&std::fs::read_to_string(&request_path).unwrap())
            .unwrap();
    let assertions = AssertionDocument::load_for_request(&request_path).unwrap();
    assert!(request.pipeline.is_empty());
    assert_eq!(assertions.assertions.len(), 1);
    assert_eq!(effective_document(&editor).unwrap().pipeline.len(), 1);
}

#[test]
fn configured_assertion_can_be_edited_in_the_catalog() {
    let mut editor = V1EditorState {
        catalog_view: CatalogView::Project,
        ..V1EditorState::default()
    };
    editor.assertions.push(AssertionEntry {
        uses: "builtin:assert-status@1".to_string(),
        with: serde_json::Map::from_iter([(
            "expected".to_string(),
            "${env.expectedStatus}".into(),
        )]),
        enabled: true,
    });

    begin_assertion_edit(&mut editor, 0).unwrap();
    assert_eq!(editor.catalog_view, CatalogView::Builtins);
    assert_eq!(editor.selected_builtin.as_deref(), Some("assert-status"));
    assert_eq!(
        editor.catalog_inputs["expected"].source,
        ParameterSource::Environment
    );
    assert_eq!(editor.catalog_inputs["expected"].value, "expectedStatus");

    let expected = editor.catalog_inputs.get_mut("expected").unwrap();
    expected.source = ParameterSource::Literal;
    expected.value = "204".to_string();
    let definition = find_builtin("assert-status").unwrap();
    let insert = PendingInsert {
        target: InsertTarget::Assertion,
        suggested_name: definition.name.to_string(),
        snippet: builtin_snippet(definition, &editor.catalog_inputs).unwrap(),
    };
    apply_insert(&mut editor, insert).unwrap();

    assert_eq!(editor.assertions.assertions.len(), 1);
    assert_eq!(editor.assertions.assertions[0].with["expected"], 204);
    assert_eq!(editor.editing_assertion, None);
}

#[test]
fn catalog_search_and_intent_filters_combine() {
    let status = find_builtin("assert-status").unwrap();
    let bearer = find_builtin("bearer").unwrap();

    assert!(builtin_matches(
        status,
        "http response status",
        Some("Validate")
    ));
    assert!(!builtin_matches(
        status,
        "http response status",
        Some("Prepare")
    ));
    assert!(builtin_matches(bearer, "authorization", None));
    assert!(!builtin_matches(bearer, "response cookie", None));
}

#[test]
fn hook_is_saved_beside_the_request_and_edited_in_the_catalog() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("project.json"), "{}").unwrap();
    let mut editor = V1EditorState::default();
    editor.open_new(root.path().to_path_buf(), None);
    let request_path = editor.file.clone().unwrap();
    let insert = PendingInsert {
        target: InsertTarget::Pipeline,
        suggested_name: "header".to_string(),
        snippet: r#"{"phase":"beforeRequest","use":"builtin:header@1","with":{"name":"X-Test","value":"old"}}"#
            .to_string(),
    };
    apply_insert(&mut editor, insert).unwrap();

    begin_hook_edit(&mut editor, 0).unwrap();
    assert_eq!(editor.selected_builtin.as_deref(), Some("header"));
    editor.catalog_inputs.get_mut("value").unwrap().value = "new".to_string();
    let definition = find_builtin("header").unwrap();
    let update = PendingInsert {
        target: InsertTarget::Pipeline,
        suggested_name: definition.name.to_string(),
        snippet: builtin_snippet(definition, &editor.catalog_inputs).unwrap(),
    };
    apply_insert(&mut editor, update).unwrap();
    assert!(save_now(&mut editor));

    let request =
        forge_core::reqv1::RequestDocument::parse(&std::fs::read_to_string(&request_path).unwrap())
            .unwrap();
    let hooks = HookDocument::load_for_request(&request_path).unwrap();
    assert!(request.pipeline.is_empty());
    assert_eq!(hooks.hooks.len(), 1);
    assert_eq!(hooks.hooks[0].with["value"], "new");
    assert_eq!(effective_document(&editor).unwrap().pipeline.len(), 1);
}

#[test]
fn html_response_markup_is_indented() {
    let html = "<!doctype html><html lang=\"en\"><head><title>Example Domain</title><link rel=\"icon\" href=\"data:,\"></head><body><h1>Example Domain</h1></body></html>";

    let formatted = pretty_markup(html);

    assert!(formatted.contains("\n<html lang=\"en\">\n  <head>"));
    assert!(formatted.contains("\n      Example Domain\n    </title>"));
    assert!(formatted.contains("\n    <link rel=\"icon\" href=\"data:,\">\n  </head>"));
    assert!(formatted.ends_with("\n</html>"));
}

#[test]
fn failed_run_opens_diagnostics() {
    let mut editor = V1EditorState {
        active_run: Some(7),
        in_flight: true,
        ..V1EditorState::default()
    };

    editor.handle_result(7, Err("project code is disabled".to_string()));

    assert_eq!(editor.result_tab, ResultTab::Diagnostics);
    assert_eq!(editor.diagnostics, ["project code is disabled"]);
}

#[test]
fn failed_open_keeps_the_current_buffer() {
    let mut editor = V1EditorState {
        text: "keep me".to_string(),
        dirty: true,
        ..V1EditorState::default()
    };

    let error = editor
        .open_file(
            std::path::PathBuf::from("/definitely/missing/request.json"),
            None,
        )
        .expect_err("missing file must fail");

    assert!(error.contains("unsaved edits"));
    assert_eq!(editor.text, "keep me");
    assert!(editor.dirty);

    editor.dirty = false;
    let error = editor
        .open_file(
            std::path::PathBuf::from("/definitely/missing/request.json"),
            None,
        )
        .expect_err("missing file must fail");
    assert!(error.contains("failed to read"));
    assert_eq!(editor.text, "keep me");
    assert!(!editor.dirty);
}

#[test]
fn auth_fetcher_is_saved_centrally_not_in_the_request() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("requests/auth/token.request.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(root.path().join("project.json"), "{\"formatVersion\":1}").unwrap();
    std::fs::write(&file, SKELETON).unwrap();
    let mut editor = V1EditorState {
        root: Some(root.path().to_path_buf()),
        file: Some(file.clone()),
        project_auth: Some(ProjectAuthConfig::for_request(
            "requests/auth/token.request.json".to_string(),
        )),
        auth_dirty: true,
        ..V1EditorState::default()
    };

    save_project_auth(&mut editor);

    let project = forge_core::reqv1::load_project(root.path()).unwrap();
    assert_eq!(
        project.auth.unwrap().request,
        "requests/auth/token.request.json"
    );
    let request =
        forge_core::reqv1::RequestDocument::parse(&std::fs::read_to_string(file).unwrap()).unwrap();
    assert_eq!(request.meta.id, "new.request");
    assert!(!editor.auth_dirty);
}

#[test]
fn provider_presets_build_expected_oauth_requests() {
    let cases = [
        (
            AuthProvider::Generic,
            "idp.example.com/token",
            "",
            "scope-a",
            "https://idp.example.com/token",
        ),
        (
            AuthProvider::Keycloak,
            "https://idp.example.com/auth",
            "acme",
            "scope-a",
            "https://idp.example.com/auth/realms/acme/protocol/openid-connect/token",
        ),
        (
            AuthProvider::Auth0,
            "tenant.auth0.com",
            "",
            "https://api.example.com",
            "https://tenant.auth0.com/oauth/token",
        ),
        (
            AuthProvider::Entra,
            "tenant-id",
            "",
            "https://api.example.com/.default",
            "https://login.microsoftonline.com/tenant-id/oauth2/v2.0/token",
        ),
    ];

    for (provider, endpoint, realm, scope, expected_url) in cases {
        let draft = AuthDraft {
            provider,
            endpoint: endpoint.to_string(),
            realm: realm.to_string(),
            client_id: "client-id".to_string(),
            client_secret: "must-not-be-persisted".to_string(),
            scope: scope.to_string(),
        };

        let document = provider_auth_document(&draft, provider.file_stem()).unwrap();
        let value = serde_json::to_value(document).unwrap();
        let form = &value["request"]["body"]["value"];

        assert_eq!(value["request"]["url"], expected_url);
        assert_eq!(form["grant_type"], "client_credentials");
        assert_eq!(form["client_id"], "client-id");
        assert_eq!(
            form["client_secret"],
            format!("${{secret.{}}}", provider.secret_name())
        );
        assert!(!value.to_string().contains("must-not-be-persisted"));
        if provider == AuthProvider::Auth0 {
            assert_eq!(form["audience"], scope);
        } else {
            assert_eq!(form["scope"], scope);
        }
    }
}

#[test]
fn provider_setup_creates_and_activates_a_derived_auth_request() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("project.json"), "{\"formatVersion\":1}").unwrap();
    let mut editor = V1EditorState {
        root: Some(root.path().to_path_buf()),
        auth_draft: AuthDraft {
            provider: AuthProvider::Keycloak,
            endpoint: "https://sso.example.com".to_string(),
            realm: "acme".to_string(),
            client_id: "forge".to_string(),
            client_secret: " leading \"secret\" value ".to_string(),
            scope: String::new(),
        },
        ..V1EditorState::default()
    };

    create_provider_auth_request(&mut editor);

    let project = forge_core::reqv1::load_project(root.path()).unwrap();
    let auth = project.auth.unwrap();
    assert_eq!(auth.request, "requests/auth/keycloak-token.request.json");
    let request_text = std::fs::read_to_string(root.path().join(&auth.request)).unwrap();
    assert!(forge_core::reqv1::RequestDocument::parse(&request_text).is_ok());
    assert!(!request_text.contains("leading"));
    assert_eq!(
        forge_core::reqv1::load_file_secrets(root.path())
            .get("KEYCLOAK_CLIENT_SECRET")
            .map(String::as_str),
        Some(" leading \"secret\" value ")
    );
    assert!(editor.auth_draft.client_secret.is_empty());
    assert!(!editor.auth_dirty);
}

#[test]
fn advisor_context_redacts_sensitive_values_and_headers() {
    let mut value = serde_json::json!({
        "token": "secret-token",
        "nested": {"password": "secret-password", "safe": "visible"},
        "headers": [
            {"name": "Authorization", "value": "Bearer secret"},
            {"name": "Accept", "value": "application/json"}
        ]
    });

    redact_sensitive_json(&mut value);

    assert_eq!(value["token"], "***");
    assert_eq!(value["nested"]["password"], "***");
    assert_eq!(value["nested"]["safe"], "visible");
    assert_eq!(value["headers"][0]["value"], "***");
    assert_eq!(value["headers"][1]["value"], "application/json");
    assert!(!value.to_string().contains("secret"));
}

#[test]
fn stale_advisor_reply_does_not_replace_the_current_answer() {
    let mut editor = V1EditorState {
        active_advisor: Some(2),
        advisor_answer: Some("current".to_string()),
        ..V1EditorState::default()
    };

    editor.handle_advisor(1, Ok("stale".to_string()));

    assert_eq!(editor.active_advisor, Some(2));
    assert_eq!(editor.advisor_answer.as_deref(), Some("current"));
}

#[test]
fn editor_validation_reports_exact_json_location() {
    let mut editor = V1EditorState {
        text: "{\n  \"formatVersion\": nope\n}".to_string(),
        ..V1EditorState::default()
    };

    validate_editor_json(&mut editor);

    let diagnostic = editor.json_diagnostic.unwrap();
    assert_eq!(diagnostic.line, 2);
    assert!(diagnostic.column > 2);
    assert!(editor.validated_document.is_none());
}

#[test]
fn marked_openapi_operations_round_trip_locally() {
    let root = tempfile::tempdir().unwrap();
    let marked = BTreeSet::from(["get-pets".to_string(), "post-pets".to_string()]);

    save_marked_operations(root.path(), &marked).unwrap();

    assert_eq!(load_marked_operations(root.path()).unwrap(), marked);
}

#[test]
fn clean_reload_refreshes_revision_and_sidecars_before_next_save() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("project.json"), r#"{"formatVersion":1}"#).unwrap();
    let path = root.path().join("test.request.json");
    std::fs::write(&path, SKELETON).unwrap();
    let mut editor = V1EditorState::default();
    editor.open_file(path.clone(), None).unwrap();
    let changed = SKELETON.replace("New request", "External change");
    std::fs::write(&path, changed).unwrap();
    editor.reload_clean_request_under(root.path()).unwrap();
    assert!(editor.text.contains("External change"));
    assert!(save_now(&mut editor));
    assert!(std::fs::read_to_string(path)
        .unwrap()
        .contains("External change"));
    editor.text = "unsaved".into();
    editor.dirty = true;
    assert!(editor.reload_clean_request_under(root.path()).is_err());
    assert_eq!(editor.text, "unsaved");
}
