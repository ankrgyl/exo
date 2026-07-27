//! Wiremock-driven tests for the Tensorlake sandbox backend.
//!
//! The single mock server stands in for both the platform API (`/sandboxes/...`)
//! and the per-sandbox proxy (`/api/v1/...`), which the backend is pointed at via
//! `TensorlakeConfig::sandbox_base_url`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use exoharness::{
    ManagedSandboxBackend, SandboxKey, SandboxLifecycleConfig, SandboxMount, SandboxMountAccess,
    SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotKind, SnapshotPayload,
    TensorlakeConfig, TensorlakeSandboxBackend,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_request(conversation_id: &str, sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: conversation_id.into(),
            sandbox_id: sandbox_id.into(),
        },
        spec: SandboxSpec {
            image: "tensorlake/ubuntu-minimal".into(),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            network: SandboxNetworkPolicy::Enabled,
            default_workdir: "/workspace".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

/// A request with no idle TTL, which maps to an ephemeral Tensorlake sandbox.
fn make_ephemeral_request(conversation_id: &str, sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        lifecycle: SandboxLifecycleConfig { idle_ttl: None },
        ..make_request(conversation_id, sandbox_id)
    }
}

fn sandbox_spec_hash(spec: &SandboxSpec) -> String {
    let mut hasher = DefaultHasher::new();
    spec.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn expected_sandbox_name(request: &SandboxRequest) -> String {
    let mut hasher = DefaultHasher::new();
    request.key.hash(&mut hasher);
    sandbox_spec_hash(&request.spec).hash(&mut hasher);
    format!("exo-{:016x}", hasher.finish())
}

fn backend_for_mock(server: &MockServer) -> TensorlakeSandboxBackend {
    backend_with_config(TensorlakeConfig {
        api_key: "test-key".into(),
        api_url: server.uri(),
        default_image: "tensorlake/ubuntu-minimal".into(),
        cpus: None,
        memory_mb: None,
        sandbox_base_url: Some(server.uri()),
    })
}

fn backend_with_config(config: TensorlakeConfig) -> TensorlakeSandboxBackend {
    TensorlakeSandboxBackend::new(config).expect("TensorlakeSandboxBackend::new")
}

fn sandbox_info_json(server: &MockServer, id: &str, status: &str) -> Value {
    json!({
        "id": id,
        "namespace": "default",
        "status": status,
        "created_at": 1_773_950_042_728i64,
        "resources": { "cpus": 1.0, "memory_mb": 1024 },
        "timeout_secs": 300,
        "allow_unauthenticated_access": false,
        "ingress_endpoint": server.uri(),
        "sandbox_url": server.uri(),
    })
}

/// Body for `POST /sandboxes`; the backend polls `GET /sandboxes/{id}` right after.
fn create_response_json(id: &str) -> Value {
    json!({ "sandbox_id": id, "status": "pending" })
}

fn sse(frames: &[Value]) -> String {
    frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect()
}

async fn mount_running_sandbox(server: &MockServer, identifier: &str, id: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/sandboxes/{identifier}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sandbox_info_json(server, id, "running")),
        )
        .mount(server)
        .await;
}

fn last_body(requests: &[wiremock::Request], method_name: &str, path_suffix: &str) -> Value {
    let request = requests
        .iter()
        .rev()
        .find(|request| {
            request.method.as_str() == method_name && request.url.path().ends_with(path_suffix)
        })
        .unwrap_or_else(|| panic!("no {method_name} request to {path_suffix}"));
    serde_json::from_slice(&request.body).expect("request body is JSON")
}

#[tokio::test]
async fn acquire_creates_named_sandbox_when_missing() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-1", "sandbox-1");
    let name = expected_sandbox_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/sandboxes/{name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(create_response_json("sbx-1")))
        .expect(1)
        .mount(&server)
        .await;
    mount_running_sandbox(&server, "sbx-1", "sbx-1").await;

    let handle = backend.acquire(request).await.expect("acquire");
    assert_eq!(handle.id(), "tensorlake:conversation:conv-1:sandbox-1");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = last_body(&requests, "POST", "/sandboxes");
    assert_eq!(create["name"], json!(name));
    assert_eq!(create["image"], json!("tensorlake/ubuntu-minimal"));
    assert_eq!(create["timeout_secs"], json!(300));
    assert_eq!(create["network"]["allow_internet_access"], json!(true));
}

