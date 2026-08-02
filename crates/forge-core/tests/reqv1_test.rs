//! End-to-end tests for the request-format v1 engine: parse the canonical
//! fixture document, resolve it to the IR (no network), run it over HTTP
//! against a wiremock server, and serve its mock.

use std::path::{Path, PathBuf};

use forge_core::exec::HttpEngine;
use forge_core::reqv1::{self, RunMode, RunStatus};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/reqv1/project")
}

fn request_file() -> PathBuf {
    project_root().join("requests/users/create.request.json")
}

fn load_doc() -> reqv1::RequestDocument {
    let text = std::fs::read_to_string(request_file()).expect("read fixture");
    reqv1::RequestDocument::parse(&text).expect("fixture parses")
}

fn secret(name: &str) -> Option<String> {
    (name == "apiToken").then(|| "s3cr3t-token".to_string())
}

fn auth_project(
    root: &Path,
    server: &MockServer,
    lifetime_seconds: u64,
) -> (PathBuf, reqv1::RequestDocument) {
    let auth_file = root.join("requests/auth/token.request.json");
    let target_file = root.join("requests/protected/me.request.json");
    std::fs::create_dir_all(auth_file.parent().unwrap()).unwrap();
    std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
    std::fs::write(
        root.join("project.json"),
        serde_json::to_vec_pretty(&json!({
            "formatVersion": 1,
            "auth": {
                "request": "requests/auth/token.request.json",
                "tokenPath": "$.access_token",
                "lifetimeSeconds": lifetime_seconds,
                "refreshBeforeSeconds": 0,
                "applyTo": "requests/protected"
            }
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        &auth_file,
        serde_json::to_vec_pretty(&json!({
            "formatVersion": 1,
            "kind": "request",
            "meta": {"id": "auth.token", "name": "Auth token"},
            "request": {"method": "POST", "url": format!("{}/token", server.uri())}
        }))
        .unwrap(),
    )
    .unwrap();
    let target = json!({
        "formatVersion": 1,
        "kind": "request",
        "meta": {"id": "protected.me", "name": "Me"},
        "request": {"method": "GET", "url": format!("{}/me", server.uri())}
    });
    std::fs::write(&target_file, serde_json::to_vec_pretty(&target).unwrap()).unwrap();
    (
        target_file,
        reqv1::RequestDocument::parse(&target.to_string()).unwrap(),
    )
}

#[test]
fn validate_resolves_canonical_document_to_ir() {
    let doc = load_doc();
    let root = project_root();
    // Use the committed environment as-is (no network).
    let env = reqv1::load_environment(&root, Some("local")).expect("env");
    let ir = reqv1::validate(&doc, &root, &request_file(), env, &secret)
        .expect("canonical document must validate");

    assert_eq!(ir.id, "users.create");
    assert_eq!(ir.url, "http://127.0.0.1:18099/users");
    // Body variables resolved from the referenced data asset.
    let forge_core::reqv1::ResolvedBody::Json(body) = &ir.body else {
        panic!("json body")
    };
    assert_eq!(body["name"], "Alice");
    assert_eq!(body["email"], "alice@example.com");
    assert_eq!(body["tenantId"], "t-1");
    // X-Request-ID resolved from the uuid generator (a real uuid).
    let rid = ir
        .headers
        .iter()
        .find(|h| h.name == "X-Request-ID")
        .expect("request id header");
    assert_eq!(rid.value.len(), 36);
    // The secret used by the bearer hook is tracked for masking.
    assert!(ir.secret_values.contains(&"s3cr3t-token".to_string()));
}

#[test]
fn dynamic_mock_interpolation_errors_are_reported() {
    let text = r#"{
      "formatVersion": 1,
      "kind": "request",
      "meta": { "id": "mock.invalid", "name": "Invalid mock" },
      "request": { "method": "GET", "url": "${env.baseUrl}/users" },
      "mock": {
        "use": "project:mocks/create-user-response",
        "with": { "user": "${env.missing}" }
      }
    }"#;
    let doc = reqv1::RequestDocument::parse(text).expect("parse");

    let diagnostics = reqv1::validate(
        &doc,
        &project_root(),
        &request_file(),
        json!({ "baseUrl": "http://mock.local" }),
        &secret,
    )
    .expect_err("missing mock input must fail validation");

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.instance_path.as_deref() == Some("/mock/with")),
        "{diagnostics:?}"
    );
}

