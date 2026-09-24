use std::path::{Path, PathBuf};

use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
use forge_core::convert::{
    detect_bruno_source, import_bruno_v1, BrunoSourceFormat, BrunoV1ImportError,
    BrunoV1ImportOptions,
};
use forge_core::exec::HttpEngine;
use forge_core::reqv1::{
    self, AssertionDocument, HookDocument, RequestDocument, RunMode, RunStatus, SequenceDocument,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, body_string, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencollection")
}

fn contract_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencollection-contracts")
}

fn request(path: &Path) -> RequestDocument {
    RequestDocument::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn write_minimal_collection(root: &Path) {
    std::fs::write(
        root.join("opencollection.yml"),
        "opencollection: 1.0.0\ninfo:\n  name: Test\n",
    )
    .unwrap();
}

fn write_source(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn write_ipd452_collection(root: &Path, token_url: &str, service_url: &str) {
    write_source(
        root,
        "opencollection.yml",
        "opencollection: 1.0.0\ninfo:\n  name: Sanitized IPD-452\nrequest:\n  auth:\n    type: bearer\n    token: \"{{access_token}}\"\n",
    );
    write_source(
        root,
        "environments/local.yml",
        &format!(
            "name: local\nvariables:\n  - name: keycloak_url\n    value: {token_url}\n  - name: keycloak_client_secret\n    secret: true\n  - name: data_manager_url\n    value: {service_url}\n  - name: contracts_url\n    value: {service_url}\n"
        ),
    );
    write_source(
        root,
        "services/internal/DataManager/IPD-452_Contracts2_OAuth2/04c_ipd_service_token_und_deployment_pruefen.yml",
        r#"info:
  name: 04c IPD service token
  type: http
  seq: 10
http:
  method: get
  url: "{{data_manager_url}}/actuator/info"
  auth:
    type: none
runtime:
  scripts:
    - type: before-request
      code: |
        const axios = require('axios');
        const adminToken = bru.getVar('access_token') || bru.getEnvVar('access_token');
        const tokenUrl = bru.getEnvVar('keycloak_url');
        const scriptTokenUrl = tokenUrl.replace(/^https:/, 'http:');
        const adminBase = scriptTokenUrl.replace('/realms/gvl/protocol/openid-connect/token', '/admin/realms/gvl');
        const headers = { Authorization: 'Bearer ' + adminToken };
        const clients = await axios.get(adminBase + '/clients?clientId=ipd-upload-data-manager', { headers });
        expect(clients.data).to.be.an('array').that.is.not.empty;
        const clientUuid = clients.data[0].id;
        const secretResponse = await axios.get(adminBase + '/clients/' + clientUuid + '/client-secret', { headers });
        const formBody = [
          'grant_type=client_credentials',
          'client_id=ipd-upload-data-manager',
          'client_secret=' + encodeURIComponent(secretResponse.data.value)
        ].join('&');
        const tokenResponse = await axios.post(scriptTokenUrl, formBody, {
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' }
        });
        bru.setVar('ipd452_service_token', tokenResponse.data.access_token);
    - type: tests
      code: |
        test("deployment", function() {
          expect(res.status).to.equal(200);
          expect(res.body.git.commit.id).to.equal('58798f3');
        });
        test("JWT roles", function() {
          const token = bru.getVar('ipd452_service_token');
          expect(token).to.be.a('string').and.not.empty;
          const payload = JSON.parse(Buffer.from(token.split('.')[1], 'base64').toString());
          const roles = (((payload.resource_access || {})['leg-contracts'] || {}).roles || []);
          expect(roles).to.include('c:r');
          expect(roles).to.include('c-m:r');
        });
"#,
    );
    for (file, seq, suffix) in [
        ("04d_contract.yml", 11, "contracts"),
        ("04e_mandate.yml", 12, "mandates"),
    ] {
        write_source(
            root,
            &format!("services/internal/DataManager/IPD-452_Contracts2_OAuth2/{file}"),
            &format!(
                "info:\n  name: {file}\n  type: http\n  seq: {seq}\nhttp:\n  method: get\n  url: \"{{{{contracts_url}}}}/{suffix}\"\n  auth:\n    type: bearer\n    token: \"{{{{ipd452_service_token}}}}\"\n"
            ),
        );
    }
}

#[test]
fn open_collection_is_preferred_and_inspect_never_writes() {
    assert_eq!(
        detect_bruno_source(&fixture()).unwrap(),
        BrunoSourceFormat::OpenCollectionYaml
    );
    let destination = tempfile::tempdir().unwrap().path().join("not-created");
    let report = import_bruno_v1(
        &fixture(),
        &destination,
        BrunoV1ImportOptions {
            inspect: true,
            exclude_underscore_dirs: true,
        },
    )
    .unwrap();

    assert_eq!(report.scanned_request_count, 5);
    assert_eq!(report.imported_request_count, 3);
    assert_eq!(report.excluded_request_count, 2);
    assert_eq!(
        report.detected_format,
        BrunoSourceFormat::OpenCollectionYaml
    );
    assert!(!destination.exists());
}

#[test]
fn materializes_sanitized_requests_sidecars_and_leaf_sequences() {
    let destination = tempfile::tempdir().unwrap();
    let report = import_bruno_v1(
        &fixture(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(report.environments[0].name, "fallback");
    assert_eq!(report.environments[0].secret_count, 2);

    let create_path = destination
        .path()
        .join("requests/first/create.request.json");
    let create = request(&create_path);
    assert_eq!(create.meta.name, "Create user");
    assert_eq!(create.meta.description.as_deref(), Some("Creates a user."));
    assert_eq!(create.meta.tags, ["smoke"]);
    assert_eq!(
        create.request.url,
        "${env.base_url}/users/7?page=1&filter=a b"
    );
    assert_eq!(create.request.headers[1].value, "${bindings.local_id}");
    assert!(!create.request.headers[2].enabled);
    assert_eq!(create.request.query.len(), 2);
    assert_eq!(create.request.query[0].name, "verbose");
    assert!(!create.request.query[0].enabled);
    assert_eq!(create.request.query[1].name, "sort");
    assert!(create.request.query[1].enabled);
    assert_eq!(create.bindings.len(), 1);

    let hooks = HookDocument::parse(
        &std::fs::read_to_string(destination.path().join("requests/first/create.hooks.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(hooks.hooks.len(), 2);
    assert_eq!(hooks.hooks[0].uses, "builtin:bearer@1");
    assert_eq!(hooks.hooks[0].with["token"], "${secret.api_token}");
    assert_eq!(hooks.hooks[1].uses, "builtin:extract-json-path@1");
    let assertions = AssertionDocument::parse(
        &std::fs::read_to_string(
            destination
                .path()
                .join("requests/first/create.assertions.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(assertions.assertions.len(), 1);
    assert_eq!(assertions.assertions[0].with["expected"], 201);

    let read = request(&destination.path().join("requests/first/read.request.json"));
    assert_eq!(
        read.request.url,
        "${env.base_url}/users/${runtime.created_id}"
    );
    let unrelated = request(&destination.path().join("requests/second/get.request.json"));
    assert_eq!(
        unrelated.request.url,
        "${env.base_url}/users/{{created_id}}"
    );
    assert!(report.diagnostics["second/get.yml"]["variables"][0].contains("created_id"));

    let first_sequence = SequenceDocument::parse(
        &std::fs::read_to_string(
            destination
                .path()
                .join("sequences/first/leaf.sequence.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(first_sequence.meta.name, "First");
    assert_eq!(
        first_sequence.meta.description.as_deref(),
        Some("First leaf documentation.")
    );
    assert_eq!(first_sequence.meta.tags, ["folder-tag"]);
    assert_eq!(
        first_sequence.requests,
        [
            "requests/first/create.request.json",
            "requests/first/read.request.json"
        ]
    );
    let second_sequence = SequenceDocument::parse(
        &std::fs::read_to_string(
            destination
                .path()
                .join("sequences/second/leaf.sequence.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        second_sequence.requests,
        ["requests/second/get.request.json"]
    );
    assert_eq!(report.output_file_count, 11);

    assert_eq!(create.request.settings.timeout_ms, Some(0));
    assert_eq!(create.request.settings.follow_redirects, Some(true));
    assert_eq!(create.request.settings.max_redirects, Some(5));
    assert_eq!(create.request.settings.encode_url, Some(false));
    for feature in [
        "settings.timeout",
        "settings.followRedirects",
        "settings.maxRedirects",
        "settings.encodeUrl",
    ] {
        assert!(!report
            .diagnostics
            .get("first/create.yml")
            .is_some_and(|features| features.contains_key(feature)));
    }
}

#[test]
fn secret_values_are_absent_from_outputs_and_report() {
    let destination = tempfile::tempdir().unwrap();
    let report = import_bruno_v1(
        &fixture(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let report = serde_json::to_string(&report).unwrap();
    for forbidden in ["must-not-be-exported", "synthetic-sensitive-value"] {
        assert!(!report.contains(forbidden));
    }
    assert!(report.contains("Client_Secret"));

    for entry in walkdir::WalkDir::new(destination.path()) {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            for forbidden in ["must-not-be-exported", "synthetic-sensitive-value"] {
                assert!(!text.contains(forbidden), "{}", entry.path().display());
            }
        }
    }
    let environment =
        std::fs::read_to_string(destination.path().join("environments/fallback.json")).unwrap();
    assert!(!environment.contains("api_token"));
    assert!(!environment.contains("Client_Secret"));
}

#[test]
fn output_is_byte_deterministic() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_report =
        import_bruno_v1(&fixture(), first.path(), BrunoV1ImportOptions::default()).unwrap();
    let second_report =
        import_bruno_v1(&fixture(), second.path(), BrunoV1ImportOptions::default()).unwrap();

    assert_eq!(
        serde_json::to_vec(&first_report).unwrap(),
        serde_json::to_vec(&second_report).unwrap()
    );
    for entry in walkdir::WalkDir::new(first.path()) {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            let relative = entry.path().strip_prefix(first.path()).unwrap();
            assert_eq!(
                std::fs::read(entry.path()).unwrap(),
                std::fs::read(second.path().join(relative)).unwrap(),
                "{}",
                relative.display()
            );
        }
    }
}

#[test]
fn imports_contracts_as_named_native_schema_assertions_and_preserves_bundle_bytes() {
    let destination = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let report = import_bruno_v1(
        &contract_fixture(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let second_report = import_bruno_v1(
        &contract_fixture(),
        second.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();

    assert_eq!(report.scanned_request_count, 5);
    assert_eq!(report.imported_request_count, 5);
    assert_eq!(report.direct_native_contract_assertion_count, 1);
    assert_eq!(report.transformed_native_contract_assertion_count, 1);
    assert_eq!(report.blocked_contract_assertion_count, 2);
    assert_eq!(report.blocked_assertion_script_count, 1);
    assert!(
        report.requires_project_code,
        "the unrelated unsafe script remains project code"
    );
    assert_eq!(
        std::fs::read(contract_fixture().join("shared/contracts/bp2.bundled.json")).unwrap(),
        std::fs::read(
            destination
                .path()
                .join("assets/schemas/bruno/bp2.bundled.json")
        )
        .unwrap()
    );
    assert!(!destination
        .path()
        .join("requests/shared/ignored.request.json")
        .exists());
    assert!(!destination
        .path()
        .join("assets/schemas/bruno/validate.js")
        .exists());
    assert!(!destination
        .path()
        .join("assets/schemas/bruno/endpoint-map.json")
        .exists());
    assert_eq!(
        serde_json::to_vec(&report).unwrap(),
        serde_json::to_vec(&second_report).unwrap()
    );

    let direct = AssertionDocument::parse(
        &std::fs::read_to_string(destination.path().join("requests/direct.assertions.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(direct.assertions.len(), 2);
    let contract = direct
        .assertions
        .iter()
        .find(|assertion| assertion.uses == "builtin:assert-schema@1")
        .unwrap();
    assert_eq!(
        contract.with["name"],
        "response matches the compact contract"
    );
    assert_eq!(contract.with["definition"], "response");
    assert_eq!(
        contract.with["schemaRef"],
        "../assets/schemas/bruno/bp2.bundled.json"
    );
    assert_eq!(
        std::fs::read(destination.path().join("requests/direct.assertions.json")).unwrap(),
        std::fs::read(second.path().join("requests/direct.assertions.json")).unwrap()
    );

    let transformed = AssertionDocument::parse(
        &std::fs::read_to_string(
            destination
                .path()
                .join("requests/transformed.assertions.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        transformed
            .assertions
            .iter()
            .find(|assertion| assertion.uses == "builtin:assert-schema@1")
            .unwrap()
            .with["instancePatch"],
        json!([{"op": "add", "path": "/data", "value": []}])
    );
    assert!(report.diagnostics["unknown.yml"]["contract-validation"][0]
        .contains("unknown contract schema key"));
    assert!(
        report.diagnostics["unsupported.yml"]["contract-validation"][0]
            .contains("unsupported validateContract response expression")
    );
    let unsupported = AssertionDocument::parse(
        &std::fs::read_to_string(
            destination
                .path()
                .join("requests/unsupported.assertions.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(unsupported.assertions.len(), 2);
    assert!(unsupported.assertions.iter().any(|assertion| {
        assertion.uses == "builtin:assert-status@1"
            && assertion.with["name"] == "returns 200 despite blocked contract"
    }));
    assert!(unsupported.assertions.iter().any(|assertion| {
        assertion.uses == "builtin:assert-schema@1"
            && assertion.with["name"] == "nested response transform is blocked"
            && assertion.with["schema"] == false
    }));
    assert!(report.diagnostics["unsafe.yml"]["assertion-scripts"][0].contains("blocked"));
    let unsafe_asset = std::fs::read_to_string(
        destination
            .path()
            .join("assets/bruno/assertions/unsafe.tests-1.js"),
    )
    .unwrap();
    assert!(!unsafe_asset.contains("require"));
    assert!(!unsafe_asset.contains("readFileSync"));
}

#[test]
fn imported_contract_bundle_validates_refs_formats_and_unknown_keys_fail_closed() {
    let destination = tempfile::tempdir().unwrap();
    import_bruno_v1(
        &contract_fixture(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let request_path = destination.path().join("requests/direct.request.json");
    let document = reqv1::load_request_document(&request_path).unwrap();
    let project = reqv1::load_project(destination.path()).unwrap();
    let resolver = reqv1::RefResolver::new(destination.path(), &project).unwrap();
    let store = reqv1::DataStore::new(&resolver);
    let empty = json!({});
    let no_secrets = |_name: &str| None;
    let inputs = reqv1::BuildInputs {
        resolver: &resolver,
        store: &store,
        base_dir: request_path.parent().unwrap(),
        env: empty.clone(),
        matrix: empty.clone(),
        runtime: empty,
        secret: &no_secrets,
    };
    let ir = reqv1::build_ir(&document, &inputs).unwrap();
    let assertion = ir
        .pipeline
        .iter()
        .find(|entry| entry.asset.address == "assert-schema")
        .unwrap();
    assert!(assertion.input.get("schemaRef").is_none());
    assert!(assertion.input["schema"]["$defs"]["identifier"].is_object());

    let response = |body: serde_json::Value| reqv1::ResponseView {
        status: 200,
        headers: vec![],
        body: serde_json::to_vec(&body).unwrap(),
        time_ms: 1,
    };
    let valid = response(json!({
        "item": {"id": 7},
        "createdAt": "2026-07-28T10:00:00Z",
        "contact": "user@buecher.example"
    }));
    assert!(reqv1::run_after_response(assertion, &valid).unwrap().0[0].passed);
    for invalid in [
        json!({"item": {"id": "7"}, "createdAt": "2026-07-28T10:00:00Z", "contact": "user@example.test"}),
        json!({"item": {"id": 7}, "createdAt": "invalid", "contact": "not-an-email"}),
        json!({"item": {"id": 7, "extra": true}, "createdAt": "2026-07-28T10:00:00Z", "contact": "user@example.test"}),
    ] {
        assert!(
            !reqv1::run_after_response(assertion, &response(invalid))
                .unwrap()
                .0[0]
                .passed
        );
    }

    let mut unknown = document;
    let schema = unknown
        .pipeline
        .iter_mut()
        .find(|entry| entry.uses == "builtin:assert-schema@1")
        .unwrap();
    schema
        .with
        .insert("definition".to_string(), json!("not_present"));
    let error = reqv1::build_ir(&unknown, &inputs).unwrap_err();
    assert!(error.0.iter().any(|diagnostic| diagnostic
        .message
        .contains("unknown JSON Schema definition")));

    let transformed_path = destination.path().join("requests/transformed.request.json");
    let transformed = reqv1::load_request_document(&transformed_path).unwrap();
    let transformed_inputs = reqv1::BuildInputs {
        base_dir: transformed_path.parent().unwrap(),
        ..inputs
    };
    let transformed_ir = reqv1::build_ir(&transformed, &transformed_inputs).unwrap();
    let transformed_assertion = transformed_ir
        .pipeline
        .iter()
        .find(|entry| entry.asset.address == "assert-schema")
        .unwrap();
    let dirty_page = response(json!({"data": [{"id": "legacy-invalid"}]}));
    assert!(
        reqv1::run_after_response(transformed_assertion, &dirty_page)
            .unwrap()
            .0[0]
            .passed
    );
}

#[test]
fn emits_project_relative_service_and_leaf_sequences() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    let leaf = source.path().join("services/internal/accounts/scenario");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::write(
        leaf.join("folder.yml"),
        "info:\n  name: Scenario\n  type: folder\n  seq: 1\n",
    )
    .unwrap();
    std::fs::write(
        leaf.join("get.yml"),
        "info:\n  name: Get\n  type: http\n  seq: 1\nhttp:\n  method: get\n  url: https://example.test\n",
    )
    .unwrap();

    import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let leaf_sequence = SequenceDocument::parse(
        &std::fs::read_to_string(
            destination
                .path()
                .join("sequences/services/internal/accounts/scenario/leaf.sequence.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let service_sequence = SequenceDocument::parse(
        &std::fs::read_to_string(
            destination
                .path()
                .join("sequences/services/internal/accounts/service.sequence.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let expected = ["requests/services/internal/accounts/scenario/get.request.json"];
    assert_eq!(leaf_sequence.requests, expected);
    assert_eq!(service_sequence.requests, expected);
}

#[test]
fn encode_url_false_is_materialized_for_structured_query_rows() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    std::fs::write(
        source.path().join("raw.yml"),
        "info:\n  name: Raw\n  type: http\nhttp:\n  method: get\n  url: https://example.test/items\n  params:\n    - name: filter\n      value: a b\n      type: query\nsettings:\n  encodeUrl: false\n",
    )
    .unwrap();

    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let imported = request(&destination.path().join("requests/raw.request.json"));
    assert_eq!(imported.request.settings.encode_url, Some(false));
    assert_eq!(imported.request.query[0].value, "a b");
    assert!(!report
        .diagnostics
        .get("raw.yml")
        .is_some_and(|features| features.contains_key("settings.encodeUrl")));
}

#[test]
fn lowers_static_xml_copy_script_to_native_multipart_asset() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    std::fs::create_dir_all(source.path().join("fixtures")).unwrap();
    std::fs::write(
        source.path().join("fixtures/upload-payload.xml"),
        "<request>sanitized</request>",
    )
    .unwrap();
    std::fs::write(
        source.path().join("upload.yml"),
        r#"info:
  name: CRID upload
  type: http
http:
  method: post
  url: https://example.test/upload
  headers:
    - name: Content-Type
      value: multipart/form-data; boundary=stale-source-boundary
runtime:
  scripts:
    - type: before-request
      code: |
        const source = path.join(process.cwd(), "fixtures/upload-payload.xml");
        const tmp = path.join(os.tmpdir(), "request.xml");
        fs.copyFileSync(source, tmp);
        req.setBody([{ name: 'document', type: 'file', value: [tmp] }]);
"#,
    )
    .unwrap();

    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let imported = request(&destination.path().join("requests/upload.request.json"));
    assert!(imported.request.headers.is_empty());
    let forge_core::reqv1::model::BodySpec::Multipart(body) = imported.request.body.unwrap() else {
        panic!("multipart body")
    };
    let forge_core::reqv1::model::MultipartPart::File { name, file, .. } = &body.parts[0] else {
        panic!("file part")
    };
    assert_eq!(name, "document");
    assert_eq!(file, "../assets/bruno/fixtures/upload-payload.xml");
    assert_eq!(
        std::fs::read_to_string(
            destination
                .path()
                .join("assets/bruno/fixtures/upload-payload.xml")
        )
        .unwrap(),
        "<request>sanitized</request>"
    );
    assert!(!report
        .diagnostics
        .get("upload.yml")
        .is_some_and(|features| features.contains_key("scripts")));
}

#[test]
fn lowers_runtime_xml_fixture_and_cleanup_to_native_multipart() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    write_source(
        source.path(),
        "scenario/fixtures/payload.xml",
        "<request>runtime</request>",
    );
    write_source(
        source.path(),
        "scenario/01_prepare.yml",
        r#"info:
  name: Prepare upload
  type: http
  seq: 1
http:
  method: get
  url: https://example.test/prepare
runtime:
  scripts:
    - type: after-response
      code: |
        const source = path.join(process.cwd(), "fixtures/payload.xml");
        const uploadPath = path.join(os.tmpdir(), "payload.xml");
        fs.copyFileSync(source, uploadPath);
        bru.setVar("upload_path", uploadPath);
"#,
    );
    write_source(
        source.path(),
        "scenario/02_upload.yml",
        r#"info:
  name: Upload
  type: http
  seq: 2
http:
  method: post
  url: https://example.test/upload
runtime:
  scripts:
    - type: before-request
      code: |
        const uploadPath = bru.getVar("upload_path");
        req.setBody([{ name: "document", type: "file", value: [uploadPath] }], { raw: true });
    - type: after-response
      code: |
        const uploadPath = bru.getVar("upload_path");
        if (uploadPath) { try { require("fs").unlinkSync(uploadPath); } catch (error) {} }
"#,
    );

    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let imported = request(
        &destination
            .path()
            .join("requests/scenario/02_upload.request.json"),
    );
    let forge_core::reqv1::model::BodySpec::Multipart(body) = imported.request.body.unwrap() else {
        panic!("multipart body")
    };
    let forge_core::reqv1::model::MultipartPart::File { file, .. } = &body.parts[0] else {
        panic!("file part")
    };
    assert_eq!(file, "../../assets/bruno/scenario/fixtures/payload.xml");
    assert_eq!(report.quarantined_script_count, 1);
    let quarantine = forge_core::convert::load_import_quarantine(destination.path())
        .unwrap()
        .unwrap();
    assert_eq!(quarantine.entries.len(), 1);
    assert_eq!(quarantine.entries[0].source_path, "scenario/01_prepare.yml");
}

#[test]
fn lowers_inline_static_xml_to_generated_asset() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    write_source(
        source.path(),
        "inline.yml",
        r#"info:
  name: Inline XML
  type: http
http:
  method: post
  url: https://example.test/upload
runtime:
  scripts:
    - type: before-request
      code: |
        const output = path.join(os.tmpdir(), "generated-payload");
        fs.writeFileSync(output, `<request>inline</request>`);
        req.setBody([{ name: "document", type: "file", value: [output] }], { raw: true });
"#,
    );

    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let imported = request(&destination.path().join("requests/inline.request.json"));
    let forge_core::reqv1::model::BodySpec::Multipart(body) = imported.request.body.unwrap() else {
        panic!("multipart body")
    };
    let forge_core::reqv1::model::MultipartPart::File { file, .. } = &body.parts[0] else {
        panic!("file part")
    };
    assert_eq!(file, "../assets/bruno/generated/inline.xml");
    assert_eq!(
        std::fs::read_to_string(destination.path().join("assets/bruno/generated/inline.xml"))
            .unwrap(),
        "<request>inline</request>"
    );
    assert_eq!(report.quarantined_script_count, 0);
    assert!(!destination
        .path()
        .join("imports/quarantine/manifest.json")
        .exists());
}

#[test]
fn materializes_native_binary_body_file() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    std::fs::write(source.path().join("payload.bin"), [0_u8, 1, 2, 255]).unwrap();
    std::fs::write(
        source.path().join("binary.yml"),
        "info:\n  name: Binary\n  type: http\nhttp:\n  method: post\n  url: https://example.test/upload\n  body:\n    type: file\n    data: payload.bin\n    contentType: application/x-test\n",
    )
    .unwrap();

    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let imported = request(&destination.path().join("requests/binary.request.json"));
    let forge_core::reqv1::model::BodySpec::Binary(body) = imported.request.body.unwrap() else {
        panic!("binary body")
    };
    assert_eq!(body.file, "../assets/bruno/payload.bin");
    assert_eq!(body.content_type.as_deref(), Some("application/x-test"));
    assert_eq!(
        std::fs::read(destination.path().join("assets/bruno/payload.bin")).unwrap(),
        [0_u8, 1, 2, 255]
    );
    assert!(!report
        .diagnostics
        .get("binary.yml")
        .is_some_and(|features| features.contains_key("body")));
}

#[test]
fn lowers_native_skips_folder_inheritance_conditions_and_fixed_delays() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    std::fs::create_dir_all(source.path().join("environments")).unwrap();
    std::fs::write(
        source.path().join("environments/local.yml"),
        "name: local\nvariables:\n  - name: restricted_access_token\n    value: redacted\n    secret: true\n  - name: kc_client_id_no_bpcr\n    value: redacted\n    secret: true\n  - name: tax_openapi_url\n    value: https://example.test\n",
    )
    .unwrap();
    let optional = source.path().join("optional");
    std::fs::create_dir_all(optional.join("nested")).unwrap();
    std::fs::write(
        optional.join("folder.yml"),
        r#"info:
  name: Optional
  type: folder
runtime:
  scripts:
    - type: before-request
      code: |
        if (!bru.getEnvVar('tax_openapi_url')) {
          bru.runner.skipRequest();
        }
"#,
    )
    .unwrap();
    std::fs::write(
        optional.join("guarded.yml"),
        r#"info:
  name: Guarded
  type: http
http:
  method: get
  url: '{{tax_openapi_url}}/guarded'
runtime:
  scripts:
    - type: before-request
      code: |
        const token = bru.getEnvVar("restricted_access_token");
        const client = bru.getEnvVar("kc_client_id_no_bpcr");
        if ((!token && client) || bru.getVar('deleted_bp_id')) {
          bru.runner.skipRequest();
        }
"#,
    )
    .unwrap();
    std::fs::write(
        optional.join("nested/folder.yml"),
        "info:\n  name: Nested\n  type: folder\n",
    )
    .unwrap();
    std::fs::write(
        optional.join("nested/inherited.yml"),
        "info:\n  name: Inherited\n  type: http\nhttp:\n  method: get\n  url: https://example.test/inherited\n",
    )
    .unwrap();
    std::fs::write(
        source.path().join("unconditional.yml"),
        "info:\n  name: Unconditional\n  type: http\nhttp:\n  method: get\n  url: https://example.test\nruntime:\n  scripts:\n    - type: before-request\n      code: bru.runner.skipRequest();\n",
    )
    .unwrap();
    std::fs::write(
        source.path().join("skip-with-extra.yml"),
        "info:\n  name: Extra\n  type: http\nhttp:\n  method: get\n  url: https://example.test\nruntime:\n  scripts:\n    - type: before-request\n      code: |\n        console.log('review me');\n        bru.runner.skipRequest();\n",
    )
    .unwrap();
    for duration in [5000_u64, 30000, 60000, 90000] {
        std::fs::write(
            source.path().join(format!("delay-{duration}.yml")),
            format!(
                "info:\n  name: Delay {duration}\n  type: http\nhttp:\n  method: get\n  url: https://example.test\nruntime:\n  scripts:\n    - type: before-request\n      code: await new Promise(r => setTimeout(r, {duration}));\n"
            ),
        )
        .unwrap();
    }

    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(report.native_skip_request_count, 3);
    assert_eq!(report.native_delay_request_count, 4);
    assert_eq!(report.blocked_before_request_script_count, 1);

    let guarded = request(
        &destination
            .path()
            .join("requests/optional/guarded.request.json"),
    );
    let guard = serde_json::to_string(&guarded.execution.unwrap().skip.unwrap().when).unwrap();
    for expected in [
        r#""scope":"env","name":"tax_openapi_url""#,
        r#""scope":"secret","name":"restricted_access_token""#,
        r#""scope":"secret","name":"kc_client_id_no_bpcr""#,
        r#""scope":"runtime","name":"deleted_bp_id""#,
    ] {
        assert!(guard.contains(expected), "{guard}");
    }
    let inherited = request(
        &destination
            .path()
            .join("requests/optional/nested/inherited.request.json"),
    );
    assert!(inherited.execution.unwrap().skip.is_some());
    assert!(!report
        .diagnostics
        .get("optional/folder.yml")
        .is_some_and(|features| features.contains_key("scripts")));

    let unconditional = request(
        &destination
            .path()
            .join("requests/unconditional.request.json"),
    );
    assert!(serde_json::to_string(&unconditional.execution.unwrap())
        .unwrap()
        .contains(r#""literal":true"#));
    assert!(report.diagnostics["skip-with-extra.yml"].contains_key("before-request-scripts"));
    for duration in [5000_u64, 30000, 60000, 90000] {
        let delayed = request(
            &destination
                .path()
                .join(format!("requests/delay-{duration}.request.json")),
        );
        assert_eq!(delayed.execution.unwrap().delay_before_ms, Some(duration));
        assert!(!report
            .diagnostics
            .get(&format!("delay-{duration}.yml"))
            .is_some_and(|features| features.contains_key("scripts")));
    }
}

#[test]
fn preserves_named_tests_with_native_and_compatible_assertions() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let second_destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    std::fs::write(
        source.path().join("native.yml"),
        r#"info:
  name: Native tests
  type: http
http:
  method: get
  url: https://example.test/native
runtime:
  scripts:
    - type: tests
      code: |
        test("returns created", () => {
          expect(res.status).to.eq(201);
        });
        test('has nested id', function () {
          expect(res.body.user.id).to.deep.equal("u-1");
        });
"#,
    )
    .unwrap();
    std::fs::write(
        source.path().join("custom.yml"),
        r#"info:
  name: Compatible tests
  type: http
http:
  method: post
  url: https://example.test/custom
  body:
    type: json
    data:
      operation: create
runtime:
  scripts:
    - type: tests
      code: |
        test("nested arrays and negation", () => {
          expect(res.body).to.have.nested.property("user.profile.name", "Ada");
          expect(res.body.user.roles).to.include("admin");
          expect(res.body.user.roles).to.not.contain("blocked");
        });
        test("request and variables", () => {
          expect(req.body.operation).to.equal("create");
          expect(bru.getVar("run_id")).to.equal("run-7");
          expect(bru.getEnvVar("region")).to.equal("test");
          expect(bru.getCollectionVar("tenant")).to.equal("acme");
        });
        test("failure is recorded", () => {
          expect(res.body.user.roles).to.have.lengthOf(99);
        });
        test("later test still runs", () => {
          expect([1, 2, 3]).to.have.members([3, 2, 1]);
          expect("apiwright").to.match(/^api/);
        });
"#,
    )
    .unwrap();

    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(report.native_assertion_script_count, 1);
    assert_eq!(report.custom_assertion_script_count, 1);
    assert_eq!(report.blocked_assertion_script_count, 0);
    assert!(report.requires_project_code);

    let native = AssertionDocument::parse(
        &std::fs::read_to_string(destination.path().join("requests/native.assertions.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(native.assertions.len(), 2);
    assert_eq!(native.assertions[0].with["name"], "returns created");
    assert_eq!(native.assertions[1].with["name"], "has nested id");

    let asset = destination
        .path()
        .join("assets/bruno/assertions/custom.tests-1.js");
    let asset_text = std::fs::read_to_string(&asset).unwrap();
    assert!(asset_text.contains("function run(ctx, input)"));
    assert!(asset_text.contains("test(\"nested arrays and negation\""));
    assert!(!asset_text.contains("eval("));
    import_bruno_v1(
        source.path(),
        second_destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    for relative in [
        "assets/bruno/compat.js",
        "assets/bruno/assertions/custom.tests-1.js",
    ] {
        assert_eq!(
            std::fs::read(destination.path().join(relative)).unwrap(),
            std::fs::read(second_destination.path().join(relative)).unwrap()
        );
    }
    let output = forge_core::reqv1::jshost::run_js_asset(
        &asset.to_string_lossy(),
        &json!({
            "request": {"body": {"operation": "create"}},
            "response": {
                "status": 200,
                "headers": [],
                "body": {"user": {"profile": {"name": "Ada"}, "roles": ["admin", "writer"]}}
            },
            "runtime": {"run_id": "run-7"},
            "environment": {"region": "test"},
            "bindings": {"tenant": "acme"}
        }),
        &json!({"compatibility": "bruno-tests-v1"}),
    )
    .unwrap();
    let results = output.as_array().unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(
        results
            .iter()
            .map(|result| result["passed"].as_bool())
            .collect::<Vec<_>>(),
        [Some(true), Some(true), Some(false), Some(true)]
    );
    assert!(results[2]["message"].as_str().unwrap().contains("length"));
}

#[test]
fn preserves_inherited_hook_order_xml_base64_and_conditional_extraction() {
    use base64::prelude::{Engine as _, BASE64_STANDARD};

    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let second_destination = tempfile::tempdir().unwrap();
    std::fs::write(
        source.path().join("opencollection.yml"),
        r#"opencollection: 1.0.0
info:
  name: Compat
vars:
  pre-request:
    - name: collection_xml
      value: |-
        <collection>
          <id>7</id>
        </collection>
runtime:
  scripts:
    - type: before-request
      code: |
        bru.setVar("order", (bru.getVar("order") || "") + "root");
        req.setHeader("X-Root", "yes");
"#,
    )
    .unwrap();
    let folder = source.path().join("service");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("folder.yml"),
        r#"info:
  name: Service
  type: folder
runtime:
  scripts:
    - type: before-request
      code: |
        bru.setVar("order", bru.getVar("order") + "-folder");
        req.body.inherited = bru.getCollectionVar("collection_xml");
"#,
    )
    .unwrap();
    std::fs::write(
        folder.join("create.yml"),
        r#"info:
  name: Create
  type: http
http:
  method: post
  url: https://example.test/create
  body:
    type: json
    data:
      initial: true
runtime:
  scripts:
    - type: before-request
      code: |
        req.setBody({
          encoded: btoa(bru.getCollectionVar("collection_xml")),
          order: bru.getVar("order")
        });
        req.setHeader("X-Request", "ok");
        bru.setEnvVar("ephemeral", "runtime-only");
    - type: after-response
      code: |
        if (res.status === 201 && res.body.access_token) {
          bru.setVar("access_token", res.body.access_token.toUpperCase());
        }
"#,
    )
    .unwrap();

    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(report.custom_before_request_script_count, 3);
    assert_eq!(report.blocked_before_request_script_count, 0);
    assert_eq!(report.custom_after_response_script_count, 1);
    assert!(report.requires_project_code);
    let second_report = import_bruno_v1(
        source.path(),
        second_destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_vec(&report).unwrap(),
        serde_json::to_vec(&second_report).unwrap()
    );
    for relative in [
        "assets/bruno/compat.js",
        "assets/bruno/hooks/opencollection.before-1.js",
        "assets/bruno/hooks/service/folder.before-1.js",
        "assets/bruno/hooks/service/create.before-1.js",
        "assets/bruno/extractors/service/create.after-2.js",
    ] {
        assert_eq!(
            std::fs::read(destination.path().join(relative)).unwrap(),
            std::fs::read(second_destination.path().join(relative)).unwrap(),
            "{relative}"
        );
    }

    let request_path = destination
        .path()
        .join("requests/service/create.request.json");
    let imported = request(&request_path);
    assert_eq!(
        serde_json::to_value(&imported.bindings["collection_xml"]).unwrap()["value"],
        "<collection>\n  <id>7</id>\n</collection>"
    );
    let hooks = HookDocument::parse(
        &std::fs::read_to_string(
            destination
                .path()
                .join("requests/service/create.hooks.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(hooks.hooks.len(), 4);
    assert!(hooks.hooks[0].uses.ends_with("opencollection.before-1.js"));
    assert!(hooks.hooks[1].uses.ends_with("service/folder.before-1.js"));
    assert!(hooks.hooks[2].uses.ends_with("service/create.before-1.js"));
    assert!(hooks.hooks[3].uses.ends_with("service/create.after-2.js"));

    let resolve_asset = |uses: &str| {
        request_path
            .parent()
            .unwrap()
            .join(uses)
            .to_string_lossy()
            .into_owned()
    };
    let xml = "<collection>\n  <id>7</id>\n</collection>";
    let before = forge_core::reqv1::jshost::run_js_asset(
        &resolve_asset(&hooks.hooks[2].uses),
        &json!({
            "request": {"body": {"initial": true}, "bodyType": "json", "headers": []},
            "response": null,
            "bindings": {"collection_xml": xml},
            "environment": {},
            "runtime": {"order": "root-folder"}
        }),
        &json!({"compatibility": "bruno-scripts-v1"}),
    )
    .unwrap();
    assert_eq!(before["body"]["type"], "json");
    assert_eq!(
        before["body"]["value"]["encoded"],
        BASE64_STANDARD.encode(xml)
    );
    assert_eq!(before["body"]["value"]["order"], "root-folder");
    assert_eq!(
        before["headers"][0],
        json!({"name": "X-Request", "value": "ok"})
    );
    assert_eq!(before["runtime"]["ephemeral"], "runtime-only");

    let after_path = resolve_asset(&hooks.hooks[3].uses);
    let after = forge_core::reqv1::jshost::run_js_asset(
        &after_path,
        &json!({
            "request": {"body": null, "bodyType": "none", "headers": []},
            "response": {"status": 201, "headers": [], "body": {"access_token": "abc"}},
            "bindings": {}, "environment": {}, "runtime": {}
        }),
        &json!({"compatibility": "bruno-scripts-v1"}),
    )
    .unwrap();
    assert_eq!(after["runtime"]["access_token"], "ABC");
    let not_extracted = forge_core::reqv1::jshost::run_js_asset(
        &after_path,
        &json!({
            "request": {"body": null, "bodyType": "none", "headers": []},
            "response": {"status": 400, "headers": [], "body": {"access_token": "abc"}},
            "bindings": {}, "environment": {}, "runtime": {}
        }),
        &json!({"compatibility": "bruno-scripts-v1"}),
    )
    .unwrap();
    assert_eq!(not_extracted["runtime"], json!({}));
}

#[tokio::test]
async fn imported_before_runtime_writes_mutate_request_and_carry_through_sequence() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/first"))
        .and(header("x-script", "yes"))
        .and(body_json(json!({"value": "one"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "response-sensitive"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/second"))
        .and(header("x-carry", "one-two"))
        .and(header("x-token", "response-sensitive"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    std::fs::write(
        source.path().join("first.yml"),
        format!(
            r#"info:
  name: First
  type: http
  seq: 1
http:
  method: post
  url: {}/first
  body:
    type: json
    data: {{}}
runtime:
  scripts:
    - type: before-request
      code: |
        req.body.value = "one";
        req.setHeader("X-Script", "yes");
        bru.setVar("carry", "one");
    - type: before-request
      code: |
        bru.setEnvVar("carry", bru.getVar("carry") + "-two");
    - type: after-response
      code: |
        if (res.status === 200 && res.body.access_token) {{
          bru.setVar("alias", res.body.access_token);
        }}
"#,
            server.uri()
        ),
    )
    .unwrap();
    std::fs::write(
        source.path().join("second.yml"),
        format!(
            r#"info:
  name: Second
  type: http
  seq: 2
http:
  method: get
  url: {}/second
runtime:
  scripts:
    - type: before-request
      code: |
        req.setHeader("X-Carry", bru.getVar("carry"));
        req.setHeader("X-Token", bru.getVar("alias"));
"#,
            server.uri()
        ),
    )
    .unwrap();
    import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let files = [
        destination.path().join("requests/first.request.json"),
        destination.path().join("requests/second.request.json"),
    ];
    let no_secrets = |_: &str| None;
    let engine = HttpEngine::new();
    let results = reqv1::run_sequence(
        &files,
        destination.path(),
        json!({}),
        &no_secrets,
        &engine,
        RunMode::Http,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        results[0].status,
        RunStatus::Passed,
        "{:?}",
        results[0].diagnostics
    );
    assert_eq!(results[0].runtime["carry"], "one-two");
    assert_eq!(results[0].runtime["alias"], "***");
    assert!(results[0]
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "PIPELINE_CONFLICT"));
    assert_eq!(
        results[1].status,
        RunStatus::Passed,
        "{:?}",
        results[1].diagnostics
    );
}

#[test]
fn assertion_assets_are_deterministic_and_unsafe_scripts_fail_closed() {
    let source = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    std::fs::write(
        source.path().join("unsafe.yml"),
        r#"info:
  name: Unsafe tests
  type: http
http:
  method: get
  url: https://example.test
runtime:
  scripts:
    - type: tests
      code: |
        const fs = require("fs");
        eval("1 + 1");
        test("must not run", async () => fetch("https://example.invalid"));
    - type: before-request
      code: |
        const fs = require("fs");
        req.setBody([{ type: "file", value: ["/tmp/not-allowed"] }]);
    - type: after-response
      code: |
        async function extract() {
          return axios.get("https://example.invalid");
        }
"#,
    )
    .unwrap();

    let first_report =
        import_bruno_v1(source.path(), first.path(), BrunoV1ImportOptions::default()).unwrap();
    let second_report = import_bruno_v1(
        source.path(),
        second.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(first_report.blocked_assertion_script_count, 1);
    assert_eq!(first_report.custom_assertion_script_count, 0);
    assert_eq!(first_report.blocked_before_request_script_count, 1);
    assert_eq!(first_report.blocked_after_response_script_count, 1);
    assert_eq!(first_report.quarantined_script_count, 3);
    assert!(first_report.requires_project_code);
    assert!(first_report.diagnostics["unsafe.yml"]["assertion-scripts"][0].contains("blocked"));
    assert_eq!(
        serde_json::to_vec(&first_report).unwrap(),
        serde_json::to_vec(&second_report).unwrap()
    );
    let quarantine = forge_core::convert::load_import_quarantine(first.path())
        .unwrap()
        .expect("blocked scripts create import quarantine");
    assert_eq!(quarantine.entries.len(), 3);
    assert!(quarantine
        .entries
        .iter()
        .any(|entry| entry.script.contains(r#"require("fs")"#)));
    assert!(quarantine
        .entries
        .iter()
        .any(|entry| { entry.category == forge_core::convert::QuarantineCategory::BeforeRequest }));
    assert!(quarantine
        .entries
        .iter()
        .any(|entry| { entry.category == forge_core::convert::QuarantineCategory::AfterResponse }));
    assert_eq!(
        std::fs::read(first.path().join("imports/quarantine/manifest.json")).unwrap(),
        std::fs::read(second.path().join("imports/quarantine/manifest.json")).unwrap()
    );

    let relative = Path::new("assets/bruno/assertions/unsafe.tests-1.js");
    let first_asset = std::fs::read(first.path().join(relative)).unwrap();
    assert_eq!(
        first_asset,
        std::fs::read(second.path().join(relative)).unwrap()
    );
    let asset_text = String::from_utf8(first_asset).unwrap();
    for unsafe_source in [
        r#"require("fs")"#,
        r#"eval("1 + 1")"#,
        r#"fetch("https://example.invalid")"#,
    ] {
        assert!(!asset_text.contains(unsafe_source));
    }
    let output = forge_core::reqv1::jshost::run_js_asset(
        &first.path().join(relative).to_string_lossy(),
        &json!({}),
        &json!({"compatibility": "bruno-tests-v1"}),
    )
    .unwrap();
    assert_eq!(output[0]["passed"], false);
    assert!(output[0]["message"]
        .as_str()
        .unwrap()
        .contains("unsafe.yml"));

    let hooks = HookDocument::parse(
        &std::fs::read_to_string(first.path().join("requests/unsafe.hooks.json")).unwrap(),
    )
    .unwrap();
    let request_dir = first.path().join("requests");
    let before = request_dir.join(&hooks.hooks[0].uses);
    let error = forge_core::reqv1::jshost::run_js_asset(
        &before.to_string_lossy(),
        &json!({}),
        &json!({"compatibility": "bruno-scripts-v1"}),
    )
    .unwrap_err();
    assert!(error.message.contains("unsafe.yml"));
    let after = request_dir.join(&hooks.hooks[1].uses);
    let output = forge_core::reqv1::jshost::run_js_asset(
        &after.to_string_lossy(),
        &json!({}),
        &json!({"compatibility": "bruno-scripts-v1"}),
    )
    .unwrap();
    assert_eq!(output[0]["passed"], false);
    assert!(output[0]["message"]
        .as_str()
        .unwrap()
        .contains("unsafe.yml"));
}

#[test]
fn reimport_preserves_reviews_and_removes_stale_quarantine() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    write_source(
        source.path(),
        "request.yml",
        r#"info:
  name: Request
  type: http
http:
  method: get
  url: https://example.test
runtime:
  scripts:
    - type: before-request
      code: fetch("https://example.invalid");
"#,
    );
    import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let mut manifest = forge_core::convert::load_import_quarantine(destination.path())
        .unwrap()
        .unwrap();
    manifest.entries[0].script = "// reviewed locally".to_string();
    manifest.entries[0].comment = "Replace with a native helper".to_string();
    forge_core::convert::save_import_quarantine(destination.path(), &manifest).unwrap();

    import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    let manifest = forge_core::convert::load_import_quarantine(destination.path())
        .unwrap()
        .unwrap();
    assert_eq!(manifest.entries[0].script, "// reviewed locally");
    assert_eq!(manifest.entries[0].comment, "Replace with a native helper");

    write_source(
        source.path(),
        "request.yml",
        "info:\n  name: Request\n  type: http\nhttp:\n  method: get\n  url: https://example.test\n",
    );
    import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    assert!(
        forge_core::convert::load_import_quarantine(destination.path())
            .unwrap()
            .is_none()
    );
}

#[test]
fn classic_bruno_detection_and_direct_import_remain_available() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("bruno.json"), r#"{"name":"Classic"}"#).unwrap();
    std::fs::write(
        source.path().join("get.bru"),
        "meta {\n  name: Get\n  type: http\n  seq: 1\n}\n\nget {\n  url: https://example.test\n  body: none\n  auth: none\n}\n",
    )
    .unwrap();

    assert_eq!(
        detect_bruno_source(source.path()).unwrap(),
        BrunoSourceFormat::ClassicBru
    );
    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(report.imported_request_count, 1);
    assert!(destination
        .path()
        .join("requests/get.request.json")
        .is_file());
}

#[test]
fn malformed_request_yaml_is_diagnostic_and_missing_marker_is_rejected() {
    let malformed = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(malformed.path());
    std::fs::write(malformed.path().join("broken.yml"), "info: [\n").unwrap();
    let report = import_bruno_v1(
        malformed.path(),
        destination.path(),
        BrunoV1ImportOptions {
            inspect: true,
            ..BrunoV1ImportOptions::default()
        },
    )
    .unwrap();
    assert!(report.diagnostics["broken.yml"].contains_key("yaml"));

    let missing = tempfile::tempdir().unwrap();
    assert!(matches!(
        detect_bruno_source(missing.path()),
        Err(BrunoV1ImportError::NotACollection(_))
    ));
}

#[test]
fn refuses_overlapping_source_and_destination() {
    let source = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    let error = import_bruno_v1(
        source.path(),
        source.path(),
        BrunoV1ImportOptions {
            inspect: true,
            ..BrunoV1ImportOptions::default()
        },
    )
    .unwrap_err();
    assert!(matches!(error, BrunoV1ImportError::Materialize(_)));
}

#[test]
fn validates_all_output_paths_before_writing() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_minimal_collection(source.path());
    for file in ["same.yml", "same.yaml"] {
        std::fs::write(
            source.path().join(file),
            "info:\n  name: Same\n  type: http\nhttp:\n  method: get\n  url: https://example.test\n",
        )
        .unwrap();
    }
    let error = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(error, BrunoV1ImportError::Materialize(_)));
    assert!(!destination.path().join("project.json").exists());
}

#[test]
fn migrates_sanitized_keycloak_azure_and_ipd5_auth_patterns() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_source(
        source.path(),
        "opencollection.yml",
        "opencollection: 1.0.0\ninfo:\n  name: Auth corpus\nrequest:\n  auth:\n    type: bearer\n    token: \"{{access_token}}\"\n",
    );
    for role in [
        "bp2",
        "crid-admin",
        "crid-standard",
        "crid-mlc",
        "crid-read",
        "ipd5",
    ] {
        write_source(
            source.path(),
            &format!("environments/{role}.yml"),
            "variables:\n  - name: keycloak_url\n    value: http://localhost/token\n  - name: azure_tenant_id\n    value: tenant\n  - name: azure_client_id\n    value: client\n  - name: azure_scope\n    value: scope\n  - name: urlIPD5\n    value: http://localhost\n  - name: userIPD5\n    value: user\n  - name: keycloak_client_secret\n    secret: true\n  - name: azure_client_secret\n    secret: true\n  - name: passwordIPD5\n    secret: true\n  - name: kc_client_secret_no_bpcr\n    secret: true\n  - name: kc_client_id_no_bpcr\n    value: restricted\n",
        );
    }
    write_source(
        source.path(),
        "services/internal/token.yml",
        "info:\n  name: Token\n  type: http\nhttp:\n  method: post\n  url: \"{{keycloak_url}}\"\n  body:\n    type: form-urlencoded\n    data:\n      - name: grant_type\n        value: client_credentials\n      - name: client_id\n        value: team-bp-tester\n      - name: client_secret\n        value: \"{{keycloak_client_secret}}\"\n  auth:\n    type: none\n",
    );
    write_source(
        source.path(),
        "services/internal/restricted-token.yml",
        "info:\n  name: Restricted token\n  type: http\nhttp:\n  method: post\n  url: \"{{keycloak_url}}\"\n  body:\n    type: form-urlencoded\n    data:\n      - name: grant_type\n        value: client_credentials\n      - name: client_id\n        value: \"{{kc_client_id_no_bpcr}}\"\n      - name: client_secret\n        value: \"{{kc_client_secret_no_bpcr}}\"\n  auth:\n    type: none\n",
    );
    write_source(
        source.path(),
        "services/internal/restricted.yml",
        "info:\n  name: Restricted\n  type: http\nhttp:\n  method: get\n  url: http://localhost/restricted\n  auth:\n    type: bearer\n    token: \"{{restricted_access_token}}\"\n",
    );
    write_source(
        source.path(),
        "services/internal/BusinessPartner/folder.yml",
        "info:\n  name: BP\n  type: folder\nruntime:\n  scripts:\n    - type: before-request\n      code: |\n        const tokenUrl = bru.getEnvVar('keycloak_url');\n        const clientSecret = bru.getEnvVar('keycloak_client_secret');\n        const clientId = 'team-bp-tester';\n        const body = 'grant_type=client_credentials';\n        bru.sendRequest({ url: tokenUrl, data: body }, () => {});\n        bru.setVar('access_token', clientSecret);\n",
    );
    write_source(
        source.path(),
        "services/internal/BusinessPartner/get.yml",
        "info:\n  name: BP\n  type: http\nhttp:\n  method: get\n  url: http://localhost/bp\n",
    );
    write_source(
        source.path(),
        "services/external/CRID/folder.yml",
        "info:\n  name: CRID\n  type: folder\nrequest:\n  auth:\n    type: bearer\n    token: \"{{crid_access_token}}\"\nruntime:\n  scripts:\n    - type: before-request\n      code: |\n        const tenant = bru.getEnvVar('azure_tenant_id');\n        const id = bru.getEnvVar('azure_client_id');\n        const secret = bru.getEnvVar('azure_client_secret');\n        const scope = bru.getEnvVar('azure_scope');\n        const url = 'https://login.microsoftonline.com/' + tenant;\n        const body = 'grant_type=client_credentials';\n        axios.post(url, body).then(resp => bru.setVar('crid_access_token', resp.data.access_token));\n",
    );
    write_source(
        source.path(),
        "services/external/CRID/get.yml",
        "info:\n  name: CRID\n  type: http\nhttp:\n  method: get\n  url: http://localhost/crid\n",
    );
    write_source(
        source.path(),
        "services/external/IPD5/folder.yml",
        "info:\n  name: IPD5\n  type: folder\nrequest:\n  auth:\n    type: bearer\n    token: \"{{token}}\"\n",
    );
    write_source(
        source.path(),
        "services/external/IPD5/ApiToken/GenerateToken.yml",
        "info:\n  name: GenerateToken\n  type: http\nhttp:\n  method: post\n  url: \"{{urlIPD5}}/ApiToken/GenerateToken\"\n  body:\n    type: json\n    data: \"{{body}}\"\n  auth:\n    type: none\n",
    );
    write_source(
        source.path(),
        "services/external/IPD5/get.yml",
        "info:\n  name: IPD5 get\n  type: http\nhttp:\n  method: get\n  url: \"{{urlIPD5}}/get\"\n",
    );
    write_source(
        source.path(),
        "services/internal/unknown.yml",
        "info:\n  name: Unknown network auth\n  type: http\nhttp:\n  method: get\n  url: http://localhost/unknown\nruntime:\n  scripts:\n    - type: before-request\n      code: |\n        const token = bru.getVar('access_token');\n        axios.get('http://localhost/admin', { headers: { Authorization: token } });\n",
    );

    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(report.imported_request_count, 8);
    assert_eq!(report.environments.len(), 6);
    assert_eq!(report.generated_auth_provider_count, 4);
    assert_eq!(report.generated_helper_request_count, 0);
    assert_eq!(report.recognized_auth_script_count, 2);
    assert_eq!(report.recognized_data_manager_auth_count, 0);
    assert_eq!(report.remaining_blocked_auth_script_count, 1);
    let project = reqv1::load_project(destination.path()).unwrap();
    assert_eq!(project.auth_providers.len(), 4);
    assert!(destination
        .path()
        .join("requests/auth/bruno/keycloak-client-credentials.request.json")
        .is_file());
    assert!(destination
        .path()
        .join("requests/services/internal/token.request.json")
        .is_file());

    let public = request(
        &destination
            .path()
            .join("requests/services/internal/BusinessPartner/get.request.json"),
    );
    assert_eq!(public.auth.unwrap().as_str(), "bruno-keycloak");
    let crid = request(
        &destination
            .path()
            .join("requests/services/external/CRID/get.request.json"),
    );
    assert_eq!(crid.auth.unwrap().as_str(), "bruno-crid-azure");
    let ipd5 = request(
        &destination
            .path()
            .join("requests/services/external/IPD5/get.request.json"),
    );
    assert_eq!(ipd5.auth.unwrap().as_str(), "bruno-ipd5");
    let restricted = request(
        &destination
            .path()
            .join("requests/services/internal/restricted.request.json"),
    );
    assert_eq!(
        restricted.auth.unwrap().as_str(),
        "bruno-keycloak-restricted"
    );
    let generated = std::fs::read_to_string(
        destination
            .path()
            .join("requests/auth/bruno/ipd5-token.request.json"),
    )
    .unwrap();
    assert!(generated.contains("${secret.passwordIPD5}"));
    assert!(!generated.contains("client-secret-value"));
}

#[tokio::test]
async fn lowers_and_runs_sanitized_ipd452_admin_discovery_with_masking() {
    let server = MockServer::start().await;
    let token_path = "/realms/gvl/protocol/openid-connect/token";
    let payload = BASE64_URL_SAFE_NO_PAD
        .encode(br#"{"resource_access":{"leg-contracts":{"roles":["c:r","c-m:r"]}}}"#);
    let service_token = format!("e30.{payload}.signature");
    Mock::given(method("POST"))
        .and(path(token_path))
        .and(body_string(
            "grant_type=client_credentials&client_id=team-bp-tester&client_secret=admin-secret",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "admin-token"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/admin/realms/gvl/clients"))
        .and(query_param("clientId", "ipd-upload-data-manager"))
        .and(header("authorization", "Bearer admin-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": "client-uuid"}])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/admin/realms/gvl/clients/client-uuid/client-secret"))
        .and(header("authorization", "Bearer admin-token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"value": "discovered-secret"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(token_path))
        .and(body_string(
            "grant_type=client_credentials&client_id=ipd-upload-data-manager&client_secret=discovered-secret",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": service_token.clone()})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/actuator/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "git": {"commit": {"id": "58798f3"}}
        })))
        .expect(1)
        .mount(&server)
        .await;
    for endpoint in ["/contracts", "/mandates"] {
        let authorization = format!("Bearer {service_token}");
        Mock::given(method("GET"))
            .and(path(endpoint))
            .and(header("authorization", authorization.as_str()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
    }

    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    write_ipd452_collection(
        source.path(),
        &format!("{}{token_path}", server.uri()),
        &server.uri(),
    );
    let report = import_bruno_v1(
        source.path(),
        destination.path(),
        BrunoV1ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(report.imported_request_count, 3);
    assert_eq!(report.generated_helper_request_count, 3);
    assert_eq!(report.recognized_auth_script_count, 1);
    assert_eq!(report.recognized_data_manager_auth_count, 1);
    assert_eq!(report.remaining_blocked_auth_script_count, 0);
    assert_eq!(report.custom_before_request_script_count, 0);

    let environment = reqv1::load_environment(destination.path(), Some("local")).unwrap();
    assert_eq!(
        environment["keycloak_admin_url"],
        format!("{}/admin/realms/gvl", server.uri())
    );
    assert_eq!(
        environment["keycloak_http_token_url"],
        format!("{}{token_path}", server.uri())
    );

    let leaf = SequenceDocument::parse(
        &std::fs::read_to_string(destination.path().join(
            "sequences/services/internal/datamanager/ipd-452_contracts2_oauth2/leaf.sequence.json",
        ))
        .unwrap(),
    )
    .unwrap();
    let helper_prefix = "requests/services/internal/DataManager/IPD-452_Contracts2_OAuth2/_helpers";
    assert_eq!(
        leaf.requests,
        [
            format!("{helper_prefix}/04c-01-keycloak-client.request.json"),
            format!("{helper_prefix}/04c-02-keycloak-client-secret.request.json"),
            format!("{helper_prefix}/04c-03-service-token.request.json"),
            "requests/services/internal/DataManager/IPD-452_Contracts2_OAuth2/04c_ipd_service_token_und_deployment_pruefen.request.json".to_string(),
            "requests/services/internal/DataManager/IPD-452_Contracts2_OAuth2/04d_contract.request.json".to_string(),
            "requests/services/internal/DataManager/IPD-452_Contracts2_OAuth2/04e_mandate.request.json".to_string(),
        ]
    );
    let service = SequenceDocument::parse(
        &std::fs::read_to_string(
            destination
                .path()
                .join("sequences/services/internal/datamanager/service.sequence.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(service.requests, leaf.requests);
    let helper_auth = leaf.requests[..3]
        .iter()
        .map(|relative| {
            request(&destination.path().join(relative))
                .auth
                .unwrap()
                .as_str()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(helper_auth, ["bruno-keycloak", "bruno-keycloak", "none"]);

    let files = leaf
        .requests
        .iter()
        .map(|relative| destination.path().join(relative))
        .collect::<Vec<_>>();
    let secrets =
        |name: &str| (name == "keycloak_client_secret").then(|| "admin-secret".to_string());
    let results = reqv1::run_sequence_with_responses(
        &files,
        destination.path(),
        environment,
        &secrets,
        &HttpEngine::new(),
        RunMode::Http,
        CancellationToken::new(),
    )
    .await;
    assert!(results
        .iter()
        .all(|(result, _)| result.status == RunStatus::Passed));
    assert_eq!(results[1].0.runtime["ipd452_client_secret"], "***");
    assert_eq!(results[2].0.runtime["ipd452_service_token"], "***");
    assert_eq!(results[3].0.assertions.len(), 2);
    assert!(results[3]
        .0
        .assertions
        .iter()
        .all(|assertion| assertion.passed));
    let mut public =
        serde_json::to_string(&results.iter().map(|(result, _)| result).collect::<Vec<_>>())
            .unwrap();
    for (_, response) in &results {
        if let Some(response) = response {
            public.push_str(&String::from_utf8_lossy(&response.body));
        }
    }
    assert!(!public.contains("admin-secret"));
    assert!(!public.contains("admin-token"));
    assert!(!public.contains("discovered-secret"));
    assert!(!public.contains(&service_token));
}

#[tokio::test]
async fn ipd452_helpers_fail_closed_when_client_or_secret_is_missing() {
    for (clients, secret_body, failed_step) in [
        (json!([]), json!({"value": "unused"}), 0),
        (json!([{"id": "client-uuid"}]), json!({}), 1),
    ] {
        let server = MockServer::start().await;
        let token_path = "/realms/gvl/protocol/openid-connect/token";
        Mock::given(method("POST"))
            .and(path(token_path))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"access_token": "admin-token"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/admin/realms/gvl/clients"))
            .respond_with(ResponseTemplate::new(200).set_body_json(clients))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/admin/realms/gvl/clients/client-uuid/client-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(secret_body))
            .mount(&server)
            .await;

        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        write_ipd452_collection(
            source.path(),
            &format!("{}{token_path}", server.uri()),
            &server.uri(),
        );
        import_bruno_v1(
            source.path(),
            destination.path(),
            BrunoV1ImportOptions::default(),
        )
        .unwrap();
        let leaf = SequenceDocument::parse(
            &std::fs::read_to_string(destination.path().join(
                "sequences/services/internal/datamanager/ipd-452_contracts2_oauth2/leaf.sequence.json",
            ))
            .unwrap(),
        )
        .unwrap();
        let files = leaf.requests[..3]
            .iter()
            .map(|relative| destination.path().join(relative))
            .collect::<Vec<_>>();
        let environment = reqv1::load_environment(destination.path(), Some("local")).unwrap();
        let secrets =
            |name: &str| (name == "keycloak_client_secret").then(|| "admin-secret".to_string());
        let results = reqv1::run_sequence(
            &files,
            destination.path(),
            environment,
            &secrets,
            &HttpEngine::new(),
            RunMode::Http,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(results[failed_step].status, RunStatus::Error);
        assert!(results[failed_step]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("JSONPath")));
        let public = serde_json::to_string(&results).unwrap();
        assert!(!public.contains("admin-secret"));
    }
}

#[cfg(unix)]
#[test]
fn refuses_existing_destination_symlink_escape_before_writing() {
    use std::os::unix::fs::symlink;

    let source = fixture();
    let destination = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), destination.path().join("requests")).unwrap();
    let error =
        import_bruno_v1(&source, destination.path(), BrunoV1ImportOptions::default()).unwrap_err();
    assert!(matches!(error, BrunoV1ImportError::Materialize(_)));
    assert!(!destination.path().join("project.json").exists());
}

#[test]
#[ignore = "requires APIWRIGHT_BRUNO_CORPUS"]
fn full_corpus_classification() {
    let source = std::env::var_os("APIWRIGHT_BRUNO_CORPUS")
        .map(PathBuf::from)
        .expect("set APIWRIGHT_BRUNO_CORPUS");
    let destination = tempfile::tempdir().unwrap().path().join("inspect-only");
    let report = import_bruno_v1(
        &source,
        &destination,
        BrunoV1ImportOptions {
            inspect: true,
            exclude_underscore_dirs: true,
        },
    )
    .unwrap();

    assert_eq!(report.scanned_request_count, 1_037);
    assert_eq!(
        report.excluded_by_policy["extensions.bruno.ignore:00_deactivated"],
        8
    );
    assert_eq!(report.excluded_by_policy["underscore-directory"], 12);
    assert_eq!(report.excluded_request_count, 20);
    assert_eq!(report.imported_request_count, 1_017);
    assert_eq!(report.generated_auth_provider_count, 4);
    assert_eq!(report.generated_helper_request_count, 3);
    assert_eq!(report.recognized_auth_script_count, 80);
    assert_eq!(report.recognized_data_manager_auth_count, 1);
    assert_eq!(report.remaining_blocked_auth_script_count, 0);
    assert_eq!(report.native_skip_request_count, 140);
    assert_eq!(report.native_delay_request_count, 31);
    assert_eq!(
        report.native_assertion_script_count
            + report.custom_assertion_script_count
            + report.blocked_assertion_script_count,
        970
    );
    assert_eq!(report.direct_native_contract_assertion_count, 65);
    assert_eq!(report.transformed_native_contract_assertion_count, 1);
    assert_eq!(report.blocked_contract_assertion_count, 0);
    assert!(!destination.exists());
}
