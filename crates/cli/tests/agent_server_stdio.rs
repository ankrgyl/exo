use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::value::RawValue;
use tempfile::TempDir;

fn exo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_exo"))
}

fn exo_command(root: &TempDir, xdg: &TempDir, pricing_path: &std::path::Path) -> Command {
    let mut command = Command::new(exo_bin());
    command
        .args(["--root", root.path().to_str().expect("utf-8 root")])
        .args(["--secret-backend", "file"])
        .args([
            "--pricing-path",
            pricing_path.to_str().expect("utf-8 pricing path"),
        ])
        .env("XDG_CONFIG_HOME", xdg.path());
    command
}

#[test]
fn agent_server_stdout_is_protocol_only_jsonl() {
    let root = TempDir::new().expect("temporary exo root");
    let xdg = TempDir::new().expect("temporary config root");
    let pricing_path = root.path().join("pricing.json");
    std::fs::write(&pricing_path, "{}").expect("write empty pricing table");

    let create_agent = exo_command(&root, &xdg, &pricing_path)
        .args([
            "agent",
            "create",
            "Basic Agent",
            "--slug",
            "basic-agent",
            "--provider",
            "local-process",
            "--model",
            "missing-model",
        ])
        .output()
        .expect("create basic agent");
    assert!(
        create_agent.status.success(),
        "agent create failed: {}",
        String::from_utf8_lossy(&create_agent.stderr)
    );
    let create_conversation = exo_command(&root, &xdg, &pricing_path)
        .args([
            "conversation",
            "create",
            "basic-agent",
            "Development",
            "--slug",
            "dev",
        ])
        .output()
        .expect("create basic conversation");
    assert!(
        create_conversation.status.success(),
        "conversation create failed: {}",
        String::from_utf8_lossy(&create_conversation.stderr)
    );

    let mut child = exo_command(&root, &xdg, &pricing_path)
        .arg("agent-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn exo agent-server");

    let mut stdin = child.stdin.take().expect("piped stdin");
    writeln!(stdin, "{{").expect("write malformed request");
    stdin
        .write_all(&[0xff, b'\n'])
        .expect("write invalid UTF-8 request");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":null,"method":"initialize","params":{{"protocol_version":1}}}}"#
    )
    .expect("write initialize request");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"agent/list","params":{{}}}}"#
    )
    .expect("write agent list request");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"agent_ref":"basic-agent","conversation_ref":"dev","input":[{{"role":"user","content":"hello"}}],"session_id":null}}}}"#
    )
    .expect("write failing turn request");
    stdin.flush().expect("flush requests");

    let stdout = child.stdout.take().expect("piped stdout");
    let mut frames = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.expect("read stdout line");
        let frame = serde_json::from_str::<Frame>(&line).expect("stdout line must be JSON-RPC");
        let terminal = frame.method.as_deref() == Some("turn/failed");
        frames.push(frame);
        if terminal {
            break;
        }
    }
    drop(stdin);

    let status = child.wait().expect("wait for agent server");
    assert!(status.success(), "agent server failed with status {status}");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("piped stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    assert_eq!(frames.len(), 7);
    assert!(frames.iter().all(|frame| frame.jsonrpc == "2.0"));
    assert_eq!(
        frames[0].error.as_ref().map(|error| error.code),
        Some(-32700)
    );
    assert_eq!(
        frames[1].error.as_ref().map(|error| error.code),
        Some(-32700)
    );
    assert!(frames[2].result.is_some());
    assert!(frames[3].result.is_some());
    assert_eq!(frames[6].method.as_deref(), Some("turn/failed"));
    assert!(
        stderr.contains("agent server executor failure"),
        "missing default error diagnostic: {stderr}"
    );
}

#[test]
fn basic_agent_server_rejects_a_persisted_rlm_agent_turn() {
    let root = TempDir::new().expect("temporary exo root");
    let xdg = TempDir::new().expect("temporary config root");
    let pricing_path = root.path().join("pricing.json");
    std::fs::write(&pricing_path, "{}").expect("write empty pricing table");

    let create_agent = exo_command(&root, &xdg, &pricing_path)
        .args([
            "--harness",
            "rlm",
            "agent",
            "create",
            "RLM Agent",
            "--slug",
            "rlm-agent",
            "--provider",
            "local-process",
            "--model",
            "unused-model",
        ])
        .output()
        .expect("create RLM agent");
    assert!(
        create_agent.status.success(),
        "agent create failed: {}",
        String::from_utf8_lossy(&create_agent.stderr)
    );
    let create_conversation = exo_command(&root, &xdg, &pricing_path)
        .args([
            "conversation",
            "create",
            "rlm-agent",
            "Development",
            "--slug",
            "dev",
        ])
        .output()
        .expect("create RLM conversation");
    assert!(
        create_conversation.status.success(),
        "conversation create failed: {}",
        String::from_utf8_lossy(&create_conversation.stderr)
    );

    let mut server = exo_command(&root, &xdg, &pricing_path)
        .arg("agent-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn basic agent server");
    let mut stdin = server.stdin.take().expect("piped stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":1}}}}"#
    )
    .expect("write initialize request");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"turn/start","params":{{"agent_ref":"rlm-agent","conversation_ref":"dev","input":[{{"role":"user","content":"hello"}}],"session_id":null}}}}"#
    )
    .expect("write turn request");
    drop(stdin);

    let output = server.wait_with_output().expect("wait for agent server");
    assert!(
        output.status.success(),
        "agent server failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let frames = String::from_utf8(output.stdout)
        .expect("utf-8 stdout")
        .lines()
        .map(|line| serde_json::from_str::<Frame>(line).expect("JSON-RPC frame"))
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 2);
    let error = frames[1].error.as_ref().expect("turn should be rejected");
    assert_eq!(error.code, -32006);
    assert_eq!(
        error.data.as_ref().map(|data| data.kind.as_str()),
        Some("incompatible_harness")
    );
}

#[derive(Deserialize)]
struct Frame {
    jsonrpc: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    result: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<ErrorFrame>,
}

#[derive(Deserialize)]
struct ErrorFrame {
    code: i32,
    #[serde(default)]
    data: Option<ErrorData>,
}

#[derive(Deserialize)]
struct ErrorData {
    kind: String,
}
