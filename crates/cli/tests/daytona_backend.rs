//! Wiremock-driven tests for the Daytona sandbox backend. Each test points a
//! `DaytonaSandboxBackend` at an in-process fake of Daytona's REST API and
//! asserts on the wire contract — endpoints hit (control plane vs toolbox host),
//! request bodies/params, and how canned responses drive find-or-create.
//! Hermetic by design, so it catches code defects but not upstream drift.

use std::collections::HashMap;

use bytes::Bytes;
use exoharness::{
    DaytonaConfig, DaytonaSandboxBackend, ManagedSandboxBackend, SandboxKey,
    SandboxLifecycleConfig, SandboxMount, SandboxMountAccess, SandboxNetworkPolicy, SandboxRequest,
    SandboxSpec, SnapshotFormat, SnapshotPayload,
};
use futures::io::{AsyncReadExt, AsyncWriteExt};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ─────────────────────── Test fixtures / helpers ───────────────────────

/// Standard sandbox request used across tests. Conversation-keyed so the label
/// format matches what the find-by-label query expects to see.
fn make_request(conversation_id: &str, sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: conversation_id.into(),
            sandbox_id: sandbox_id.into(),
        },
        spec: SandboxSpec {
            image: "docker.io/library/ubuntu:24.04".into(),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            egress_policy: SandboxNetworkPolicy::allow_all(),
            default_workdir: "/".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

/// Construct a backend pointed at a wiremock instance. Same wiremock host for
/// both control-plane and toolbox URLs — Daytona's real deployment uses
/// separate hosts, but tests differentiate by path prefix.
fn backend_for_mock(server: &MockServer) -> DaytonaSandboxBackend {
    DaytonaSandboxBackend::new(DaytonaConfig {
        api_key: "test-api-key".into(),
        api_url: server.uri(),
        toolbox_url: server.uri(),
        target: None,
        organization_id: Some("test-org".into()),
    })
    .expect("DaytonaSandboxBackend::new")
}

fn sandbox_json(id: &str, state: &str) -> Value {
    json!({
        "id": id,
        "state": state,
        "createdAt": "2026-05-25T08:00:00Z"
    })
}

fn list_response(items: Vec<Value>) -> Value {
    json!({ "items": items })
}

/// Mount a `GET /sandbox` (find-by-label) responder returning `items`.
async fn mount_find(server: &MockServer, items: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_response(items)))
        .mount(server)
        .await;
}

/// Mount `GET /sandbox/{id}` returning a `started` sandbox, so `acquire`'s
/// wait-until-started poll resolves immediately. (`path_regex` matches a single
/// id segment, so it won't shadow `/sandbox` or `/sandbox/{id}/{action}`.)
async fn mount_get_started(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/sandbox/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb", "started")))
        .mount(server)
        .await;
}

// ─────────────────────── acquire: find-or-create ───────────────────────

#[tokio::test]
async fn acquire_creates_when_no_warm_match() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-fresh", "started")))
        .expect(1)
        .mount(&server)
        .await;

    mount_get_started(&server).await;

    let request = make_request("conv-1", "sandbox-1");
    let _handle = backend
        .acquire(request)
        .await
        .expect("acquire should succeed against a 200-returning mock");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = requests
        .iter()
        .find(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "POST")
        .expect("POST /sandbox (create) should have been called");
    let body: Value = serde_json::from_slice(&create.body).expect("body is JSON");

    // Labels carry the SandboxKey + spec hash; their absence would break
    // cross-process recovery by label.
    let labels = body
        .get("labels")
        .and_then(Value::as_object)
        .expect("labels present");
    assert!(
        labels.contains_key("exo.sandbox.key"),
        "labels should include exo.sandbox.key: {labels:?}"
    );
    assert!(
        labels.contains_key("exo.sandbox.spec-hash"),
        "labels should include exo.sandbox.spec-hash: {labels:?}"
    );

    // A fresh create routes the requested image into Daytona's `snapshot`
    // selector (Daytona's only base-image lever).
    assert_eq!(
        body.get("snapshot").and_then(Value::as_str),
        Some("docker.io/library/ubuntu:24.04"),
        "fresh acquire should pass the requested image as `snapshot`: {body:?}"
    );
}