#[tokio::test]
async fn acquire_reuses_running_named_sandbox() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-warm", "sandbox-warm");
    let name = expected_sandbox_name(&request);

    mount_running_sandbox(&server, &name, "sbx-warm").await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(create_response_json("sbx-new")))
        .expect(0)
        .mount(&server)
        .await;

    backend.acquire(request).await.expect("acquire");
}

#[tokio::test]
async fn acquire_resumes_suspended_named_sandbox() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-suspended", "sandbox-suspended");
    let name = expected_sandbox_name(&request);

    // The lookup by name reports suspended; the post-resume poll (by id) is running.
    Mock::given(method("GET"))
        .and(path(format!("/sandboxes/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_info_json(
            &server,
            "sbx-suspended",
            "suspended",
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandboxes/sbx-suspended/resume"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    mount_running_sandbox(&server, "sbx-suspended", "sbx-suspended").await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(create_response_json("sbx-new")))
        .expect(0)
        .mount(&server)
        .await;

    backend.acquire(request).await.expect("acquire");
}

#[tokio::test]
async fn acquire_without_idle_ttl_creates_unnamed_sandbox() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_ephemeral_request("conv-ephemeral", "sandbox-ephemeral");

    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(create_response_json("sbx-eph")))
        .expect(1)
        .mount(&server)
        .await;
    mount_running_sandbox(&server, "sbx-eph", "sbx-eph").await;

    backend.acquire(request).await.expect("acquire");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = last_body(&requests, "POST", "/sandboxes");
    assert!(create.get("name").is_none(), "ephemeral create sent a name");
    assert!(
        create.get("timeout_secs").is_none(),
        "ephemeral create pinned a timeout"
    );
}

#[tokio::test]
async fn acquire_disables_internet_access_for_a_disabled_network_policy() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let mut request = make_request("conv-net", "sandbox-net");
    request.spec.network = SandboxNetworkPolicy::Disabled;
    let name = expected_sandbox_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/sandboxes/{name}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(create_response_json("sbx-net")))
        .mount(&server)
        .await;
    mount_running_sandbox(&server, "sbx-net", "sbx-net").await;

    backend.acquire(request).await.expect("acquire");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = last_body(&requests, "POST", "/sandboxes");
    assert_eq!(create["network"]["allow_internet_access"], json!(false));
}

#[tokio::test]
async fn acquire_forwards_configured_resource_overrides() {
    let server = MockServer::start().await;
    let backend = backend_with_config(TensorlakeConfig {
        api_key: "test-key".into(),
        api_url: server.uri(),
        default_image: "tensorlake/ubuntu-minimal".into(),
        cpus: Some(4.0),
        memory_mb: Some(8192),
        sandbox_base_url: Some(server.uri()),
    });
    let request = make_request("conv-res", "sandbox-res");
    let name = expected_sandbox_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/sandboxes/{name}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(create_response_json("sbx-res")))
        .mount(&server)
        .await;
    mount_running_sandbox(&server, "sbx-res", "sbx-res").await;

    backend.acquire(request).await.expect("acquire");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = last_body(&requests, "POST", "/sandboxes");
    assert_eq!(create["resources"]["cpus"], json!(4.0));
    assert_eq!(create["resources"]["memory_mb"], json!(8192));
}

