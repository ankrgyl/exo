//! Integration test exercising the real `exo` binary against:
//!   - a real sandbox provider (local-process / docker / apple-container), and
//!   - a wiremock-backed fake OpenAI Responses endpoint.
//!
//! `#[ignore]`'d so `cargo test` skips it by default; the integration workflow
//! runs `cargo test --workspace -- --ignored` and selects the provider via the
//! `EXO_TEST_SANDBOX_BACKEND` env var (defaults to `docker`), the same
//! variable the workflow matrix sets and `snapshot_round_trip.rs` reads. The
//! secret backend is always `file`, with the master key materialised inside a
//! per-test tempdir via `XDG_CONFIG_HOME`.

use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SandboxProvider {
    LocalProcess,
    Docker,
    AppleContainer,
}

impl SandboxProvider {
    fn from_env() -> Self {
        let raw = std::env::var("EXO_TEST_SANDBOX_BACKEND").unwrap_or_else(|_| "docker".into());
        match raw.as_str() {
            "local-process" => Self::LocalProcess,
            "docker" => Self::Docker,
            "apple-container" => Self::AppleContainer,
            other => panic!("unknown EXO_TEST_SANDBOX_BACKEND={other}"),
        }
    }

    fn cli_arg(self) -> &'static str {
        match self {
            Self::LocalProcess => "local-process",
            Self::Docker => "docker",
            Self::AppleContainer => "apple-container",
        }
    }

    fn runtime_available(self) -> bool {
        match self {
            Self::LocalProcess => true,
            Self::Docker => Command::new("docker")
                .arg("info")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
            Self::AppleContainer => Command::new("container")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
        }
    }
}

fn exo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_exo"))
}

fn run_exo(args: &[&str], root: &str, xdg: &str) -> std::process::Output {
    let output = Command::new(exo_bin())
        .args(["--root", root])
        .args(["--secret-backend", "file"])
        .args(args)
        .env("XDG_CONFIG_HOME", xdg)
        .env("OPENAI_API_KEY", "sk-test-key")
        .output()
        .expect("failed to spawn exo");
    if !output.status.success() {
        panic!(
            "exo {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    output
}

fn canned_response_body() -> Value {
    json!({
        "id": "resp_test_abc123",
        "object": "response",
        "status": "completed",
        "created_at": 1_700_000_000_u64,
        "model": "gpt-test",
        "output": [
            {
                "type": "message",
                "id": "msg_test_xyz",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": "Hello from the mock OpenAI server.",
                        "annotations": []
                    }
                ]
            }
        ],
        "usage": {
            "input_tokens": 5,
            "output_tokens": 7,
            "total_tokens": 12
        }
    })
}

#[tokio::test]
#[ignore = "spawns real exo binary + real sandbox + wiremock; run with cargo test -- --ignored"]
async fn conversation_send_round_trips_through_real_sandbox_and_mocked_openai() {
    let provider = SandboxProvider::from_env();
    if !provider.runtime_available() {
        println!(
            "sandbox provider {:?} not available on this runner, skipping",
            provider
        );
        return;
    }

    let root_dir = TempDir::new().expect("tempdir for --root");
    let xdg_dir = TempDir::new().expect("tempdir for XDG_CONFIG_HOME");
    let root = root_dir.path().to_string_lossy().into_owned();
    let xdg = xdg_dir.path().to_string_lossy().into_owned();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(canned_response_body()))
        .mount(&mock_server)
        .await;

    run_exo(
        &["secret", "set", "test-key", "--env", "OPENAI_API_KEY"],
        &root,
        &xdg,
    );
    run_exo(
        &[
            "model",
            "register",
            "gpt-test",
            "--secret",
            "test-key",
            "--base-url",
            &mock_server.uri(),
        ],
        &root,
        &xdg,
    );
    run_exo(
        &[
            "agent",
            "create",
            "--slug",
            "test-agent",
            "--model",
            "gpt-test",
            "--provider",
            provider.cli_arg(),
            "Integration Test Agent",
        ],
        &root,
        &xdg,
    );
    run_exo(
        &["conversation", "create", "test-agent", "first"],
        &root,
        &xdg,
    );

    let output = run_exo(
        &["conversation", "send", "test-agent", "first", "hello there"],
        &root,
        &xdg,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Hello from the mock OpenAI server."),
        "expected mocked assistant text in stdout; got: {stdout}"
    );

    let recorded = mock_server.received_requests().await.unwrap_or_default();
    let responses_calls = recorded
        .iter()
        .filter(|r| r.url.path() == "/responses")
        .count();
    assert_eq!(
        responses_calls,
        1,
        "expected exactly one POST /responses; got {responses_calls} (all paths: {:?})",
        recorded
            .iter()
            .map(|r| r.url.path().to_string())
            .collect::<Vec<_>>()
    );

    // `agents/` holds the `by-slug/` index next to the agent-id dirs, and
    // readdir order is not deterministic — pick the entry that actually has a
    // `conversations` subdir instead of whatever comes back first.
    let conv_root = root_dir
        .path()
        .join("exoharness/agents")
        .read_dir()
        .expect("agents dir exists")
        .flatten()
        .map(|entry| entry.path().join("conversations"))
        .find(|path| path.is_dir())
        .expect("at least one agent with a conversations dir");
    let conv_dir = conv_root
        .read_dir()
        .expect("conversations dir exists")
        .next()
        .expect("at least one conversation")
        .unwrap()
        .path();
    let events_dir = conv_dir.join("events");
    let mut found_assistant_text = false;
    for entry in events_dir.read_dir().expect("events dir exists").flatten() {
        let raw = std::fs::read(entry.path()).expect("event file readable");
        let event: Value = serde_json::from_slice(&raw).expect("event is valid json");
        let Some(messages) = event
            .pointer("/data/messages")
            .and_then(Value::as_array)
            .cloned()
        else {
            continue;
        };
        for message in messages {
            if message.get("role").and_then(Value::as_str) == Some("assistant") {
                let text = serde_json::to_string(&message).unwrap_or_default();
                if text.contains("Hello from the mock OpenAI server.") {
                    found_assistant_text = true;
                }
            }
        }
    }
    assert!(
        found_assistant_text,
        "expected mocked assistant text in persisted events under {}",
        events_dir.display()
    );

    if provider == SandboxProvider::Docker {
        let leftover_containers = Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                "label=exo.sandbox.owner-pid",
                "--filter",
                "status=exited",
            ])
            .output()
            .expect("docker ps");
        let stdout = String::from_utf8_lossy(&leftover_containers.stdout);
        let stale = stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>();
        assert!(
            stale.is_empty(),
            "expected zero leftover Exited exo containers after binary exit; found: {stale:?}"
        );
    }
}
