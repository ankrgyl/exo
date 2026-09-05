//! Wiremock-driven tests for the Sprites sandbox backend.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use exoharness::{
    ManagedSandboxBackend, SandboxKey, SandboxLifecycleConfig, SandboxMount, SandboxMountAccess,
    SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotFormat, SnapshotPayload,
    SpritesConfig, SpritesSandboxBackend,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_request(conversation_id: &str, sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: conversation_id.into(),
            sandbox_id: sandbox_id.into(),
        },
        spec: SandboxSpec {
            image: "default".into(),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            egress_policy: SandboxNetworkPolicy::allow_all(),
            default_workdir: "/home/sprite".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

fn sandbox_spec_hash(spec: &SandboxSpec) -> String {
    let mut hasher = DefaultHasher::new();
    spec.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn expected_sprite_name(request: &SandboxRequest) -> String {
    let mut hasher = DefaultHasher::new();
    request.key.hash(&mut hasher);
    sandbox_spec_hash(&request.spec).hash(&mut hasher);
    format!("exo-{:016x}", hasher.finish())
}

fn backend_for_mock(server: &MockServer) -> SpritesSandboxBackend {
    backend_with_config(
        server,
        SpritesConfig {
            token: "test-token".into(),
            api_url: server.uri(),
            url_auth: None,
            organization: None,
            extra_labels: Vec::new(),
        },
    )
}

fn backend_with_config(_server: &MockServer, config: SpritesConfig) -> SpritesSandboxBackend {
    SpritesSandboxBackend::new(config).expect("SpritesSandboxBackend::new")
}

fn sprite_info_json(name: &str) -> Value {
    json!({
        "id": "sprite-test-id",
        "name": name,
        "status": "cold",
        "url": "https://example.sprites.app",
        "organization": "test-org",
        "created_at": "2026-06-01T12:00:00Z",
        "updated_at": "2026-06-01T12:00:00Z",
    })
}

#[tokio::test]
async fn acquire_creates_sprite_when_missing() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-1", "sandbox-1");
    let name = expected_sprite_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/v1/sprites/{name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/sprites"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sprite_info_json(&name)))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.expect("acquire");
    assert_eq!(handle.id(), "sprites:conversation:conv-1:sandbox-1");
}

#[tokio::test]
async fn acquire_create_includes_exo_metadata_labels() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-labels", "sandbox-labels");
    let name = expected_sprite_name(&request);
    let spec_hash = sandbox_spec_hash(&request.spec);

    Mock::given(method("GET"))
        .and(path(format!("/v1/sprites/{name}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/sprites"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sprite_info_json(&name)))
        .expect(1)
        .mount(&server)
        .await;

    backend.acquire(request).await.expect("acquire");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = requests
        .iter()
        .find(|r| r.method.as_str() == "POST")
        .expect("create request");
    let body: Value = serde_json::from_slice(&create.body).unwrap();
    let labels = body
        .get("labels")
        .and_then(Value::as_array)
        .expect("labels array");
    assert!(labels.iter().any(|label| {
        label.as_str() == Some("exo.sandbox.key=conversation:conv-labels:sandbox-labels")
    }));
    assert!(
        labels
            .iter()
            .any(|label| label.as_str() == Some(&format!("exo.sandbox.spec-hash={spec_hash}")))
    );
}

#[tokio::test]
async fn acquire_create_honors_binding_url_auth_and_organization() {
    let server = MockServer::start().await;
    let backend = backend_with_config(
        &server,
        SpritesConfig {
            token: "test-token".into(),
            api_url: server.uri(),
            url_auth: Some("public".into()),
            organization: Some("my-org".into()),
            extra_labels: vec!["prod".into()],
        },
    );
    let request = make_request("conv-bind", "sandbox-bind");
    let name = expected_sprite_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/v1/sprites/{name}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/sprites"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sprite_info_json(&name)))
        .expect(1)
        .mount(&server)
        .await;

    backend.acquire(request).await.expect("acquire");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = requests
        .iter()
        .find(|r| r.method.as_str() == "POST")
        .expect("create request");
    let body: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(
        body.pointer("/url_settings/auth").and_then(Value::as_str),
        Some("public")
    );
    assert_eq!(
        body.get("organization").and_then(Value::as_str),
        Some("my-org")
    );
    let labels = body
        .get("labels")
        .and_then(Value::as_array)
        .expect("labels");
    assert!(labels.iter().any(|label| label.as_str() == Some("prod")));
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
        msg.contains("mount") || msg.contains("sprites"),
        "unexpected: {msg}"
    );
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        0
    );
}

