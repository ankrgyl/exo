//! Wiremock-driven tests for the E2B sandbox backend.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use exoharness::{
    E2bConfig, E2bSandboxBackend, ManagedSandboxBackend, SandboxCommand, SandboxKey,
    SandboxLifecycleConfig, SandboxMount, SandboxMountAccess, SandboxNetworkPolicy, SandboxRequest,
    SandboxSpec, SnapshotFormat, SnapshotPayload,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

fn make_request(thread_id: &str, sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            thread_id: thread_id.into(),
            sandbox_id: sandbox_id.into(),
        },
        spec: SandboxSpec {
            image: "base".into(),
            resources: Default::default(),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            network: SandboxNetworkPolicy::Enabled,
            default_workdir: "/home/user".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

fn backend_for_mock(server: &MockServer) -> E2bSandboxBackend {
    E2bSandboxBackend::new(E2bConfig {
        api_key: "test-api-key".into(),
        api_url: server.uri(),
        template_id: "base".into(),
        envd_port: 49_983,
        envd_base_url: Some(server.uri()),
        secure: false,
    })
    .expect("E2bSandboxBackend::new")
}

fn sandbox_created_json(id: &str) -> Value {
    json!({
        "sandboxID": id,
        "templateID": "base",
        "envdVersion": "0.1.0",
    })
}

async fn mount_empty_sandbox_list(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v2/sandboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(2)
        .mount(server)
        .await;
}

fn listed_sandbox_json(id: &str, state: &str) -> Value {
    json!({
        "sandboxID": id,
        "templateID": "base",
        "state": state,
        "startedAt": "2026-06-01T12:00:00Z",
        "clientID": "client",
        "cpuCount": 2,
        "memoryMB": 512,
        "diskSizeMB": 1024,
        "endAt": "2026-06-01T13:00:00Z",
        "envdVersion": "0.1.0",
    })
}

#[tokio::test]
async fn acquire_posts_to_sandboxes_with_metadata() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_empty_sandbox_list(&server).await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sandbox_created_json("sb-fresh")))
        .expect(1)
        .mount(&server)
        .await;

    backend
        .acquire(make_request("conv-1", "sandbox-1"))
        .await
        .expect("acquire should succeed");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = requests
        .iter()
        .find(|r| r.method.as_str() == "POST" && r.url.path() == "/sandboxes")
        .expect("create request");
    let body: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(body.get("templateID").and_then(Value::as_str), Some("base"));
    let metadata = body
        .get("metadata")
        .and_then(Value::as_object)
        .expect("metadata present");
    assert!(metadata.contains_key("exo.sandbox.key"));
    assert!(metadata.contains_key("exo.sandbox.spec-hash"));
}

#[tokio::test]
async fn acquire_rejects_host_mounts() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    let mut request = make_request("conv-2", "sandbox-2");
    request.spec.mounts.push(SandboxMount {
        host_path: PathBuf::from("/tmp/foo"),
        guest_path: "/workspace".into(),
        access: SandboxMountAccess::ReadWrite,
        internal: false,
    });

    let error = match backend.acquire(request).await {
        Ok(_) => panic!("acquire should reject host mounts"),
        Err(error) => error,
    };
    let msg = format!("{error:#}").to_lowercase();
    assert!(
        msg.contains("mount") || msg.contains("e2b"),
        "unexpected: {msg}"
    );
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        0
    );
}

#[tokio::test]
async fn acquire_reuses_running_sandbox_without_connect() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("GET"))
        .and(path("/v2/sandboxes"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([listed_sandbox_json("sb-running", "running"),])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-3", "sandbox-3"))
        .await
        .expect("acquire should reuse running sandbox");

    assert_eq!(handle.id(), "e2b:thread:conv-3:sandbox-3");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests.iter().any(|r| r.url.path().contains("/connect")),
        "running sandbox must not call connect"
    );
}

#[tokio::test]
async fn acquire_connects_paused_sandbox() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("GET"))
        .and(path("/v2/sandboxes"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([listed_sandbox_json("sb-paused", "paused"),])),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandboxes/sb-paused/connect"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sandbox_created_json("sb-paused")))
        .expect(1)
        .mount(&server)
        .await;

    backend
        .acquire(make_request("conv-4", "sandbox-4"))
        .await
        .expect("acquire should connect paused sandbox");
}

#[tokio::test]
async fn acquire_list_metadata_query_is_not_double_url_encoded() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("GET"))
        .and(path("/v2/sandboxes"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([listed_sandbox_json("sb-keyed", "running"),])),
        )
        .expect(1)
        .mount(&server)
        .await;

    backend
        .acquire(make_request("conv-colons", "sandbox-colons"))
        .await
        .expect("acquire should find sandbox by metadata");

    let requests = server.received_requests().await.unwrap_or_default();
    let query = requests[0]
        .url
        .query()
        .expect("list request has query string");
    assert!(
        !query.contains("%253A"),
        "metadata filter must not double-encode ':' in sandbox keys; got {query}"
    );
    assert!(
        query.contains("thread%3Aconv-colons%3Asandbox-colons")
            || query.contains("thread:conv-colons:sandbox-colons"),
        "expected sandbox key in metadata query; got {query}"
    );
    assert!(
        query.contains("state=running%2Cpaused") || query.contains("state=running,paused"),
        "expected comma-separated state filter; got {query}"
    );
}

