//! Wiremock-driven tests for the Vercel sandbox backend. These validate the
//! REST contract without requiring Vercel credentials.

use std::collections::HashMap;
use std::time::Duration;

use exoharness::{
    ManagedSandboxBackend, SandboxCommand, SandboxKey, SandboxLifecycleConfig,
    SandboxNetworkPolicy, SandboxRequest, SandboxSpec, VercelConfig, VercelSandboxBackend,
};
use serde::Deserialize;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn make_request(conversation_id: &str, sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: conversation_id.into(),
            sandbox_id: sandbox_id.into(),
        },
        spec: SandboxSpec {
            image: "node24".into(),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            egress_policy: SandboxNetworkPolicy::allow_all(),
            default_workdir: "/vercel/sandbox".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

fn backend_for_mock(server: &MockServer) -> VercelSandboxBackend {
    VercelSandboxBackend::new(VercelConfig {
        api_token: "test-token".into(),
        api_url: server.uri(),
        team_id: "team_1".into(),
        project_id: "project_1".into(),
    })
    .expect("VercelSandboxBackend::new")
}

fn sandbox_response(session_id: &str) -> Value {
    json!({
        "sandbox": {
            "id": "sandbox-id",
            "status": "running"
        },
        "session": {
            "id": session_id
        }
    })
}

async fn mount_missing_named_sandbox(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/v2/sandboxes/exo-[0-9a-f]+$"))
        .and(query_param("teamId", "team_1"))
        .and(query_param("projectId", "project_1"))
        .and(query_param("resume", "true"))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

async fn mount_existing_named_sandbox(server: &MockServer, session_id: &str) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/v2/sandboxes/exo-[0-9a-f]+$"))
        .and(query_param("teamId", "team_1"))
        .and(query_param("projectId", "project_1"))
        .and(query_param("resume", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_response(session_id)))
        .mount(server)
        .await;
}

#[tokio::test]
async fn acquire_creates_named_sandbox_when_missing() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_missing_named_sandbox(&server).await;
    Mock::given(method("POST"))
        .and(path("/v2/sandboxes"))
        .and(query_param("teamId", "team_1"))
        .and(body_creates_named_sandbox())
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_response("sess_1")))
        .expect(1)
        .mount(&server)
        .await;

    backend
        .acquire(make_request("conv-1", "sandbox-1"))
        .await
        .expect("acquire should create a named Vercel sandbox");
}

#[tokio::test]
async fn acquire_reuses_named_sandbox_without_create() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_existing_named_sandbox(&server, "sess_reused").await;

    backend
        .acquire(make_request("conv-2", "sandbox-2"))
        .await
        .expect("acquire should reuse the named Vercel sandbox");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests
            .iter()
            .any(|request| request.method.to_string().to_uppercase() == "POST"),
        "reusing a named sandbox must not create a fresh sandbox"
    );
}