#[tokio::test]
async fn exec_collects_run_process_sse_output() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-exec", "sandbox-exec");
    let name = expected_sandbox_name(&request);
    mount_running_sandbox(&server, &name, "sbx-exec").await;

    let stream = sse(&[
        json!({ "handle": 1, "pid": 42, "started_at": 1_710_000_000_000i64 }),
        json!({ "line": "hello", "timestamp": 1i64, "stream": "stdout" }),
        json!({ "line": "world", "timestamp": 2i64, "stream": "stdout" }),
        json!({ "line": "oops", "timestamp": 3i64, "stream": "stderr" }),
        json!({ "exit_code": 0 }),
    ]);
    Mock::given(method("POST"))
        .and(path("/api/v1/processes/run"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    let output = handle
        .exec(&exoharness::SandboxCommand {
            argv: vec!["echo".into(), "hello".into()],
            env: [("MODE".to_string(), "prod".to_string())]
                .into_iter()
                .collect(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("exec");

    assert!(output.ok);
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "hello\nworld\n");
    assert_eq!(output.stderr, "oops\n");
    assert_eq!(output.cwd, "/workspace");

    let requests = server.received_requests().await.unwrap_or_default();
    let run = last_body(&requests, "POST", "/api/v1/processes/run");
    assert_eq!(run["command"], json!("echo"));
    assert_eq!(run["args"], json!(["hello"]));
    assert_eq!(run["working_dir"], json!("/workspace"));
    assert_eq!(run["env"]["MODE"], json!("prod"));
}

#[tokio::test]
async fn exec_reports_a_non_zero_exit_code() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-fail", "sandbox-fail");
    let name = expected_sandbox_name(&request);
    mount_running_sandbox(&server, &name, "sbx-fail").await;

    let stream = sse(&[
        json!({ "line": "boom", "timestamp": 1i64, "stream": "stderr" }),
        json!({ "exit_code": 3 }),
    ]);
    Mock::given(method("POST"))
        .and(path("/api/v1/processes/run"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    let output = handle
        .exec(&exoharness::SandboxCommand {
            argv: vec!["false".into()],
            env: Default::default(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("exec");

    assert!(!output.ok);
    assert_eq!(output.exit_code, Some(3));
    assert_eq!(output.stderr, "boom\n");
}

#[tokio::test]
async fn exec_maps_a_signalled_process_to_a_shell_style_exit_code() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-signal", "sandbox-signal");
    let name = expected_sandbox_name(&request);
    mount_running_sandbox(&server, &name, "sbx-signal").await;

    let stream = sse(&[json!({ "exit_code": null, "signal": 9 })]);
    Mock::given(method("POST"))
        .and(path("/api/v1/processes/run"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    let output = handle
        .exec(&exoharness::SandboxCommand {
            argv: vec!["sleep".into(), "100".into()],
            env: Default::default(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("exec");

    assert!(!output.ok);
    assert_eq!(output.exit_code, Some(137));
}

#[tokio::test]
async fn start_process_bridges_stdin_output_and_exit() {
    use futures::AsyncReadExt as _;
    use futures::AsyncWriteExt as _;

    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-stream", "sandbox-stream");
    let name = expected_sandbox_name(&request);
    mount_running_sandbox(&server, &name, "sbx-stream").await;

    Mock::given(method("POST"))
        .and(path("/api/v1/processes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "handle": 1,
            "pid": 77,
            "status": "running",
            "stdin_writable": true,
            "command": "cat",
            "args": [],
            "started_at": 1i64,
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/processes/77/stdout/follow"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "event: output\ndata: {\"line\":\"streamed\",\"timestamp\":1}\n\nevent: eof\ndata: {}\n\n",
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/processes/77/stderr/follow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("event: eof\ndata: {}\n\n", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/processes/77/stdin"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/processes/77/stdin/close"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/processes/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "handle": 1,
            "pid": 77,
            "status": "exited",
            "exit_code": 0,
            "stdin_writable": false,
            "command": "cat",
            "args": [],
            "started_at": 1i64,
            "ended_at": 2i64,
        })))
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    let mut parts = handle
        .start_process(&exoharness::SandboxCommand {
            argv: vec!["cat".into()],
            env: Default::default(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("start_process");

    parts.stdin.write_all(b"ping\n").await.expect("write stdin");
    parts.stdin.close().await.expect("close stdin");

    let mut stdout = String::new();
    parts
        .stdout
        .read_to_string(&mut stdout)
        .await
        .expect("read stdout");
    assert_eq!(stdout, "streamed\n");
    assert_eq!(parts.wait.await.expect("wait"), 0);
}

#[tokio::test]
async fn stop_suspends_a_named_sandbox() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-stop", "sandbox-stop");
    let name = expected_sandbox_name(&request);
    mount_running_sandbox(&server, &name, "sbx-stop").await;

    Mock::given(method("POST"))
        .and(path(format!("/sandboxes/{name}/suspend")))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    handle.stop().await.expect("stop");
}

#[tokio::test]
async fn stop_deletes_an_ephemeral_sandbox() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_ephemeral_request("conv-stop-eph", "sandbox-stop-eph");

    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(create_response_json("sbx-eph")))
        .mount(&server)
        .await;
    mount_running_sandbox(&server, "sbx-eph", "sbx-eph").await;
    Mock::given(method("DELETE"))
        .and(path("/sandboxes/sbx-eph"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    handle.stop().await.expect("stop");
}

#[tokio::test]
async fn snapshot_returns_a_tensorlake_manifest() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-snap", "sandbox-snap");
    let name = expected_sandbox_name(&request);
    mount_running_sandbox(&server, &name, "sbx-snap").await;

    Mock::given(method("POST"))
        .and(path("/sandboxes/sbx-snap/snapshot"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "snapshot_id": "snap-123",
            "status": "pending",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/snapshots/snap-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "completed" })))
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    let payload = handle.snapshot().await.expect("snapshot");

    assert_eq!(payload.kind, SnapshotKind::TensorlakeSnapshot);
    let manifest: Value = serde_json::from_slice(&payload.bytes).expect("manifest json");
    assert_eq!(manifest["snapshot_id"], json!("snap-123"));
    assert_eq!(manifest["sandbox_name"], json!(name));

    let requests = server.received_requests().await.unwrap_or_default();
    let snapshot = last_body(&requests, "POST", "/snapshot");
    assert_eq!(snapshot["snapshot_type"], json!("filesystem"));
}

#[tokio::test]
async fn acquire_from_snapshot_replaces_the_named_sandbox() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-restore", "sandbox-restore");
    let name = expected_sandbox_name(&request);

    mount_running_sandbox(&server, &name, "sbx-old").await;
    Mock::given(method("DELETE"))
        .and(path(format!("/sandboxes/{name}")))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(create_response_json("sbx-restored")),
        )
        .expect(1)
        .mount(&server)
        .await;
    mount_running_sandbox(&server, "sbx-restored", "sbx-restored").await;

    let payload = SnapshotPayload {
        kind: SnapshotKind::TensorlakeSnapshot,
        bytes: Bytes::from(
            serde_json::to_vec(&json!({ "snapshot_id": "snap-9", "sandbox_name": name }))
                .expect("manifest"),
        ),
    };
    let handle = backend
        .acquire_from_snapshot(request, payload)
        .await
        .expect("acquire_from_snapshot");
    assert_eq!(
        handle.id(),
        "tensorlake-restored:conversation:conv-restore:sandbox-restore"
    );

    let requests = server.received_requests().await.unwrap_or_default();
    let create = last_body(&requests, "POST", "/sandboxes");
    assert_eq!(create["snapshot_id"], json!("snap-9"));
    assert_eq!(create["name"], json!(name));
    assert!(
        create.get("image").is_none(),
        "restore should boot from the snapshot, not an image"
    );
}

#[tokio::test]
async fn acquire_from_snapshot_rejects_foreign_payload_kinds() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-foreign", "sandbox-foreign");

    let payload = SnapshotPayload {
        kind: SnapshotKind::DockerImageTar,
        bytes: Bytes::from_static(b"not-a-tensorlake-manifest"),
    };
    let error = match backend.acquire_from_snapshot(request, payload).await {
        Ok(_) => panic!("expected a snapshot kind mismatch error"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("TensorlakeSnapshot"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn acquire_rejects_host_bind_mounts() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let mut request = make_request("conv-mount", "sandbox-mount");
    request.spec.mounts.push(SandboxMount {
        host_path: PathBuf::from("/tmp"),
        guest_path: "/workspace".into(),
        access: SandboxMountAccess::ReadWrite,
        internal: false,
    });

    let error = match backend.acquire(request).await {
        Ok(_) => panic!("expected a host bind-mount rejection"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("host bind-mounts"),
        "unexpected error: {error}"
    );
}

/// A command that already started must never be replayed: the run stream dying
/// mid-flight is terminal, not a reason to re-POST `/processes/run`.
#[tokio::test]
async fn exec_does_not_rerun_a_command_after_a_mid_stream_failure() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-truncated", "sandbox-truncated");
    let name = expected_sandbox_name(&request);
    mount_running_sandbox(&server, &name, "sbx-truncated").await;

    // The process started and emitted output, then the stream ended without the
    // exit event — exactly the shape of a connection dropped mid-command.
    let truncated = sse(&[
        json!({ "handle": 1, "pid": 42, "started_at": 1_710_000_000_000i64 }),
        json!({ "line": "side effect happened", "timestamp": 1i64, "stream": "stdout" }),
    ]);
    Mock::given(method("POST"))
        .and(path("/api/v1/processes/run"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(truncated, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    let error = match handle.exec(&touch_command()).await {
        Ok(output) => panic!("expected a truncated-stream error, got {output:?}"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("without an exit event"),
        "unexpected error: {error}"
    );
}

/// A proxy that reports the sandbox gone *before* the command is accepted is
/// safe to retry, once, against a freshly resolved target.
#[tokio::test]
async fn exec_retries_once_when_the_proxy_reports_the_sandbox_gone() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-gone", "sandbox-gone");
    let name = expected_sandbox_name(&request);
    mount_running_sandbox(&server, &name, "sbx-gone").await;

    Mock::given(method("POST"))
        .and(path("/api/v1/processes/run"))
        .respond_with(ResponseTemplate::new(404).set_body_string("sandbox not found"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let stream = sse(&[
        json!({ "handle": 1, "pid": 43, "started_at": 1_710_000_000_000i64 }),
        json!({ "line": "recovered", "timestamp": 1i64, "stream": "stdout" }),
        json!({ "exit_code": 0 }),
    ]);
    Mock::given(method("POST"))
        .and(path("/api/v1/processes/run"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    let output = handle.exec(&touch_command()).await.expect("exec");

    assert!(output.ok);
    assert_eq!(output.stdout, "recovered\n");
}

/// Resume is rejected while a suspend is still in flight, so a `suspending`
/// sandbox has to be waited out and resumed exactly once after it settles.
#[tokio::test]
async fn acquire_waits_out_a_suspending_sandbox_before_resuming() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-suspending", "sandbox-suspending");
    let name = expected_sandbox_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/sandboxes/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_info_json(
            &server,
            "sbx-suspending",
            "suspending",
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/sandboxes/sbx-suspending"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_info_json(
            &server,
            "sbx-suspending",
            "suspended",
        )))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_running_sandbox(&server, "sbx-suspending", "sbx-suspending").await;
    Mock::given(method("POST"))
        .and(path("/sandboxes/sbx-suspending/resume"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;

    backend.acquire(request).await.expect("acquire");
}

fn touch_command() -> exoharness::SandboxCommand {
    exoharness::SandboxCommand {
        argv: vec!["touch".into(), "side-effect".into()],
        env: Default::default(),
        display_argv: None,
        cwd: None,
        timeout: None,
    }
}

/// The snapshot id is only worth persisting once the platform can restore from
/// it; `POST /snapshot` returns before that is true.
#[tokio::test]
async fn snapshot_waits_for_the_platform_to_finish_writing_it() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-snap-wait", "sandbox-snap-wait");
    let name = expected_sandbox_name(&request);
    mount_running_sandbox(&server, &name, "sbx-snap-wait").await;

    Mock::given(method("POST"))
        .and(path("/sandboxes/sbx-snap-wait/snapshot"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "snapshot_id": "snap-slow",
            "status": "in_progress",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/snapshots/snap-slow"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "in_progress" })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/snapshots/snap-slow"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "completed" })))
        .expect(1..)
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    let payload = handle.snapshot().await.expect("snapshot");
    let manifest: Value = serde_json::from_slice(&payload.bytes).expect("manifest json");
    assert_eq!(manifest["snapshot_id"], json!("snap-slow"));
}

#[tokio::test]
async fn snapshot_surfaces_a_failed_snapshot_instead_of_persisting_its_id() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-snap-fail", "sandbox-snap-fail");
    let name = expected_sandbox_name(&request);
    mount_running_sandbox(&server, &name, "sbx-snap-fail").await;

    Mock::given(method("POST"))
        .and(path("/sandboxes/sbx-snap-fail/snapshot"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "snapshot_id": "snap-bad",
            "status": "in_progress",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/snapshots/snap-bad"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "failed",
            "error": "export exceeded disk budget",
        })))
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    let error = match handle.snapshot().await {
        Ok(_) => panic!("a failed snapshot must not yield a usable manifest"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("export exceeded disk budget"),
        "unexpected error: {error}"
    );
}

/// Restoring one sandbox's filesystem under another's identity would hand the
/// caller someone else's state.
#[tokio::test]
async fn acquire_from_snapshot_rejects_a_manifest_from_another_sandbox() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-restore", "sandbox-restore");

    let payload = SnapshotPayload {
        kind: SnapshotKind::TensorlakeSnapshot,
        bytes: Bytes::from(
            serde_json::to_vec(&json!({
                "snapshot_id": "snap-elsewhere",
                "sandbox_name": "exo-0000000000000000",
            }))
            .unwrap(),
        ),
    };

    let error = match backend.acquire_from_snapshot(request, payload).await {
        Ok(_) => panic!("expected a sandbox-identity mismatch"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("exo-0000000000000000"),
        "unexpected error: {error}"
    );
}