#[tokio::test]
async fn acquire_creates_when_metadata_list_is_empty() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("GET"))
        .and(path("/v2/sandboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sandbox_created_json("sb-fresh")))
        .expect(1)
        .mount(&server)
        .await;

    backend
        .acquire(make_request("conv-5", "sandbox-5"))
        .await
        .expect("acquire should create when no metadata match");
}

#[tokio::test]
async fn stop_calls_pause_not_delete() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_empty_sandbox_list(&server).await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sandbox_created_json("sb-stop")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandboxes/sb-stop/pause"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-6", "sandbox-6"))
        .await
        .unwrap();
    handle.stop().await.expect("stop should pause");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests
            .iter()
            .any(|r| r.method.to_string().to_uppercase() == "DELETE")
    );
}

#[tokio::test]
async fn snapshot_returns_e2b_snapshot_payload() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_empty_sandbox_list(&server).await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sandbox_created_json("sb-snap")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandboxes/sb-snap/snapshots"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "snapshotID": "team/exo-snap-test:default",
            "names": ["team/exo-snap-test:default"],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-7", "sandbox-7"))
        .await
        .unwrap();
    let payload = handle.snapshot().await.expect("snapshot ok");

    assert_eq!(payload.format, SnapshotFormat::E2bRef);
    let manifest: Value = serde_json::from_slice(&payload.bytes).unwrap();
    assert_eq!(
        manifest.get("snapshot_id").and_then(Value::as_str),
        Some("team/exo-snap-test:default")
    );
}

#[tokio::test]
async fn acquire_from_snapshot_uses_snapshot_template_id() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sandbox_created_json("sb-restored")))
        .expect(1)
        .mount(&server)
        .await;

    let manifest = json!({
        "snapshot_id": "team/exo-snap-canonical:default",
        "base_template": "base",
    });
    let payload = SnapshotPayload {
        format: SnapshotFormat::E2bRef,
        bytes: Bytes::from(serde_json::to_vec(&manifest).unwrap()),
    };

    backend
        .acquire_from_snapshot(make_request("conv-8", "sandbox-8"), payload)
        .await
        .expect("restore ok");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = requests
        .iter()
        .find(|r| r.url.path() == "/sandboxes" && r.method.as_str() == "POST")
        .expect("create called");
    let body: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(
        body.get("templateID").and_then(Value::as_str),
        Some("team/exo-snap-canonical:default")
    );
}

#[tokio::test]
async fn acquire_from_snapshot_rejects_wrong_format() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    let payload = SnapshotPayload {
        format: SnapshotFormat::DockerImageTar,
        bytes: Bytes::from_static(b"\x00"),
    };
    let error = match backend
        .acquire_from_snapshot(make_request("conv-9", "sandbox-9"), payload)
        .await
    {
        Ok(_) => panic!("expected format mismatch error"),
        Err(error) => error,
    };
    let msg = format!("{error:#}").to_lowercase();
    assert!(msg.contains("e2b") || msg.contains("format"));
}

#[tokio::test]
async fn exec_uses_envd_process_start() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_empty_sandbox_list(&server).await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sandbox_created_json("sb-exec")))
        .mount(&server)
        .await;

    let connect_stream = connect_enveloped_stream(&[
        (0, json!({"event": {"data": {"stdout": "hello from mock"}}})),
        (2, json!({"event": {"end": {"status": "exit status 0"}}})),
    ]);

    Mock::given(method("POST"))
        .and(path("/process.Process/Start"))
        .and(exec_start_request_with_stdin(false))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(connect_stream, "application/connect+json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-10", "sandbox-10"))
        .await
        .unwrap();
    let output = handle
        .exec(&exoharness::SandboxCommand {
            argv: vec!["/bin/echo".into(), "hello".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("exec ok");

    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "hello from mock");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/process.Process/Start"),
        "exec must hit envd process start"
    );
}

#[tokio::test]
async fn start_process_streams_envd_process_stdout() {
    use futures::io::AsyncReadExt;

    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_empty_sandbox_list(&server).await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sandbox_created_json("sb-stream")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/process.Process/Start"))
        .and(exec_start_request_with_stdin(true))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            connect_enveloped_stream(&[
                (0, json!({"event": {"start": {"pid": 42}}})),
                (0, json!({"event": {"data": {"stdout": "streamed-line\n"}}})),
                (0, json!({"event": {"end": {"status": "exit status 0"}}})),
            ]),
            "application/connect+json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-stream", "sandbox-stream"))
        .await
        .unwrap();
    let mut process = handle
        .start_process(&SandboxCommand {
            argv: vec!["/bin/echo".into(), "ignored".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("start_process should stream envd output");

    let mut stdout = String::new();
    process
        .stdout
        .read_to_string(&mut stdout)
        .await
        .expect("read streamed stdout");
    let exit_code = process.wait.await.expect("wait for process exit");

    assert_eq!(stdout, "streamed-line\n");
    assert_eq!(exit_code, 0);
}

fn exec_start_request_with_stdin(enabled: bool) -> impl Match {
    struct HasStdin {
        enabled: bool,
    }
    impl Match for HasStdin {
        fn matches(&self, request: &Request) -> bool {
            let body = String::from_utf8_lossy(&request.body);
            if self.enabled {
                body.contains("\"stdin\":true")
            } else {
                body.contains("\"stdin\":false")
            }
        }
    }
    HasStdin { enabled }
}

/// Build a Connect server-stream body (5-byte framed JSON messages).
fn connect_enveloped_stream(messages: &[(u8, Value)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (flags, value) in messages {
        let payload = serde_json::to_vec(value).expect("json");
        let len = payload.len() as u32;
        out.push(*flags);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&payload);
    }
    out
}