#[tokio::test]
async fn acquire_reuses_running_match_without_create_or_start() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    // A labelled, already-running match: no create and no start should follow.
    mount_find(&server, vec![sandbox_json("sb-running", "started")]).await;
    mount_get_started(&server).await;

    let request = make_request("conv-3", "sandbox-3");
    backend
        .acquire(request)
        .await
        .expect("acquire should reuse the running sandbox");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests
            .iter()
            .any(|r| r.method.to_string().to_uppercase() == "POST"),
        "reusing a running sandbox must not POST (no create, no start): {:?}",
        requests.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn acquire_starts_stopped_match_without_creating() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, vec![sandbox_json("sb-stopped", "stopped")]).await;
    Mock::given(method("POST"))
        .and(path("/sandbox/sb-stopped/start"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    let request = make_request("conv-4", "sandbox-4");
    backend
        .acquire(request)
        .await
        .expect("acquire should start the stopped sandbox");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/sandbox/sb-stopped/start"),
        "a stopped match must be started"
    );
    // It must reuse the existing sandbox, not create a fresh one.
    assert!(
        !requests
            .iter()
            .any(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "POST"),
        "starting a stopped match must NOT create a new sandbox"
    );
}

#[tokio::test]
async fn acquire_does_not_start_transient_match() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    // A sandbox mid-startup must be left alone: issuing /start on a "starting"
    // sandbox races with Daytona's own transition.
    mount_find(&server, vec![sandbox_json("sb-starting", "starting")]).await;
    mount_get_started(&server).await;

    backend
        .acquire(make_request("conv-12", "sandbox-12"))
        .await
        .expect("acquire should reuse the transitioning sandbox");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests
            .iter()
            .any(|r| r.method.to_string().to_uppercase() == "POST"),
        "a transient (starting) sandbox must not be started or recreated: {:?}",
        requests.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn acquire_replaces_dead_match_with_fresh_create() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    // A terminal/error sandbox can't be reused — acquire must create a new one.
    mount_find(&server, vec![sandbox_json("sb-dead", "destroyed")]).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-new", "started")))
        .expect(1)
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    backend
        .acquire(make_request("conv-13", "sandbox-13"))
        .await
        .expect("acquire should create a fresh sandbox when the match is dead");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "POST"),
        "a dead match must trigger a fresh create"
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.url.path() == "/sandbox/sb-dead/start"),
        "a dead match must not be started"
    );
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
        Ok(_) => panic!("acquire should reject requests with host mounts"),
        Err(e) => e,
    };
    let msg = format!("{error:#}").to_lowercase();
    assert!(
        msg.contains("mount") || msg.contains("daytona"),
        "error should mention mounts and/or Daytona: {msg}"
    );

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests.len(),
        0,
        "mount-rejected request must not reach the API"
    );
}