#[tokio::test]
async fn runs_over_http_with_hooks_assertions_and_extractor() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/users"))
        .and(header("authorization", "Bearer s3cr3t-token")) // beforeRequest bearer hook
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "json": { "name": "Alice", "email": "alice@example.com" }
        })))
        .mount(&server)
        .await;

    let doc = load_doc();
    let root = project_root();
    // Point the environment's baseUrl at the mock server.
    let env = json!({ "baseUrl": server.uri() });
    let engine = HttpEngine::new();

    let result = reqv1::run(
        &doc,
        &root,
        &request_file(),
        env,
        &secret,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
        Value::Null,
    )
    .await;

    assert_eq!(
        result.status,
        RunStatus::Passed,
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(result.http.as_ref().unwrap().status, 201);
    // assert-status + assert-json-path both passed.
    assert_eq!(result.assertions.len(), 2);
    assert!(
        result.assertions.iter().all(|a| a.passed),
        "{:?}",
        result.assertions
    );
    // extractor wrote userEmail into runtime.
    assert_eq!(
        result.runtime.get("userEmail"),
        Some(&Value::from("alice@example.com"))
    );
    // The secret never leaks into any diagnostic message.
    assert!(result
        .diagnostics
        .iter()
        .all(|d| !d.message.contains("s3cr3t-token")));
}

#[tokio::test]
async fn failed_assertion_marks_run_failed() {
    let server = MockServer::start().await;
    // Return the wrong name so assert-json-path fails.
    Mock::given(method("POST"))
        .and(path("/users"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "json": { "name": "Bob", "email": "bob@example.com" }
        })))
        .mount(&server)
        .await;

    let doc = load_doc();
    let env = json!({ "baseUrl": server.uri() });
    let engine = HttpEngine::new();
    let result = reqv1::run(
        &doc,
        &project_root(),
        &request_file(),
        env,
        &secret,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
        Value::Null,
    )
    .await;

    assert_eq!(result.status, RunStatus::Failed);
    assert!(result.assertions.iter().any(|a| !a.passed));
}

#[tokio::test]
async fn mock_mode_serves_the_document_mock_and_runs_after_response() {
    // No server: mock mode replaces the send. The document's mock returns 201
    // + data:user-responses#/created ({id:u-1,name:Alice}). assert-status
    // passes; assert-json-path on $.json.name fails (mock body has no `json`
    // wrapper) — proving assertions run against the mock, catching drift.
    let doc = load_doc();
    let env = json!({ "baseUrl": "http://unused" });
    let engine = HttpEngine::new();
    let result = reqv1::run(
        &doc,
        &project_root(),
        &request_file(),
        env,
        &secret,
        &engine,
        RunMode::Mock,
        CancellationToken::new(),
        Value::Null,
    )
    .await;

    assert_eq!(result.http.as_ref().unwrap().status, 201);
    // assert-status passed against the mock; the json-path assertion did not.
    assert!(result
        .assertions
        .iter()
        .any(|a| a.passed && a.message.contains("status")));
    assert!(result.assertions.iter().any(|a| !a.passed));
}

#[test]
fn missing_asset_is_reported_with_pointer() {
    let doc = reqv1::RequestDocument::parse(
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"x","name":"x"},
            "request":{"method":"GET","url":"${env.baseUrl}/x"},
            "bindings":{"u":{"ref":"data:users#/valid/nobody"}}}"#,
    )
    .unwrap();
    let root = project_root();
    let env = json!({ "baseUrl": "http://x" });
    let errs = reqv1::validate(&doc, &root, &request_file(), env, &secret).unwrap_err();
    assert!(errs.iter().any(|d| d.code == "INVALID_POINTER"), "{errs:?}");
    assert!(
        errs.iter()
            .any(|d| d.instance_path.as_deref() == Some("/bindings/u")),
        "{errs:?}"
    );
}

#[test]
fn unknown_alias_is_reported() {
    let doc = reqv1::RequestDocument::parse(
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"x","name":"x"},
            "request":{"method":"GET","url":"http://x"},
            "bindings":{"u":{"ref":"data:does-not-exist#/x"}}}"#,
    )
    .unwrap();
    let errs =
        reqv1::validate(&doc, &project_root(), &request_file(), json!({}), &secret).unwrap_err();
    assert!(errs.iter().any(|d| d.code == "INVALID_ALIAS"), "{errs:?}");
}

