use std::{process::Stdio, time::Duration};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
};

async fn send(stdin: &mut ChildStdin, message: Value) {
    stdin
        .write_all(format!("{message}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
}

async fn response(reader: &mut BufReader<ChildStdout>, id: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert_ne!(
                reader.read_line(&mut line).await.unwrap(),
                0,
                "MCP server closed stdout before response {id}"
            );
            let message: Value = serde_json::from_str(line.trim()).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for MCP response {id}"))
}

#[tokio::test]
async fn binary_manages_a_request_v1_project_over_mcp_stdio() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("project.json"),
        r#"{"formatVersion":1}"#,
    )
    .unwrap();
    let requests = project.path().join("requests");
    std::fs::create_dir(&requests).unwrap();
    std::fs::write(
        requests.join("safe.request.json"),
        r#"{
            "formatVersion": 1,
            "kind": "request",
            "meta": {"id": "safe", "name": "Safe mock"},
            "request": {"method": "GET", "url": "https://example.test"},
            "mock": {"status": 200, "headers": [], "body": {"type": "text", "value": "ok"}}
        }"#,
    )
    .unwrap();
    std::fs::write(
        requests.join("blocked.request.json"),
        r#"{
            "formatVersion": 1,
            "kind": "request",
            "meta": {"id": "blocked", "name": "Blocked project code"},
            "matrix": {"case": {"use": "project:generators/untrusted"}},
            "request": {"method": "GET", "url": "https://example.test"},
            "mock": {"status": 200, "headers": [], "body": {"type": "text", "value": "ok"}}
        }"#,
    )
    .unwrap();
    std::fs::write(
        requests.join("second.request.json"),
        r#"{
            "formatVersion": 1,
            "kind": "request",
            "meta": {"id": "second", "name": "Second mock"},
            "request": {"method": "GET", "url": "https://example.test/second"},
            "mock": {"status": 200, "headers": [], "body": {"type": "text", "value": "ok"}}
        }"#,
    )
    .unwrap();
    let secret_dir = project.path().join("assets/data");
    std::fs::create_dir_all(&secret_dir).unwrap();
    std::fs::write(
        secret_dir.join("synthetic.secrets.json"),
        r#"{"token":"synthetic-test-secret-value"}"#,
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_apiwright"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "apiwright-integration-test", "version": "1"}
            }
        }),
    )
    .await;
    let initialized = response(&mut stdout, 1).await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "apiwright");

    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let listed = response(&mut stdout, 2).await;
    let mut names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "add_assertion",
            "delete_project_file",
            "delete_request",
            "inspect_project",
            "read_project_code",
            "read_project_file",
            "read_request",
            "remove_assertion",
            "run_request",
            "run_sequence",
            "validate_request",
            "write_project_file",
            "write_request",
        ]
    );
    assert!(listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool["outputSchema"]["type"] == "object"));

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "inspect_project",
                "arguments": {"root": project.path()}
            }
        }),
    )
    .await;
    let inspected = response(&mut stdout, 3).await;
    assert!(inspected.get("error").is_none(), "{inspected}");
    assert_ne!(inspected["result"]["isError"], true, "{inspected}");
    let request_paths = inspected["result"]["structuredContent"]["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|request| request["rel_path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        request_paths,
        [
            "requests/blocked.request.json",
            "requests/safe.request.json",
            "requests/second.request.json"
        ]
    );

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 17,
            "method": "tools/call",
            "params": {
                "name": "read_project_file",
                "arguments": {
                    "root": project.path(),
                    "path": "assets/data/synthetic.secrets.json",
                    "kind": "asset"
                }
            }
        }),
    )
    .await;
    let secret_read = response(&mut stdout, 17).await;
    assert_eq!(secret_read["result"]["isError"], true, "{secret_read}");
    assert!(!secret_read
        .to_string()
        .contains("synthetic-test-secret-value"));

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "run_request",
                "arguments": {
                    "root": project.path(),
                    "request": "requests/safe.request.json"
                }
            }
        }),
    )
    .await;
    let run = response(&mut stdout, 4).await;
    assert_ne!(run["result"]["isError"], true, "{run}");
    assert_eq!(run["result"]["structuredContent"]["mode"], "mock");
    assert_eq!(
        run["result"]["structuredContent"]["cases"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "run_request",
                "arguments": {
                    "root": project.path(),
                    "request": "requests/blocked.request.json"
                }
            }
        }),
    )
    .await;
    let blocked = response(&mut stdout, 5).await;
    assert_eq!(blocked["result"]["isError"], true, "{blocked}");
    assert_eq!(
        blocked["result"]["structuredContent"]["code"],
        "execution_error"
    );
    assert!(blocked["result"]["structuredContent"]["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| diagnostic["message"]
            .as_str()
            .is_some_and(|message| message.contains("project code is disabled"))));

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "write_project_file",
                "arguments": {
                    "root": project.path(),
                    "path": "environments/ci.json",
                    "kind": "environment",
                    "expectedRevision": "new",
                    "content": {"baseUrl": "https://mock.example.test"}
                }
            }
        }),
    )
    .await;
    let environment = response(&mut stdout, 6).await;
    assert_ne!(environment["result"]["isError"], true, "{environment}");
    let environment_revision = environment["result"]["structuredContent"]["revision"]
        .as_str()
        .unwrap();
    assert!(environment_revision.starts_with("sha256:"));

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "write_project_file",
                "arguments": {
                    "root": project.path(),
                    "path": "sequences/smoke.sequence.json",
                    "kind": "sequence",
                    "expectedRevision": "new",
                    "content": {
                        "formatVersion": 1,
                        "kind": "sequence",
                        "meta": {"id": "smoke", "name": "Smoke"},
                        "requests": [
                            "requests/safe.request.json",
                            "requests/second.request.json"
                        ]
                    }
                }
            }
        }),
    )
    .await;
    let sequence = response(&mut stdout, 7).await;
    assert_ne!(sequence["result"]["isError"], true, "{sequence}");
    let sequence_revision = sequence["result"]["structuredContent"]["revision"]
        .as_str()
        .unwrap();

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "run_sequence",
                "arguments": {
                    "root": project.path(),
                    "sequence": "sequences/smoke.sequence.json"
                }
            }
        }),
    )
    .await;
    let sequence_run = response(&mut stdout, 8).await;
    assert_ne!(sequence_run["result"]["isError"], true, "{sequence_run}");
    assert_eq!(
        sequence_run["result"]["structuredContent"]["cases"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 15,
            "method": "tools/call",
            "params": {
                "name": "read_request",
                "arguments": {
                    "root": project.path(),
                    "request": "requests/safe.request.json"
                }
            }
        }),
    )
    .await;
    let referenced_request = response(&mut stdout, 15).await;
    let referenced_revision = referenced_request["result"]["structuredContent"]["revision"]
        .as_str()
        .unwrap();
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 16,
            "method": "tools/call",
            "params": {
                "name": "delete_request",
                "arguments": {
                    "root": project.path(),
                    "request": "requests/safe.request.json",
                    "expectedRevision": referenced_revision
                }
            }
        }),
    )
    .await;
    let protected_request = response(&mut stdout, 16).await;
    assert_eq!(
        protected_request["result"]["isError"], true,
        "{protected_request}"
    );
    assert!(requests.join("safe.request.json").exists());

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "delete_project_file",
                "arguments": {
                    "root": project.path(),
                    "path": "sequences/smoke.sequence.json",
                    "kind": "sequence",
                    "expectedRevision": sequence_revision
                }
            }
        }),
    )
    .await;
    let sequence_deleted = response(&mut stdout, 9).await;
    assert_ne!(
        sequence_deleted["result"]["isError"], true,
        "{sequence_deleted}"
    );

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "read_request",
                "arguments": {
                    "root": project.path(),
                    "request": "requests/safe.request.json"
                }
            }
        }),
    )
    .await;
    let request = response(&mut stdout, 10).await;
    let request_revision = request["result"]["structuredContent"]["revision"]
        .as_str()
        .unwrap();

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": {
            "name": "add_assertion",
            "arguments": {
                "root": project.path(),
                "request": "requests/safe.request.json",
                "expectedRevision": request_revision,
                "assertion": {
                    "use": "builtin:assert-status@1",
                    "with": {"expected": 200},
                    "enabled": true
                },
            }
            }
        }),
    )
    .await;
    let assertion_added = response(&mut stdout, 13).await;
    assert_ne!(
        assertion_added["result"]["isError"], true,
        "{assertion_added}"
    );
    assert_eq!(assertion_added["result"]["structuredContent"]["index"], 0);
    let request_revision = assertion_added["result"]["structuredContent"]["revision"]
        .as_str()
        .unwrap();

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 14,
            "method": "tools/call",
            "params": {
            "name": "remove_assertion",
            "arguments": {
                "root": project.path(),
                "request": "requests/safe.request.json",
                "expectedRevision": request_revision,
                "index": 0
            }
            }
        }),
    )
    .await;
    let assertion_removed = response(&mut stdout, 14).await;
    assert_ne!(
        assertion_removed["result"]["isError"], true,
        "{assertion_removed}"
    );
    assert_eq!(
        assertion_removed["result"]["structuredContent"]["remaining"],
        0
    );
    assert!(!requests.join("safe.assertions.json").exists());
    let request_revision = assertion_removed["result"]["structuredContent"]["revision"]
        .as_str()
        .unwrap();
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "delete_request",
                "arguments": {
                    "root": project.path(),
                    "request": "requests/safe.request.json",
                    "expectedRevision": request_revision
                }
            }
        }),
    )
    .await;
    let request_deleted = response(&mut stdout, 11).await;
    assert_ne!(
        request_deleted["result"]["isError"], true,
        "{request_deleted}"
    );
    assert!(!requests.join("safe.request.json").exists());

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "delete_project_file",
                "arguments": {
                    "root": project.path(),
                    "path": "environments/ci.json",
                    "kind": "environment",
                    "expectedRevision": environment_revision
                }
            }
        }),
    )
    .await;
    let environment_deleted = response(&mut stdout, 12).await;
    assert_ne!(
        environment_deleted["result"]["isError"], true,
        "{environment_deleted}"
    );

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("MCP server did not stop after stdin closed")
        .unwrap();
    assert!(status.success(), "MCP server exited with {status}");
}