#[tokio::test]
async fn acquire_filters_by_label_as_single_json_query_param() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("GET"))
        .and(path("/sandbox"))
        .and(query_param_present("labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_response(Vec::new())))
        .expect(1)
        .mount(&server)
        .await;
    // No warm match → acquire proceeds to create; mock it so acquire succeeds.
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-q", "started")))
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    let request = make_request("conv-6", "sandbox-6");
    backend.acquire(request).await.unwrap();

    let requests = server.received_requests().await.unwrap_or_default();
    let find = requests
        .iter()
        .find(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "GET")
        .expect("GET /sandbox (find) should have been called");
    let label_params: Vec<_> = find
        .url
        .query_pairs()
        .filter(|(k, _)| k == "labels")
        .collect();
    assert_eq!(
        label_params.len(),
        1,
        "labels must be one query param, not repeated: {}",
        find.url
    );
    let parsed: Value =
        serde_json::from_str(&label_params[0].1).expect("labels query value must be JSON-encoded");
    assert!(
        parsed.get("exo.sandbox.key").is_some(),
        "labels JSON should include exo.sandbox.key: {parsed:?}"
    );
}

/// Assert that a given query param key is present in the URL (without asserting
/// on its value).
fn query_param_present(name: &'static str) -> impl wiremock::Match {
    struct Present(&'static str);
    impl wiremock::Match for Present {
        fn matches(&self, request: &wiremock::Request) -> bool {
            request.url.query_pairs().any(|(k, _)| k == self.0)
        }
    }
    Present(name)
}

// ─────────────────────── stop ───────────────────────

#[tokio::test]
async fn stop_calls_stop_endpoint_not_delete() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-stop", "started")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandbox/sb-stop/stop"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    let handle = backend
        .acquire(make_request("conv-7", "sandbox-7"))
        .await
        .unwrap();
    handle.stop().await.expect("stop should succeed");

    let requests = server.received_requests().await.unwrap_or_default();
    // No DELETE: stopped sandboxes must survive so the next process resumes them.
    assert!(
        !requests
            .iter()
            .any(|r| r.method.to_string().to_uppercase() == "DELETE"),
        "stop must not DELETE the sandbox"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/sandbox/sb-stop/stop"),
        "POST /sandbox/<id>/stop should have been called"
    );
}

// ─────────────────────── snapshot ───────────────────────

#[tokio::test]
async fn snapshot_returns_daytona_snapshot_payload_with_manifest() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-snap", "started")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandbox/sb-snap/snapshot"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    // snapshot() polls the sandbox until it leaves `snapshotting`...
    mount_get_started(&server).await;
    // ...then waits for the snapshot resource itself to reach `active`.
    Mock::given(method("GET"))
        .and(path_regex(r"^/snapshots/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "state": "active" })))
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-8", "sandbox-8"))
        .await
        .unwrap();
    let payload = handle
        .snapshot()
        .await
        .expect("snapshot should succeed against the mock");

    assert_eq!(payload.format, SnapshotFormat::DaytonaRef);

    let manifest: Value =
        serde_json::from_slice(&payload.bytes).expect("payload should be a JSON manifest");
    assert!(
        manifest
            .get("snapshot_name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.starts_with("exo-snap-")),
        "manifest should carry a snapshot_name with the exo-snap- prefix: {manifest:?}"
    );

    let requests = server.received_requests().await.unwrap_or_default();
    let snap_call = requests
        .iter()
        .find(|r| r.url.path() == "/sandbox/sb-snap/snapshot")
        .expect("snapshot endpoint should have been called");
    let body: Value = serde_json::from_slice(&snap_call.body).unwrap();
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .expect("snapshot request body must contain `name`");
    assert!(name.starts_with("exo-snap-"));
}

#[tokio::test]
async fn snapshot_surfaces_feature_flag_error_on_403() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-flag", "started")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandbox/sb-flag/snapshot"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "statusCode": 403,
            "error": "Forbidden",
            "message": "Required feature flags are not enabled"
        })))
        .mount(&server)
        .await;

    mount_get_started(&server).await;

    let handle = backend
        .acquire(make_request("conv-flag", "sandbox-flag"))
        .await
        .unwrap();
    let error = match handle.snapshot().await {
        Ok(_) => panic!("snapshot should fail when the feature flag is off"),
        Err(e) => format!("{e:#}").to_lowercase(),
    };
    assert!(
        error.contains("feature") && error.contains("enable"),
        "403 should surface an actionable feature-flag message: {error}"
    );
}