#[test]
fn resolves_multipart_binary_and_transport_settings_from_project_files() {
    let root = tempfile::tempdir().unwrap();
    let requests = root.path().join("requests");
    let assets = root.path().join("assets");
    std::fs::create_dir_all(&requests).unwrap();
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("upload.txt"), b"file payload").unwrap();
    std::fs::write(assets.join("payload.bin"), [0_u8, 1, 2, 255]).unwrap();
    let request_file = requests.join("body.request.json");

    let multipart = reqv1::RequestDocument::parse(
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"multipart","name":"multipart"},
            "request":{"method":"POST","url":"https://example.test/upload","settings":{
            "timeoutMs":0,"followRedirects":false,"maxRedirects":3,"encodeUrl":false},
            "body":{"type":"multipart","parts":[
            {"type":"text","name":"note","value":"hello ${env.name}","contentType":"text/plain"},
            {"type":"file","name":"upload","file":"../assets/upload.txt","filename":"sent.txt"},
            {"type":"text","name":"disabled","value":"no","enabled":false}]}}}"#,
    )
    .unwrap();
    let ir = reqv1::validate(
        &multipart,
        root.path(),
        &request_file,
        json!({"name": "world"}),
        &secret,
    )
    .unwrap();
    assert_eq!(ir.timeout, None);
    assert!(!ir.follow_redirects);
    assert_eq!(ir.max_redirects, 3);
    assert!(!ir.encode_url);
    let reqv1::ResolvedBody::Multipart(parts) = ir.body else {
        panic!("multipart body")
    };
    assert_eq!(parts.len(), 2);
    assert!(matches!(
        &parts[0].data,
        reqv1::ir::ResolvedMultipartData::Text(value) if value == "hello world"
    ));
    assert!(matches!(
        &parts[1].data,
        reqv1::ir::ResolvedMultipartData::File(path) if path == &assets.join("upload.txt")
    ));
    assert_eq!(parts[1].filename.as_deref(), Some("sent.txt"));

    let binary = reqv1::RequestDocument::parse(
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"binary","name":"binary"},
            "request":{"method":"POST","url":"https://example.test/upload","body":{
            "type":"binary","file":"../assets/payload.bin","contentType":"application/x-test"}}}"#,
    )
    .unwrap();
    let ir = reqv1::validate(&binary, root.path(), &request_file, json!({}), &secret).unwrap();
    let reqv1::ResolvedBody::Binary { content_type, data } = ir.body else {
        panic!("binary body")
    };
    assert_eq!(content_type.as_deref(), Some("application/x-test"));
    assert_eq!(data, [0_u8, 1, 2, 255]);
    assert_eq!(ir.timeout, Some(std::time::Duration::from_secs(30)));
}

#[test]
fn body_files_reject_missing_and_symlink_escape_paths() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let requests = root.path().join("requests");
    let assets = root.path().join("assets");
    std::fs::create_dir_all(&requests).unwrap();
    std::fs::create_dir_all(&assets).unwrap();
    let request_file = requests.join("body.request.json");

    let missing = reqv1::RequestDocument::parse(
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"x","name":"x"},
            "request":{"method":"POST","url":"http://x","body":{"type":"binary",
            "file":"../assets/missing.bin"}}}"#,
    )
    .unwrap();
    let errors =
        reqv1::validate(&missing, root.path(), &request_file, json!({}), &secret).unwrap_err();
    assert!(errors.iter().any(|error| error.code == "ASSET_NOT_FOUND"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        std::fs::write(outside.path().join("outside.bin"), b"outside").unwrap();
        symlink(
            outside.path().join("outside.bin"),
            assets.join("escape.bin"),
        )
        .unwrap();
        let escape = reqv1::RequestDocument::parse(
            r#"{"formatVersion":1,"kind":"request","meta":{"id":"x","name":"x"},
                "request":{"method":"POST","url":"http://x","body":{"type":"binary",
                "file":"../assets/escape.bin"}}}"#,
        )
        .unwrap();
        let errors =
            reqv1::validate(&escape, root.path(), &request_file, json!({}), &secret).unwrap_err();
        assert!(errors.iter().any(|error| error.code == "PATH_ESCAPE"));
    }
}

#[test]
fn schema_json_matches_the_shipped_schema() {
    // The fixture copy and the source schema must not drift.
    let shipped = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/request-v1.schema.json"),
    )
    .expect("shipped schema");
    let fixture = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/reqv1/schemas/request-v1.schema.json"),
    )
    .expect("fixture schema");
    assert_eq!(
        shipped, fixture,
        "fixture schema drifted from schemas/request-v1.schema.json"
    );
    let schema: Value = serde_json::from_str(&shipped).unwrap();
    let typed = json!({
        "formatVersion": 1,
        "kind": "request",
        "meta": {"id": "upload", "name": "Upload"},
        "execution": {
            "delayBeforeMs": 5000,
            "skip": {"reason": "optional", "when": {"not": {
                "var": {"scope": "env", "name": "enabled"}
            }}}
        },
        "request": {
            "method": "POST",
            "url": "https://example.test/upload",
            "settings": {"timeoutMs": 0, "encodeUrl": false},
            "body": {"type": "multipart", "parts": [
                {"type": "text", "name": "note", "value": "hello"},
                {"type": "file", "name": "upload", "file": "../assets/a.bin"}
            ]}
        }
    });
    assert!(jsonschema::is_valid(&schema, &typed));
    let magic = json!({
        "formatVersion": 1,
        "kind": "request",
        "meta": {"id": "upload", "name": "Upload"},
        "request": {
            "method": "POST",
            "url": "https://example.test/upload",
            "body": {"type": "binary", "value": "a.bin"}
        }
    });
    assert!(!jsonschema::is_valid(&schema, &magic));

    let mut negative_delay = typed.clone();
    negative_delay["execution"]["delayBeforeMs"] = json!(-1);
    assert!(!jsonschema::is_valid(&schema, &negative_delay));
}

