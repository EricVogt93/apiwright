use forge_core::exec::HttpEngine;
use forge_core::reqv1::{self, AuthSession, RequestDocument, RunMode, RunStatus};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn pre_cancelled_mock_runs_do_not_resolve_secrets_or_execute_cases() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("project.json"), r#"{"formatVersion":1}"#).unwrap();
    let path = root.path().join("first.request.json");
    let doc = json!({
        "formatVersion":1,"kind":"request","meta":{"id":"first","name":"First"},
        "matrix":{"case":{"value":[1,2]}},
        "request":{"method":"GET","url":"https://example.test/${secret.TEST_SECRET}"},
        "mock":{"status":200,"headers":[],"body":{"type":"text","value":"ok"}}
    });
    std::fs::write(&path, doc.to_string()).unwrap();
    let doc = RequestDocument::parse(&doc.to_string()).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let secret = |_: &str| -> Option<String> { panic!("cancelled runs must not resolve secrets") };
    let engine = HttpEngine::new();
    let auth = AuthSession::default();
    let result = reqv1::run_matrix_with_responses_in_session(
        &doc,
        root.path(),
        &path,
        json!({}),
        &secret,
        &engine,
        RunMode::Mock,
        cancel.clone(),
        &auth,
    )
    .await;
    let error = result.unwrap_err();
    assert!(error.to_string().contains("cancelled"));

    let results = reqv1::run_sequence_with_environment_values_in_session(
        &[path.clone(), path],
        root.path(),
        &[json!({}), json!({})],
        &secret,
        &engine,
        RunMode::Mock,
        cancel,
        &auth,
    )
    .await
    .unwrap();
    assert_eq!(results.len(), 1, "sequence must stop before later requests");
    assert_eq!(results[0].0.status, RunStatus::Error);
    assert!(results[0]
        .0
        .diagnostics
        .iter()
        .any(|d| d.message.contains("cancelled")));
    assert!(results[0].1.is_none());
}