#[tokio::test]
async fn acquire_from_snapshot_passes_snapshot_name_in_create_body() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    // acquire_from_snapshot creates directly (no find step).
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sandbox_json("sb-restored", "started")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let manifest = json!({ "snapshot_name": "exo-snap-canonical-fixture" });
    let payload = SnapshotPayload {
        format: SnapshotFormat::DaytonaRef,
        bytes: Bytes::from(serde_json::to_vec(&manifest).unwrap()),
    };

    mount_get_started(&server).await;

    let request = make_request("conv-9", "sandbox-9");
    backend
        .acquire_from_snapshot(request, payload)
        .await
        .expect("acquire_from_snapshot should succeed against a 200 mock");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = requests
        .iter()
        .find(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "POST")
        .expect("POST /sandbox should have been called");
    let body: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(
        body.get("snapshot").and_then(Value::as_str),
        Some("exo-snap-canonical-fixture"),
        "create-from-snapshot must pass the manifest's snapshot_name to Daytona: {body:?}"
    );
}

#[tokio::test]
async fn acquire_from_snapshot_rejects_foreign_formats() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    let payload = SnapshotPayload {
        format: SnapshotFormat::E2bRef,
        bytes: Bytes::from_static(b"{}"),
    };
    let request = make_request("conv-10", "sandbox-10");
    let error = match backend.acquire_from_snapshot(request, payload).await {
        Ok(_) => panic!("Daytona backend must reject an e2b-ref payload"),
        Err(e) => e,
    };
    let msg = format!("{error:#}").to_lowercase();
    assert!(
        msg.contains("daytona") && msg.contains("e2b-ref"),
        "error should explain the format mismatch: {msg}"
    );

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 0, "format mismatch must not reach the API");
}

#[tokio::test]
async fn acquire_from_snapshot_bridges_docker_tar_but_fails_on_garbage() {
    // A DockerImageTar payload is accepted (that's the teleport bridge), but the
    // bytes here aren't a real `docker save` tarball, so the local `docker load`
    // step fails before anything reaches the Daytona API.
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    let payload = SnapshotPayload {
        format: SnapshotFormat::DockerImageTar,
        bytes: Bytes::from_static(b"\x00not-a-real-tar"),
    };
    let request = make_request("conv-11", "sandbox-11");
    let error = match backend.acquire_from_snapshot(request, payload).await {
        Ok(_) => panic!("garbage tar bytes must not restore"),
        Err(e) => e,
    };
    let msg = format!("{error:#}").to_lowercase();
    assert!(
        msg.contains("docker"),
        "error should point at the docker bridge step: {msg}"
    );

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests.len(),
        0,
        "a failed bridge import must not reach the API"
    );
}

// ─────────────────────── exec (toolbox URL) ───────────────────────

#[tokio::test]
async fn exec_uses_toolbox_url_not_api_url() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-exec", "started")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-exec/process/execute"))
        .and(body_json_includes_command())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "exitCode": 0,
            "result": "hello from mock",
        })))
        .expect(1)
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    let handle = backend
        .acquire(make_request("conv-11", "sandbox-11"))
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
        .expect("exec should succeed");

    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "hello from mock");

    // The toolbox path must be used: in production it's a different DNS name,
    // so the code must route exec through `toolbox_endpoint`, not `api_endpoint`.
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/toolbox/sb-exec/process/execute"),
        "exec must hit /toolbox/<id>/process/execute"
    );
}