#[tokio::test]
async fn skip_precedes_interpolation_and_masks_secret_condition_values() {
    let doc = reqv1::RequestDocument::parse(
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"optional","name":"Optional"},
            "execution":{"skip":{"reason":"optional service unavailable","when":{"all":[
                {"var":{"scope":"secret","name":"apiToken"}},
                {"not":{"var":{"scope":"runtime","name":"disabled"}}}
            ]}}},"request":{"method":"GET","url":"${env.missing}/resource"}}"#,
    )
    .unwrap();
    let result = reqv1::run_with_runtime(
        &doc,
        &project_root(),
        &request_file(),
        json!({}),
        &secret,
        &HttpEngine::new(),
        RunMode::Http,
        CancellationToken::new(),
        Value::Null,
        json!({}),
    )
    .await;

    assert_eq!(result.status, RunStatus::Skipped);
    assert_eq!(
        result.skip_reason.as_deref(),
        Some("optional service unavailable")
    );
    assert!(result.http.is_none());
    assert!(result.assertions.is_empty());
    assert!(!serde_json::to_string(&result)
        .unwrap()
        .contains("s3cr3t-token"));
}

#[tokio::test]
async fn execution_conditions_use_bruno_truthiness() {
    let base = |value: Option<Value>| {
        let env = value
            .map(|value| json!({"flag": value}))
            .unwrap_or_else(|| json!({}));
        (env, HttpEngine::new())
    };
    let doc = reqv1::RequestDocument::parse(
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"truthy","name":"Truthy"},
            "execution":{"skip":{"reason":"truthy","when":{"var":{"scope":"env","name":"flag"}}}},
            "request":{"method":"GET","url":"http://unused"},"mock":{"status":200}}"#,
    )
    .unwrap();
    for value in [
        None,
        Some(Value::Null),
        Some(json!(false)),
        Some(json!(0)),
        Some(json!("")),
    ] {
        let (env, engine) = base(value);
        let result = reqv1::run(
            &doc,
            &project_root(),
            &request_file(),
            env,
            &secret,
            &engine,
            RunMode::Mock,
            CancellationToken::new(),
            Value::Null,
        )
        .await;
        assert_eq!(result.status, RunStatus::Passed);
    }
    for value in [json!(true), json!(1), json!("x"), json!([]), json!({})] {
        let (env, engine) = base(Some(value));
        let result = reqv1::run(
            &doc,
            &project_root(),
            &request_file(),
            env,
            &secret,
            &engine,
            RunMode::Mock,
            CancellationToken::new(),
            Value::Null,
        )
        .await;
        assert_eq!(result.status, RunStatus::Skipped);
    }
}

#[tokio::test]
async fn skipped_sequence_step_preserves_position_and_continues() {
    let root = tempfile::tempdir().unwrap();
    let requests = root.path().join("requests");
    std::fs::create_dir_all(&requests).unwrap();
    let skipped = requests.join("optional.request.json");
    let next = requests.join("next.request.json");
    std::fs::write(
        &skipped,
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"optional","name":"Optional"},
            "execution":{"skip":{"reason":"not configured","when":{"literal":true}}},
            "request":{"method":"GET","url":"${env.missing}"}}"#,
    )
    .unwrap();
    std::fs::write(
        &next,
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"next","name":"Next"},
            "request":{"method":"GET","url":"http://unused"},"mock":{"status":204}}"#,
    )
    .unwrap();

    let results = reqv1::run_sequence(
        &[skipped, next],
        root.path(),
        json!({}),
        &|_| None,
        &HttpEngine::new(),
        RunMode::Mock,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].request_id, "optional");
    assert_eq!(results[0].status, RunStatus::Skipped);
    assert_eq!(results[1].request_id, "next");
    assert_eq!(results[1].status, RunStatus::Passed);
}

#[tokio::test]
async fn pre_request_delay_is_cancellable_without_waiting_or_transport() {
    let doc = reqv1::RequestDocument::parse(
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"delayed","name":"Delayed"},
            "execution":{"delayBeforeMs":90000},
            "request":{"method":"GET","url":"http://127.0.0.1:9/never-sent"}}"#,
    )
    .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = reqv1::run(
        &doc,
        &project_root(),
        &request_file(),
        json!({}),
        &secret,
        &HttpEngine::new(),
        RunMode::Http,
        cancel,
        Value::Null,
    )
    .await;
    assert_eq!(result.status, RunStatus::Error);
    assert!(result.http.is_none());
    assert!(result
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("pre-request delay")));
}

#[tokio::test]
async fn matrix_runs_once_per_case_with_case_scoped_values() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/users"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let file = project_root().join("requests/users/create-cases.request.json");
    let text = std::fs::read_to_string(&file).expect("read fixture");
    let doc = reqv1::RequestDocument::parse(&text).expect("parses");
    let env = json!({ "baseUrl": server.uri() });
    let engine = HttpEngine::new();

    let results = reqv1::run_matrix(
        &doc,
        &project_root(),
        &file,
        env,
        &secret,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
    )
    .await
    .expect("matrix resolves");

    // Two cases in data:create-user-cases#/cases, both expect 201.
    assert_eq!(results.len(), 2);
    assert!(
        results.iter().all(|(_, r)| r.status == RunStatus::Passed),
        "{results:?}"
    );
    assert_eq!(results[0].0["case"]["name"], "valid");
    assert_eq!(results[1].0["case"]["name"], "missingEmail");

    // The server saw two distinct payloads — one per case.
    let seen = server.received_requests().await.expect("recorded");
    assert_eq!(seen.len(), 2);
    let bodies: Vec<Value> = seen
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert!(bodies.contains(&json!({ "name": "Alice" })));
    assert!(bodies.contains(&json!({ "name": "Bob" })));
}