#[tokio::test]
async fn exec_sends_command_env_and_collects_logs() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_existing_named_sandbox(&server, "sess_exec").await;
    Mock::given(method("POST"))
        .and(path("/v2/sandboxes/sessions/sess_exec/cmd"))
        .and(query_param("teamId", "team_1"))
        .and(body_runs_command_with_env())
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                [
                    json!({
                        "command": {
                            "id": "cmd_1",
                            "name": "bash",
                            "args": ["-lc", "printf $OPENAI_API_KEY"],
                            "cwd": "/vercel/sandbox",
                            "sandboxId": "sandbox-id",
                            "exitCode": null,
                            "startedAt": 1
                        }
                    })
                    .to_string(),
                    json!({
                        "command": {
                            "id": "cmd_1",
                            "name": "bash",
                            "args": ["-lc", "printf $OPENAI_API_KEY"],
                            "cwd": "/vercel/sandbox",
                            "sandboxId": "sandbox-id",
                            "exitCode": 0,
                            "startedAt": 1
                        }
                    })
                    .to_string(),
                ]
                .join("\n"),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/sandboxes/sessions/sess_exec/cmd/cmd_1/logs"))
        .and(query_param("teamId", "team_1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                [
                    json!({"stream": "stdout", "data": "sk-secret"}),
                    json!({"stream": "stderr", "data": "warn\n"}),
                ]
                .into_iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-3", "sandbox-3"))
        .await
        .unwrap();
    let mut env = HashMap::new();
    env.insert("OPENAI_API_KEY".to_string(), "sk-secret".to_string());
    let output = handle
        .exec(&SandboxCommand {
            argv: vec![
                "bash".to_string(),
                "-lc".to_string(),
                "printf $OPENAI_API_KEY".to_string(),
            ],
            env,
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("exec should run through Vercel command API");

    assert!(output.ok);
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "sk-secret");
    assert_eq!(output.stderr, "warn\n");
}

#[tokio::test]
async fn stop_stops_vercel_session() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_existing_named_sandbox(&server, "sess_stop").await;
    Mock::given(method("POST"))
        .and(path("/v2/sandboxes/sessions/sess_stop/stop"))
        .and(query_param("teamId", "team_1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-4", "sandbox-4"))
        .await
        .unwrap();
    handle
        .stop()
        .await
        .expect("stop should call Vercel stop-session");
}