#[tokio::test]
async fn start_process_streams_raw_daytona_session_logs() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sandbox_json("sb-process", "started")),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-process/process/session"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(
            r"^/toolbox/sb-process/process/session/[^/]+/exec$",
        ))
        .and(body_json_includes_async_session_command())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "cmdId": "cmd-1",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/toolbox/sb-process/process/session/[^/]+/command/cmd-1/logs$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"id\":1,\"result\":{}}\n"))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/toolbox/sb-process/process/session/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commands": [
                {
                    "id": "cmd-1",
                    "exitCode": 0,
                },
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-process/process/execute"))
        .and(body_json_reads_exit_status_file())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "exitCode": 0,
            "result": "0\n",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/toolbox/sb-process/process/session/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    let handle = backend
        .acquire(make_request("conv-14", "sandbox-14"))
        .await
        .unwrap();
    let mut parts = handle
        .start_process(&exoharness::SandboxCommand {
            argv: vec!["/usr/bin/codex-app-server".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("start_process should use Daytona process sessions");

    let mut stdout = String::new();
    parts
        .stdout
        .read_to_string(&mut stdout)
        .await
        .expect("stdout should be readable");
    let exit_code = parts.wait.await.expect("wait should resolve");

    assert_eq!(stdout, "{\"id\":1,\"result\":{}}\n");
    assert_eq!(exit_code, 0);
}

#[tokio::test]
async fn start_process_streams_structured_daytona_session_logs() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sandbox_json("sb-structured", "started")),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-structured/process/session"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(
            r"^/toolbox/sb-structured/process/session/[^/]+/exec$",
        ))
        .and(body_json_includes_async_session_command())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "cmdId": "cmd-1",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/toolbox/sb-structured/process/session/[^/]+/command/cmd-1/logs$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "stdout": "{\"id\":1,\"result\":{}}\n",
            "stderr": "warning\n",
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/toolbox/sb-structured/process/session/[^/]+$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commands": [
                {
                    "id": "cmd-1",
                    "exitCode": 0,
                },
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-structured/process/execute"))
        .and(body_json_reads_exit_status_file())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "exitCode": 0,
            "result": "0\n",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(
            r"^/toolbox/sb-structured/process/session/[^/]+$",
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    let handle = backend
        .acquire(make_request("conv-15", "sandbox-15"))
        .await
        .unwrap();
    let mut parts = handle
        .start_process(&exoharness::SandboxCommand {
            argv: vec!["/usr/bin/codex-app-server".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("start_process should use Daytona process sessions");

    let mut stdout = String::new();
    parts
        .stdout
        .read_to_string(&mut stdout)
        .await
        .expect("stdout should be readable");
    let mut stderr = String::new();
    parts
        .stderr
        .read_to_string(&mut stderr)
        .await
        .expect("stderr should be readable");
    let exit_code = parts.wait.await.expect("wait should resolve");

    assert_eq!(stdout, "{\"id\":1,\"result\":{}}\n");
    assert_eq!(stderr, "warning\n");
    assert_eq!(exit_code, 0);
}

#[tokio::test]
async fn start_process_seeds_env_for_daytona_session_without_leaking_secret_in_command() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-env", "started")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-env/process/session"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/toolbox/sb-env/process/session/[^/]+/exec$"))
        .and(body_json_includes_async_session_command_without_secret(
            "sk-secret",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "cmdId": "cmd-1",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(
            r"^/toolbox/sb-env/process/session/[^/]+/command/cmd-1/input$",
        ))
        .and(body_json_seeds_session_env_with_secret(
            "OPENAI_API_KEY",
            "sk-secret",
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/toolbox/sb-env/process/session/[^/]+/command/cmd-1/logs$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok\n"))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/toolbox/sb-env/process/session/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commands": [
                {
                    "id": "cmd-1",
                    "exitCode": 0,
                },
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-env/process/execute"))
        .and(body_json_reads_exit_status_file())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "exitCode": 0,
            "result": "0\n",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/toolbox/sb-env/process/session/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    let handle = backend
        .acquire(make_request("conv-18", "sandbox-18"))
        .await
        .unwrap();
    let mut env = HashMap::new();
    env.insert("OPENAI_API_KEY".to_string(), "sk-secret".to_string());
    let mut parts = handle
        .start_process(&exoharness::SandboxCommand {
            argv: vec!["/usr/bin/codex-app-server".into()],
            env,
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("start_process should seed env before returning Daytona session stdin");

    let mut stdout = String::new();
    parts
        .stdout
        .read_to_string(&mut stdout)
        .await
        .expect("stdout should be readable");
    let exit_code = parts.wait.await.expect("wait should resolve");

    assert_eq!(stdout, "ok\n");
    assert_eq!(exit_code, 0);
}

#[tokio::test]
async fn start_process_reports_daytona_stdin_errors() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-stdin", "started")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-stdin/process/session"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(
            r"^/toolbox/sb-stdin/process/session/[^/]+/exec$",
        ))
        .and(body_json_includes_async_session_command())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "cmdId": "cmd-1",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/toolbox/sb-stdin/process/session/[^/]+/command/cmd-1/logs$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/toolbox/sb-stdin/process/session/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commands": [
                {
                    "id": "cmd-1",
                },
            ],
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-stdin/process/execute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "exitCode": 0,
            "result": "",
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(
            r"^/toolbox/sb-stdin/process/session/[^/]+/command/cmd-1/input$",
        ))
        .respond_with(ResponseTemplate::new(500).set_body_string("input rejected"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/toolbox/sb-stdin/process/session/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    let handle = backend
        .acquire(make_request("conv-16", "sandbox-16"))
        .await
        .unwrap();
    let mut parts = handle
        .start_process(&exoharness::SandboxCommand {
            argv: vec!["/usr/bin/codex-app-server".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("start_process should use Daytona process sessions");

    parts
        .stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1}\n")
        .await
        .expect("stdin write should enter the local pipe");

    let error = tokio::time::timeout(Duration::from_secs(2), parts.wait)
        .await
        .expect("wait should not hang after a Daytona stdin failure")
        .expect_err("stdin failure should fail the process wait");
    let message = format!("{error:#}");
    assert!(
        message.contains("Daytona process stdin forwarding failed"),
        "error should identify stdin forwarding: {message}"
    );
    assert!(
        message.contains("Daytona process input failed"),
        "error should include the Daytona input failure: {message}"
    );
}

#[tokio::test]
async fn start_process_deletes_daytona_session_when_wait_is_aborted() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-abort", "started")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-abort/process/session"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(
            r"^/toolbox/sb-abort/process/session/[^/]+/exec$",
        ))
        .and(body_json_includes_async_session_command())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "cmdId": "cmd-1",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/toolbox/sb-abort/process/session/[^/]+/command/cmd-1/logs$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/toolbox/sb-abort/process/session/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commands": [
                {
                    "id": "cmd-1",
                },
            ],
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-abort/process/execute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "exitCode": 0,
            "result": "",
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/toolbox/sb-abort/process/session/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    let handle = backend
        .acquire(make_request("conv-17", "sandbox-17"))
        .await
        .unwrap();
    let parts = handle
        .start_process(&exoharness::SandboxCommand {
            argv: vec!["/usr/bin/codex-app-server".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("start_process should use Daytona process sessions");

    let wait_task = tokio::spawn(parts.wait);
    tokio::time::sleep(Duration::from_millis(10)).await;
    wait_task.abort();
    let join_error = wait_task
        .await
        .expect_err("aborted wait task should report cancellation");
    assert!(join_error.is_cancelled());

    wait_for_delete_request(&server, "/toolbox/sb-abort/process/session/").await;
}

#[tokio::test]
async fn start_process_waits_on_exit_status_file_when_session_status_omits_exit_code() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sandbox_json("sb-exit-file", "started")),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-exit-file/process/session"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(
            r"^/toolbox/sb-exit-file/process/session/[^/]+/exec$",
        ))
        .and(body_json_includes_async_session_command())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "cmdId": "cmd-1",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/toolbox/sb-exit-file/process/session/[^/]+/command/cmd-1/logs$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("done\n"))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/toolbox/sb-exit-file/process/session/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commands": [
                {
                    "id": "cmd-1",
                },
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-exit-file/process/execute"))
        .and(body_json_reads_exit_status_file())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "exitCode": 0,
            "result": "0\n",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/toolbox/sb-exit-file/process/session/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    mount_get_started(&server).await;

    let handle = backend
        .acquire(make_request("conv-19", "sandbox-19"))
        .await
        .unwrap();
    let mut parts = handle
        .start_process(&exoharness::SandboxCommand {
            argv: vec!["/usr/bin/codex-app-server".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("start_process should use Daytona process sessions");

    let mut stdout = String::new();
    parts
        .stdout
        .read_to_string(&mut stdout)
        .await
        .expect("stdout should be readable");
    let exit_code = parts.wait.await.expect("wait should resolve");

    assert_eq!(stdout, "done\n");
    assert_eq!(exit_code, 0);
}

/// Match any POST body that has a `command` field — keeps the test from
/// over-asserting on the precise shell-rendering while still proving the body
/// shape lines up with Daytona's expected schema.
fn body_json_includes_command() -> impl wiremock::Match {
    struct Has;
    impl wiremock::Match for Has {
        fn matches(&self, request: &wiremock::Request) -> bool {
            serde_json::from_slice::<Value>(&request.body)
                .ok()
                .and_then(|v| v.get("command").and_then(Value::as_str).map(str::to_string))
                .is_some()
        }
    }
    Has
}

fn body_json_includes_async_session_command() -> impl wiremock::Match {
    struct Has;
    impl wiremock::Match for Has {
        fn matches(&self, request: &wiremock::Request) -> bool {
            let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
                return false;
            };
            body.get("command").and_then(Value::as_str).is_some()
                && body.get("runAsync").and_then(Value::as_bool) == Some(true)
                && body.get("suppressInputEcho").and_then(Value::as_bool) == Some(true)
        }
    }
    Has
}

fn body_json_seeds_session_env_with_secret(
    key: &'static str,
    secret: &'static str,
) -> impl wiremock::Match {
    struct Has {
        key: &'static str,
        secret: &'static str,
    }
    impl wiremock::Match for Has {
        fn matches(&self, request: &wiremock::Request) -> bool {
            let Ok(body) = serde_json::from_slice::<DaytonaSessionInputBody>(&request.body) else {
                return false;
            };
            body.data.contains(self.key)
                && body.data.contains(self.secret)
                && body.data.contains("__EXO_ENV_END__")
        }
    }
    Has { key, secret }
}

fn body_json_includes_async_session_command_without_secret(
    secret: &'static str,
) -> impl wiremock::Match {
    struct Has {
        secret: &'static str,
    }
    impl wiremock::Match for Has {
        fn matches(&self, request: &wiremock::Request) -> bool {
            let Ok(body) = serde_json::from_slice::<DaytonaSessionExecBody>(&request.body) else {
                return false;
            };
            body.command.contains("__EXO_ENV_END__")
                && !body.command.contains(self.secret)
                && body.env.is_none()
                && body.run_async == Some(true)
                && body.suppress_input_echo == Some(true)
        }
    }
    Has { secret }
}

fn body_json_reads_exit_status_file() -> impl wiremock::Match {
    struct Has;
    impl wiremock::Match for Has {
        fn matches(&self, request: &wiremock::Request) -> bool {
            let Ok(body) = serde_json::from_slice::<DaytonaSessionExecBody>(&request.body) else {
                return false;
            };
            body.command.contains("/tmp/exo-process-exit-")
                && body.command.contains("cat")
                && body.run_async.is_none()
                && body.suppress_input_echo.is_none()
        }
    }
    Has
}

#[derive(serde::Deserialize)]
struct DaytonaSessionInputBody {
    data: String,
}

#[derive(serde::Deserialize)]
struct DaytonaSessionExecBody {
    command: String,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    #[serde(rename = "runAsync")]
    run_async: Option<bool>,
    #[serde(rename = "suppressInputEcho")]
    suppress_input_echo: Option<bool>,
}

async fn wait_for_delete_request(server: &MockServer, path_prefix: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let requests = server.received_requests().await.unwrap_or_default();
        if requests.iter().any(|request| {
            request.method.to_string().to_uppercase() == "DELETE"
                && request.url.path().starts_with(path_prefix)
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for DELETE request with path prefix {path_prefix}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