#[test]
fn matrix_binding_must_be_an_array() {
    let doc = reqv1::RequestDocument::parse(
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"x","name":"x"},
            "matrix":{"case":{"value":42}},
            "request":{"method":"GET","url":"http://x"}}"#,
    )
    .unwrap();
    let root = project_root();
    let project = reqv1::load_project(&root).unwrap();
    let resolver = reqv1::RefResolver::new(&root, &project).unwrap();
    let store = reqv1::DataStore::new(&resolver);
    let err =
        reqv1::matrix::resolve_cases(&doc.matrix, &resolver, &store, &root, &json!({}), &secret)
            .unwrap_err();
    assert!(
        err.0
            .iter()
            .any(|d| d.message.contains("must resolve to an array")),
        "{err:?}"
    );
}

#[tokio::test]
async fn js_assets_run_hook_assertions_extractor_and_generator() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/users"))
        .and(header("authorization", "Bearer s3cr3t-token")) // JS hook
        .and(header("x-tag", "req-alice")) // JS generator binding
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "u-77", "name": "Alice"
        })))
        .mount(&server)
        .await;

    let file = project_root().join("requests/users/create-js.request.json");
    let doc = reqv1::RequestDocument::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let env = json!({ "baseUrl": server.uri() });
    let engine = HttpEngine::new();

    let result = reqv1::run(
        &doc,
        &project_root(),
        &file,
        env,
        &secret,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
        Value::Null,
    )
    .await;

    assert_eq!(
        result.status,
        RunStatus::Passed,
        "diagnostics: {:?}",
        result.diagnostics
    );
    // The JS assertion asset returned two results, both passing.
    assert_eq!(result.assertions.len(), 2, "{:?}", result.assertions);
    assert!(result.assertions.iter().all(|a| a.passed));
    // The JS extractor wrote runtime.userId from the response body.
    assert_eq!(result.runtime.get("userId"), Some(&json!("u-77")));
}

#[tokio::test]
async fn js_dynamic_mock_serves_and_assertions_run_against_it() {
    // No HTTP server: the dynamic mock asset builds the response from the
    // bound user, so the JS assertion passes and the extractor sees u-mock.
    let file = project_root().join("requests/users/create-js.request.json");
    let doc = reqv1::RequestDocument::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let env = json!({ "baseUrl": "http://unused" });
    let engine = HttpEngine::new();

    let result = reqv1::run(
        &doc,
        &project_root(),
        &file,
        env,
        &secret,
        &engine,
        RunMode::Mock,
        CancellationToken::new(),
        Value::Null,
    )
    .await;

    assert_eq!(
        result.status,
        RunStatus::Passed,
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(result.http.as_ref().unwrap().status, 201);
    assert_eq!(result.runtime.get("userId"), Some(&json!("u-mock")));
}

#[tokio::test]
async fn runtime_and_per_request_environments_thread_through_a_sequence() {
    let login_server = MockServer::start().await;
    let profile_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "token": "tok-xyz" })))
        .mount(&login_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", "Bearer tok-xyz")) // came from request A's extract
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "user": "alice" })))
        .mount(&profile_server)
        .await;

    let root = project_root();
    let a = root.join("requests/users/seq-a.request.json");
    let b = root.join("requests/users/seq-b.request.json");
    let environments = vec![
        json!({ "baseUrl": login_server.uri() }),
        json!({ "baseUrl": profile_server.uri() }),
    ];
    let engine = HttpEngine::new();
    let auth = reqv1::AuthSession::default();

    let results = reqv1::run_sequence_with_environment_values_in_session(
        &[a, b],
        &root,
        &environments,
        &secret,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
        &auth,
    )
    .await
    .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].0.status,
        RunStatus::Passed,
        "{:?}",
        results[0].0.diagnostics
    );
    assert_eq!(results[0].0.runtime.get("authToken"), Some(&json!("***")));
    assert_eq!(
        results[0].1.as_ref().map(|response| response.status),
        Some(200)
    );
    // Request B only passes if ${runtime.authToken} reached it AND the
    // assert-schema builtin validated {user:"alice"}.
    assert_eq!(
        results[1].0.status,
        RunStatus::Passed,
        "{:?}",
        results[1].0.diagnostics
    );
    assert!(
        results[1].0.assertions.iter().all(|a| a.passed),
        "{:?}",
        results[1].0.assertions
    );
    assert_eq!(
        results[1].1.as_ref().map(|response| response.status),
        Some(200)
    );
}