#[tokio::test]
async fn start_process_reports_bridge_install_failure() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_existing_named_sandbox(&server, "sess_process").await;
    Mock::given(method("POST"))
        .and(path("/v2/sandboxes/sessions/sess_process/cmd"))
        .and(query_param("teamId", "team_1"))
        .and(body_runs_shell_command_containing(
            "/tmp/exo-process-bridge.py",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                json!({
                    "command": {
                        "id": "cmd_install",
                        "name": "/bin/sh",
                        "args": ["-lc", "install bridge"],
                        "cwd": "/vercel/sandbox",
                        "sandboxId": "sandbox-id",
                        "exitCode": 1,
                        "startedAt": 1
                    }
                })
                .to_string(),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v2/sandboxes/sessions/sess_process/cmd/cmd_install/logs",
        ))
        .and(query_param("teamId", "team_1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                json!({"stream": "stderr", "data": "python3 missing"}).to_string(),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-5", "sandbox-5"))
        .await
        .unwrap();
    let result = handle
        .start_process(&SandboxCommand {
            argv: vec!["codex".into(), "app-server".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await;
    let error = match result {
        Ok(_) => panic!("start_process should fail during mocked bridge install"),
        Err(error) => error,
    };
    let error = format!("{error:#}");
    assert!(
        error.contains("installing process bridge failed") && error.contains("python3 missing"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn start_process_rejects_existing_vercel_bridge() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_existing_named_sandbox(&server, "sess_process_busy").await;
    Mock::given(method("POST"))
        .and(path("/v2/sandboxes/sessions/sess_process_busy/cmd"))
        .and(query_param("teamId", "team_1"))
        .and(body_runs_bridge_request("ping"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                json!({
                    "command": {
                        "id": "cmd_ping",
                        "name": "python3",
                        "args": ["/tmp/exo-process-bridge.py", "client", "{\"type\":\"ping\"}"],
                        "cwd": "/vercel/sandbox",
                        "sandboxId": "sandbox-id",
                        "exitCode": 0,
                        "startedAt": 1
                    }
                })
                .to_string(),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v2/sandboxes/sessions/sess_process_busy/cmd/cmd_ping/logs",
        ))
        .and(query_param("teamId", "team_1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(json!({"stream": "stdout", "data": "{\"ok\":true}"}).to_string()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-6", "sandbox-6"))
        .await
        .unwrap();
    let result = handle
        .start_process(&SandboxCommand {
            argv: vec!["codex".into(), "app-server".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await;
    let error = match result {
        Ok(_) => panic!("start_process should reject a second active bridge"),
        Err(error) => error,
    };
    let error = format!("{error:#}");
    assert!(
        error.contains("only one active long-running process"),
        "unexpected error: {error}"
    );

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests
            .iter()
            .any(|request| String::from_utf8_lossy(&request.body).contains("pkill")),
        "second start_process must not stop the existing Vercel bridge"
    );
}

fn body_creates_named_sandbox() -> impl wiremock::Match {
    struct Has;
    impl wiremock::Match for Has {
        fn matches(&self, request: &Request) -> bool {
            let Ok(body) = serde_json::from_slice::<VercelCreateBody>(&request.body) else {
                return false;
            };
            body.project_id == "project_1"
                && body.name.starts_with("exo-")
                && body.runtime.as_deref() == Some("node24")
                && body.persistent == Some(true)
                && body.tags.contains_key("exo.sandbox.key")
                && body.tags.contains_key("exo.sandbox.spec-hash")
        }
    }
    Has
}

fn body_runs_command_with_env() -> impl wiremock::Match {
    struct Has;
    impl wiremock::Match for Has {
        fn matches(&self, request: &Request) -> bool {
            let Ok(body) = serde_json::from_slice::<VercelCommandBody>(&request.body) else {
                return false;
            };
            body.command == "bash"
                && body.args == ["-lc", "printf $OPENAI_API_KEY"]
                && body.cwd.as_deref() == Some("/vercel/sandbox")
                && body.env.get("OPENAI_API_KEY").map(String::as_str) == Some("sk-secret")
                && body.sudo == Some(false)
                && body.wait == Some(true)
        }
    }
    Has
}

fn body_runs_shell_command_containing(needle: &'static str) -> impl wiremock::Match {
    struct Has {
        needle: &'static str,
    }
    impl wiremock::Match for Has {
        fn matches(&self, request: &Request) -> bool {
            let Ok(body) = serde_json::from_slice::<VercelCommandBody>(&request.body) else {
                return false;
            };
            body.command == "/bin/sh"
                && body.args.first().map(String::as_str) == Some("-lc")
                && body
                    .args
                    .get(1)
                    .is_some_and(|command| command.contains(self.needle))
                && body.cwd.as_deref() == Some("/vercel/sandbox")
                && body.env.is_empty()
                && body.sudo == Some(false)
                && body.wait == Some(true)
        }
    }
    Has { needle }
}

fn body_runs_bridge_request(kind: &'static str) -> impl wiremock::Match {
    struct Has {
        kind: &'static str,
    }
    impl wiremock::Match for Has {
        fn matches(&self, request: &Request) -> bool {
            let Ok(body) = serde_json::from_slice::<VercelCommandBody>(&request.body) else {
                return false;
            };
            let Some(request_arg) = body.args.get(2) else {
                return false;
            };
            let Ok(bridge_request) = serde_json::from_str::<BridgeRequestBody>(request_arg) else {
                return false;
            };
            body.command == "python3"
                && body.args.first().map(String::as_str) == Some("/tmp/exo-process-bridge.py")
                && body.args.get(1).map(String::as_str) == Some("client")
                && bridge_request.kind == self.kind
                && body.cwd.as_deref() == Some("/vercel/sandbox")
                && body.env.is_empty()
                && body.sudo == Some(false)
                && body.wait == Some(true)
        }
    }
    Has { kind }
}

#[derive(Deserialize)]
struct VercelCreateBody {
    #[serde(rename = "projectId")]
    project_id: String,
    runtime: Option<String>,
    name: String,
    persistent: Option<bool>,
    tags: HashMap<String, String>,
}

#[derive(Deserialize)]
struct VercelCommandBody {
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: HashMap<String, String>,
    sudo: Option<bool>,
    wait: Option<bool>,
}

#[derive(Deserialize)]
struct BridgeRequestBody {
    #[serde(rename = "type")]
    kind: String,
}