#[tokio::test]
async fn acquire_reuses_existing_sprite_without_create() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-3", "sandbox-3");
    let name = expected_sprite_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/v1/sprites/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(sprite_info_json(&name)))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(request)
        .await
        .expect("acquire should reuse existing sprite");
    assert_eq!(handle.id(), "sprites:conversation:conv-3:sandbox-3");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests.iter().any(|r| r.method.as_str() == "POST"),
        "acquire must not create sprites when one already exists"
    );
}

#[tokio::test]
async fn stop_does_not_delete_sprite() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-5", "sandbox-5");
    let name = expected_sprite_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/v1/sprites/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(sprite_info_json(&name)))
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.unwrap();
    handle.stop().await.expect("stop is a no-op");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests.iter().any(|r| r.method.as_str() == "DELETE"),
        "stop must not delete the sprite"
    );
}

#[tokio::test]
async fn exec_accepts_plain_text_http_response() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-plain", "sandbox-plain");
    let name = expected_sprite_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/v1/sprites/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(sprite_info_json(&name)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"/v1/sprites/.+/exec$"))
        .respond_with(ResponseTemplate::new(200).set_body_string("4\n"))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.unwrap();
    let output = handle
        .exec(&exoharness::SandboxCommand {
            argv: vec!["python3".into(), "-c".into(), "print(2+2)".into()],
            env: Default::default(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("plain text exec body should parse");

    assert_eq!(output.stdout, "4\n");
    assert!(output.ok);
}

#[tokio::test]
async fn exec_uses_http_post_with_cmd_query_params() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-6", "sandbox-6");
    let name = expected_sprite_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/v1/sprites/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(sprite_info_json(&name)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"/v1/sprites/.+/exec$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "exitCode": 0,
            "stdout": "hello\n",
            "stderr": "",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.unwrap();
    let output = handle
        .exec(&exoharness::SandboxCommand {
            argv: vec!["echo".into(), "hello".into()],
            env: Default::default(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("exec");

    assert!(output.ok);
    assert_eq!(output.stdout, "hello\n");

    let requests = server.received_requests().await.unwrap_or_default();
    let exec_req = requests
        .iter()
        .find(|r| r.url.path().ends_with("/exec"))
        .expect("exec request");
    let query = exec_req.url.query().expect("query string");
    assert!(query.contains("cmd=echo"));
    assert!(query.contains("cmd=hello"));
    assert!(query.contains("stdin=false"));
}

#[tokio::test]
async fn snapshot_returns_sprites_snapshot_payload() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-7", "sandbox-7");
    let name = expected_sprite_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/v1/sprites/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(sprite_info_json(&name)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/sprites/{name}/checkpoint")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                r#"{"type":"info","data":"  ID: v3","time":"2026-06-01T12:00:00Z"}"#,
                "\n",
                r#"{"type":"complete","data":"Checkpoint v3 created","time":"2026-06-01T12:00:01Z"}"#,
                "\n",
            ),
            "application/x-ndjson",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend.acquire(request).await.unwrap();
    let payload = handle.snapshot().await.expect("snapshot");

    assert_eq!(payload.format, SnapshotFormat::SpritesRef);
    let manifest: Value = serde_json::from_slice(&payload.bytes).unwrap();
    assert_eq!(
        manifest.get("checkpoint_id").and_then(Value::as_str),
        Some("v3")
    );
    assert_eq!(
        manifest.get("sprite_name").and_then(Value::as_str),
        Some(name.as_str())
    );
}

#[tokio::test]
async fn acquire_from_snapshot_restores_checkpoint() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);
    let request = make_request("conv-8", "sandbox-8");
    let name = expected_sprite_name(&request);

    Mock::given(method("GET"))
        .and(path(format!("/v1/sprites/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(sprite_info_json(&name)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/sprites/{name}/checkpoints/v2/restore")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"type":"complete","data":"Restore complete","time":"2026-06-01T12:00:00Z"}"#,
            "application/x-ndjson",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let manifest = json!({
        "checkpoint_id": "v2",
        "sprite_name": name,
    });
    let payload = SnapshotPayload {
        format: SnapshotFormat::SpritesRef,
        bytes: Bytes::from(serde_json::to_vec(&manifest).unwrap()),
    };

    backend
        .acquire_from_snapshot(request, payload)
        .await
        .expect("restore");
}

#[tokio::test]
async fn acquire_from_snapshot_rejects_wrong_format() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    let payload = SnapshotPayload {
        format: SnapshotFormat::DockerImageTar,
        bytes: Bytes::from_static(b"not-a-tar"),
    };

    let error = match backend
        .acquire_from_snapshot(make_request("conv-9", "sandbox-9"), payload)
        .await
    {
        Ok(_) => panic!("wrong snapshot format should fail"),
        Err(error) => error,
    };
    let msg = format!("{error:#}").to_lowercase();
    assert!(
        msg.contains("sprites") || msg.contains("format"),
        "unexpected: {msg}"
    );
}