#[tokio::test]
async fn on_error_and_finally_phases_run() {
    // A request whose afterResponse assertion always fails is not an "error"
    // (it's Failed) — so drive an actual error via a hook that can't resolve.
    // Simplest real error: point at a dead server so the send fails.
    let doc = reqv1::RequestDocument::parse(
        r#"{
          "formatVersion": 1, "kind": "request",
          "meta": { "id": "err", "name": "err" },
          "request": { "method": "GET", "url": "${env.baseUrl}/x" },
          "pipeline": [
            { "phase": "onError", "use": "project:hooks/on-error-mark" },
            { "phase": "finally", "use": "project:hooks/finally-mark" }
          ]
        }"#,
    )
    .unwrap();

    // A port nothing listens on -> send fails -> onError + finally run.
    let env = json!({ "baseUrl": "http://127.0.0.1:9" });
    let engine = HttpEngine::new();
    // Write the doc to the fixture project so project: refs resolve.
    let file = project_root().join("requests/users/err.request.json");
    std::fs::write(&file, serde_json::to_string(&doc_json()).unwrap()).ok();

    let result = reqv1::run(
        &doc,
        &project_root(),
        &file,
        env,
        &secret,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
        Value::Null,
    )
    .await;
    let _ = std::fs::remove_file(&file);

    assert_eq!(result.status, RunStatus::Error);
    // onError asset recorded the error; finally asset always ran.
    assert_eq!(result.runtime.get("errored"), Some(&json!(true)));
    assert_eq!(result.runtime.get("finallyRan"), Some(&json!(true)));
    let msg = result
        .runtime
        .get("errorMsg")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(!msg.is_empty(), "onError asset should receive ctx.error");
}

fn doc_json() -> Value {
    json!({
      "formatVersion": 1, "kind": "request",
      "meta": { "id": "err", "name": "err" },
      "request": { "method": "GET", "url": "${env.baseUrl}/x" },
      "pipeline": [
        { "phase": "onError", "use": "project:hooks/on-error-mark" },
        { "phase": "finally", "use": "project:hooks/finally-mark" }
      ]
    })
}

#[tokio::test]
async fn assert_schema_builtin_validates_response_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/users"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "json": { "name": "Alice", "email": "alice@example.com" }
        })))
        .mount(&server)
        .await;

    // Inline document exercising assert-schema pass and fail.
    let doc = reqv1::RequestDocument::parse(
        r#"{
          "formatVersion": 1, "kind": "request",
          "meta": { "id": "sch", "name": "sch" },
          "request": { "method": "POST", "url": "${env.baseUrl}/users",
            "headers": [ { "name": "Content-Type", "value": "application/json", "enabled": true } ],
            "body": { "type": "json", "value": { "x": 1 } } },
          "pipeline": [
            { "phase": "afterResponse", "use": "builtin:assert-schema@1",
              "with": { "schema": { "type": "object", "required": ["json"],
                "properties": { "json": { "type": "object", "required": ["name"],
                  "properties": { "name": { "type": "string" } } } } } } },
            { "phase": "afterResponse", "use": "builtin:assert-schema@1",
              "with": { "schema": { "type": "object", "required": ["missing"] } } }
          ]
        }"#,
    )
    .unwrap();
    let env = json!({ "baseUrl": server.uri() });
    let engine = HttpEngine::new();
    let result = reqv1::run(
        &doc,
        &project_root(),
        &request_file(),
        env,
        &secret,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
        Value::Null,
    )
    .await;

    assert_eq!(result.assertions.len(), 2, "{:?}", result.assertions);
    assert!(
        result.assertions[0].passed,
        "matching schema should pass: {:?}",
        result.assertions[0]
    );
    assert!(
        !result.assertions[1].passed,
        "missing-required schema should fail"
    );
    assert_eq!(result.status, RunStatus::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_server_serves_over_real_http() {
    use forge_core::reqv1::MockServerConfig;

    let env = json!({ "baseUrl": "http://mock.local" });
    let config = MockServerConfig::scan(&project_root(), env, &secret).expect("scan");

    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let port = server.server_addr().to_ip().unwrap().port();

    // Serve exactly the two requests below on a blocking thread.
    std::thread::spawn(move || {
        let sec = |name: &str| (name == "apiToken").then(|| "tok".to_string());
        for _ in 0..2 {
            let request = server.recv().expect("receive");
            let method = request.method().as_str().to_string();
            let url = request.url().to_string();
            let path = url.split('?').next().unwrap_or(&url);
            match config.handle(&method, path, &sec).expect("valid mock") {
                Some(mock) => {
                    request
                        .respond(
                            tiny_http::Response::from_data(mock.body).with_status_code(mock.status),
                        )
                        .expect("respond");
                }
                None => {
                    request
                        .respond(
                            tiny_http::Response::from_string("no mock route").with_status_code(404),
                        )
                        .expect("respond");
                }
            }
        }
    });

    // Hit POST /users — a mocked route — and an unmocked one.
    let client = reqwest::Client::new();
    let ok = client
        .post(format!("http://127.0.0.1:{port}/users"))
        .send()
        .await
        .expect("request");
    assert_eq!(ok.status(), 201);
    let body: Value = ok.json().await.expect("json");
    assert!(body.get("id").is_some(), "{body}");

    let missing = client
        .get(format!("http://127.0.0.1:{port}/does-not-exist"))
        .send()
        .await
        .expect("request");
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn project_auth_fetcher_reuses_a_live_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "token-1"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    let (file, doc) = auth_project(root.path(), &server, 60);
    let engine = HttpEngine::new();
    let auth = reqv1::AuthSession::default();
    for _ in 0..2 {
        let (result, _) = reqv1::run_with_response_in_session(
            &doc,
            root.path(),
            &file,
            json!({}),
            &|_| None,
            &engine,
            RunMode::Http,
            CancellationToken::new(),
            Value::Null,
            &auth,
        )
        .await;
        assert_eq!(result.status, RunStatus::Passed, "{:?}", result.diagnostics);
    }
    server.verify().await;
}

#[tokio::test]
async fn named_project_auth_refreshes_before_observed_request_duration_exceeds_ttl() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "token-1"
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(1_200)))
        .expect(2)
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    let (file, mut doc) = auth_project(root.path(), &server, 2);
    doc.auth = Some(reqv1::RequestAuthSelection::provider("short-lived"));
    std::fs::write(
        root.path().join("project.json"),
        serde_json::to_vec_pretty(&json!({
            "authProviders": {"short-lived": {
                "request": "requests/auth/token.request.json",
                "lifetimeSeconds": 2,
                "refreshBeforeSeconds": 0,
                "applyTo": "requests/protected"
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let engine = HttpEngine::new();
    let auth = reqv1::AuthSession::default();
    for _ in 0..2 {
        let (result, _) = reqv1::run_with_response_in_session(
            &doc,
            root.path(),
            &file,
            json!({}),
            &|_| None,
            &engine,
            RunMode::Http,
            CancellationToken::new(),
            Value::Null,
            &auth,
        )
        .await;
        assert_eq!(result.status, RunStatus::Passed, "{:?}", result.diagnostics);
    }
    server.verify().await;
}

fn write_request(root: &Path, relative: &str, value: Value) -> reqv1::RequestDocument {
    let file = root.join(relative);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    reqv1::RequestDocument::parse(&value.to_string()).unwrap()
}

#[tokio::test]
async fn named_auth_providers_have_isolated_caches_and_explicit_selection() {
    let server = MockServer::start().await;
    for (path_value, token) in [("/token-a", "token-a"), ("/token-b", "token-b")] {
        Mock::given(method("POST"))
            .and(path(path_value))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": token
            })))
            .expect(1)
            .mount(&server)
            .await;
    }
    for (path_value, token) in [("/a", "token-a"), ("/b", "token-b")] {
        Mock::given(method("GET"))
            .and(path(path_value))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;
    }

    let root = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        write_request(
            root.path(),
            &format!("requests/auth/{name}.request.json"),
            json!({
                "formatVersion": 1, "kind": "request", "auth": "none",
                "meta": {"id": format!("auth.{name}"), "name": name},
                "request": {"method": "POST", "url": format!("{}/token-{name}", server.uri())}
            }),
        );
    }
    std::fs::write(
        root.path().join("project.json"),
        serde_json::to_vec_pretty(&json!({
            "authProviders": {
                "a": {"request": "requests/auth/a.request.json", "applyTo": "requests/a"},
                "b": {"request": "requests/auth/b.request.json", "applyTo": "requests/b"}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let file_a = root.path().join("requests/a/get.request.json");
    let file_b = root.path().join("requests/b/get.request.json");
    let doc_a = write_request(
        root.path(),
        "requests/a/get.request.json",
        json!({"formatVersion":1,"kind":"request","auth":"a","meta":{"id":"a","name":"a"},
            "request":{"method":"GET","url":format!("{}/a", server.uri())}}),
    );
    let doc_b = write_request(
        root.path(),
        "requests/b/get.request.json",
        json!({"formatVersion":1,"kind":"request","auth":"b","meta":{"id":"b","name":"b"},
            "request":{"method":"GET","url":format!("{}/b", server.uri())}}),
    );
    let engine = HttpEngine::new();
    let auth = reqv1::AuthSession::default();
    for _ in 0..2 {
        for (doc, file) in [(&doc_a, &file_a), (&doc_b, &file_b)] {
            let (result, _) = reqv1::run_with_response_in_session(
                doc,
                root.path(),
                file,
                json!({}),
                &|_| None,
                &engine,
                RunMode::Http,
                CancellationToken::new(),
                Value::Null,
                &auth,
            )
            .await;
            assert_eq!(result.status, RunStatus::Passed, "{:?}", result.diagnostics);
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn named_auth_uses_most_specific_scope_and_fails_closed_on_ambiguity() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/specific-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"specific"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/target"))
        .and(header("authorization", "Bearer specific"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    for (name, endpoint) in [("broad", "/broad-token"), ("specific", "/specific-token")] {
        write_request(
            root.path(),
            &format!("requests/auth/{name}.request.json"),
            json!({"formatVersion":1,"kind":"request","auth":"none",
                "meta":{"id":name,"name":name},
                "request":{"method":"POST","url":format!("{}{endpoint}", server.uri())}}),
        );
    }
    let target_file = root.path().join("requests/private/target.request.json");
    let target = write_request(
        root.path(),
        "requests/private/target.request.json",
        json!({"formatVersion":1,"kind":"request","meta":{"id":"target","name":"target"},
            "request":{"method":"GET","url":format!("{}/target",server.uri())}}),
    );
    let write_project = |providers: Value| {
        std::fs::write(
            root.path().join("project.json"),
            serde_json::to_vec_pretty(&json!({"authProviders": providers})).unwrap(),
        )
        .unwrap();
    };
    write_project(json!({
        "broad":{"request":"requests/auth/broad.request.json","applyTo":"requests"},
        "specific":{"request":"requests/auth/specific.request.json","applyTo":"requests/private"}
    }));
    let engine = HttpEngine::new();
    let (result, _) = reqv1::run_with_response(
        &target,
        root.path(),
        &target_file,
        json!({}),
        &|_| None,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
        Value::Null,
    )
    .await;
    assert_eq!(result.status, RunStatus::Passed, "{:?}", result.diagnostics);

    write_project(json!({
        "first":{"request":"requests/auth/broad.request.json","applyTo":"requests/private"},
        "second":{"request":"requests/auth/specific.request.json","applyTo":"requests/private"}
    }));
    let (ambiguous, _) = reqv1::run_with_response(
        &target,
        root.path(),
        &target_file,
        json!({}),
        &|_| None,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
        Value::Null,
    )
    .await;
    assert_eq!(ambiguous.status, RunStatus::Error);
    assert!(ambiguous.diagnostics[0]
        .message
        .contains("ambiguous auth providers"));
    server.verify().await;
}

#[tokio::test]
async fn explicit_none_and_provider_requests_never_receive_automatic_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/public"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"token"})))
        .expect(1)
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let provider_file = root.path().join("requests/auth/token.request.json");
    let provider = write_request(
        root.path(),
        "requests/auth/token.request.json",
        json!({"formatVersion":1,"kind":"request","meta":{"id":"auth","name":"auth"},
            "request":{"method":"POST","url":format!("{}/token",server.uri())}}),
    );
    let public_file = root.path().join("requests/public.request.json");
    let public = write_request(
        root.path(),
        "requests/public.request.json",
        json!({"formatVersion":1,"kind":"request","auth":"none","meta":{"id":"public","name":"public"},
            "request":{"method":"GET","url":format!("{}/public",server.uri())}}),
    );
    std::fs::write(
        root.path().join("project.json"),
        serde_json::to_vec_pretty(&json!({"authProviders":{
            "all":{"request":"requests/auth/token.request.json","applyTo":"requests"}
        }}))
        .unwrap(),
    )
    .unwrap();
    let engine = HttpEngine::new();
    for (doc, file) in [(&public, &public_file), (&provider, &provider_file)] {
        let (result, _) = reqv1::run_with_response(
            doc,
            root.path(),
            file,
            json!({}),
            &|_| None,
            &engine,
            RunMode::Http,
            CancellationToken::new(),
            Value::Null,
        )
        .await;
        assert_eq!(result.status, RunStatus::Passed, "{:?}", result.diagnostics);
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| request.headers.get("authorization").is_none()));
}

#[tokio::test]
async fn named_auth_missing_credentials_fail_before_send_with_masked_diagnostic() {
    let server = MockServer::start().await;
    let root = tempfile::tempdir().unwrap();
    write_request(
        root.path(),
        "requests/auth/keycloak.request.json",
        json!({"formatVersion":1,"kind":"request","auth":"none",
            "meta":{"id":"auth.keycloak","name":"keycloak"},
            "request":{"method":"POST","url":format!("{}/token",server.uri()),
                "body":{"type":"form","value":{"grant_type":"client_credentials",
                    "client_secret":"${secret.keycloak_client_secret}"}}}}),
    );
    let target_file = root.path().join("requests/private.request.json");
    let target = write_request(
        root.path(),
        "requests/private.request.json",
        json!({"formatVersion":1,"kind":"request","auth":"keycloak",
            "meta":{"id":"private","name":"private"},
            "request":{"method":"GET","url":format!("{}/private",server.uri())}}),
    );
    std::fs::write(
        root.path().join("project.json"),
        serde_json::to_vec_pretty(&json!({"authProviders":{"keycloak":{
            "request":"requests/auth/keycloak.request.json","applyTo":"requests"
        }}}))
        .unwrap(),
    )
    .unwrap();
    let engine = HttpEngine::new();
    let (result, _) = reqv1::run_with_response(
        &target,
        root.path(),
        &target_file,
        json!({}),
        &|_| None,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
        Value::Null,
    )
    .await;
    assert_eq!(result.status, RunStatus::Error);
    assert!(result.diagnostics[0]
        .message
        .contains("keycloak_client_secret"));
    assert!(!result.diagnostics[0].message.contains("Bearer"));
    assert!(server.received_requests().await.unwrap().is_empty());
}
