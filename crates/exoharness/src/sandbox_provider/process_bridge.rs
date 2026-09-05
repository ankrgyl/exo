use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::task::AbortOnDropHandle;

const PROCESS_PIPE_BUFFER_SIZE: usize = 64 * 1024;
pub const PATH: &str = "/tmp/exo-process-bridge.py";
pub const RECV_TIMEOUT: Duration = Duration::from_secs(30);

pub const SCRIPT: &str = r###"
import base64
import json
import os
import queue
import socket
import socketserver
import subprocess
import sys
import threading

HOST = "127.0.0.1"
PORT = 48765
LOG_PATH = "/tmp/exo-process-bridge.log"
MAX_RECV_EVENTS = 64
MAX_RECV_BYTES = 1024 * 1024


def log(message):
    with open(LOG_PATH, "a", buffering=1) as bridge_log:
        print(message, file=bridge_log)


def decode_env_json():
    raw = os.environ.get("EXO_PROCESS_BRIDGE_ENV_JSON")
    if not raw:
        return {}
    decoded = json.loads(raw)
    if not isinstance(decoded, dict):
        raise RuntimeError("EXO_PROCESS_BRIDGE_ENV_JSON must be an object")
    return {str(key): str(value) for key, value in decoded.items()}


def decode_argv_json():
    raw = os.environ.get("EXO_PROCESS_BRIDGE_ARGV_JSON")
    if not raw:
        raise RuntimeError("EXO_PROCESS_BRIDGE_ARGV_JSON is required")
    decoded = json.loads(raw)
    if not isinstance(decoded, list) or not decoded:
        raise RuntimeError("EXO_PROCESS_BRIDGE_ARGV_JSON must be a non-empty array")
    return [str(value) for value in decoded]


class BridgeState:
    def __init__(self):
        argv = decode_argv_json()
        cwd = os.environ.get("EXO_PROCESS_BRIDGE_CWD") or None
        env = os.environ.copy()
        env.update(decode_env_json())
        log("starting process bridge command")
        self.process = subprocess.Popen(
            argv,
            cwd=cwd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        self.events = queue.Queue()
        self.write_lock = threading.Lock()
        self.stdout_thread = threading.Thread(target=self._read_stream, args=("stdout", self.process.stdout), daemon=True)
        self.stderr_thread = threading.Thread(target=self._read_stream, args=("stderr", self.process.stderr), daemon=True)
        self.stdout_thread.start()
        self.stderr_thread.start()
        threading.Thread(target=self._wait, daemon=True).start()

    def _read_stream(self, stream, handle):
        try:
            while True:
                data = handle.read(65536)
                if not data:
                    break
                self.events.put({
                    "type": stream,
                    "data": base64.b64encode(data).decode("ascii"),
                })
        except Exception as exc:
            self.events.put({"type": "error", "message": f"{stream} reader failed: {exc}"})

    def _wait(self):
        try:
            exit_code = self.process.wait()
            self.stdout_thread.join()
            self.stderr_thread.join()
            self.events.put({"type": "exit", "exit_code": exit_code})
        except Exception as exc:
            self.events.put({"type": "error", "message": f"wait failed: {exc}"})

    def write(self, data):
        if self.process.poll() is not None:
            raise RuntimeError(f"bridged process is not running: {self.process.returncode}")
        payload = base64.b64decode(data.encode("ascii"))
        with self.write_lock:
            self.process.stdin.write(payload)
            self.process.stdin.flush()

    def close_stdin(self):
        with self.write_lock:
            if self.process.stdin:
                self.process.stdin.close()

    def recv(self, timeout):
        try:
            event = self.events.get(timeout=timeout)
        except queue.Empty:
            return {"ok": True, "timeout": True}
        events = []
        total_bytes = 0
        while True:
            if event.get("type") == "error":
                raise RuntimeError(event.get("message") or "bridged process failed")
            events.append(event)
            total_bytes += len(event.get("data", ""))
            if event.get("type") == "exit":
                break
            if len(events) >= MAX_RECV_EVENTS or total_bytes >= MAX_RECV_BYTES:
                break
            try:
                event = self.events.get_nowait()
            except queue.Empty:
                break
        return {"ok": True, "events": events}


STATE = None


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        global STATE
        data = b""
        while True:
            chunk = self.request.recv(1024 * 1024)
            if not chunk:
                break
            data += chunk
        try:
            request = json.loads(data.decode("utf-8") or "{}")
            kind = request.get("type")
            if kind == "ping":
                response = {"ok": True}
            elif kind == "write":
                STATE.write(request["data"])
                response = {"ok": True}
            elif kind == "close_stdin":
                STATE.close_stdin()
                response = {"ok": True}
            elif kind == "recv":
                response = STATE.recv(float(request.get("timeout_seconds", 30)))
            else:
                response = {"ok": False, "error": f"unknown bridge request type: {kind}"}
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        self.request.sendall(json.dumps(response, separators=(",", ":")).encode("utf-8"))
        if any(event.get("type") == "exit" for event in response.get("events", [])):
            threading.Thread(target=self.server.shutdown, daemon=True).start()


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True


def server():
    global STATE
    log("starting exo process bridge")
    STATE = BridgeState()
    with Server((HOST, PORT), Handler) as srv:
        srv.serve_forever()


def client():
    request = sys.argv[2] if len(sys.argv) > 2 else os.environ.get("EXO_PROCESS_BRIDGE_REQUEST")
    if not request:
        raise SystemExit("EXO_PROCESS_BRIDGE_REQUEST is required")
    with socket.create_connection((HOST, PORT), timeout=35) as sock:
        sock.sendall(request.encode("utf-8"))
        sock.shutdown(socket.SHUT_WR)
        chunks = []
        while True:
            chunk = sock.recv(1024 * 1024)
            if not chunk:
                break
            chunks.append(chunk)
    sys.stdout.write(b"".join(chunks).decode("utf-8"))


def ping():
    request = json.dumps({"type": "ping"}, separators=(",", ":"))
    os.environ["EXO_PROCESS_BRIDGE_REQUEST"] = request
    client()


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "client"
    if mode == "server":
        server()
    elif mode == "ping":
        ping()
    elif mode == "client":
        client()
    else:
        raise SystemExit(f"unknown mode: {mode}")
"###;

#[derive(Debug, Clone, Serialize)]
pub struct Request {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<crate::SandboxTerminalSize>,
}

impl Request {
    pub fn ping() -> Self {
        Self {
            kind: "ping",
            data: None,
            timeout_seconds: None,
            size: None,
        }
    }

    fn recv() -> Self {
        Self {
            kind: "recv",
            data: None,
            timeout_seconds: Some(RECV_TIMEOUT.as_secs_f64()),
            size: None,
        }
    }

    fn write(data: &[u8]) -> Self {
        Self {
            kind: "write",
            data: Some(STANDARD.encode(data)),
            timeout_seconds: None,
            size: None,
        }
    }

    fn close_stdin() -> Self {
        Self {
            kind: "close_stdin",
            data: None,
            timeout_seconds: None,
            size: None,
        }
    }

    fn resize(size: crate::SandboxTerminalSize) -> Self {
        Self {
            kind: "resize",
            data: None,
            timeout_seconds: None,
            size: Some(size),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub timeout: bool,
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Stdout { data: String },
    Stderr { data: String },
    Exit { exit_code: i32 },
}

#[async_trait]
pub trait Client: Send + Sync + 'static {
    async fn request(&self, request: Request) -> Result<Response>;
}

pub fn install_script_shell_command() -> String {
    format!(
        "python3 -c {} {} {}",
        super::shell_quote(
            "import pathlib,sys; path=pathlib.Path(sys.argv[1]); path.write_text(sys.argv[2]); path.chmod(0o700)"
        ),
        super::shell_quote(PATH),
        super::shell_quote(SCRIPT),
    )
}

pub fn stop_shell_command() -> String {
    format!(
        "pkill -f {} >/dev/null 2>&1 || true",
        super::shell_quote("[e]xo-process-bridge.py"),
    )
}

pub fn server_shell_command() -> String {
    format!("python3 {} server", super::shell_quote(PATH))
}

pub fn client_argv(request_json: String) -> Vec<String> {
    vec![
        "python3".to_string(),
        PATH.to_string(),
        "client".to_string(),
        request_json,
    ]
}

pub fn process_parts(client: Arc<dyn Client>) -> crate::SandboxProcessParts {
    let (stdout_reader, stdout_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stderr_reader, stderr_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stdin_reader, stdin_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (wait_tx, wait_rx) = oneshot::channel();
    let (error_tx, mut error_rx) = mpsc::unbounded_channel();

    let output_poller = spawn_output_poller(
        Arc::clone(&client),
        stdout_writer,
        stderr_writer,
        wait_tx,
        error_tx.clone(),
    );
    let stdin_forwarder = spawn_stdin_forwarder(client, stdin_reader, error_tx);
    let tasks = [output_poller, stdin_forwarder].map(AbortOnDropHandle::new);

    let wait: BoxFuture<'static, crate::Result<i32>> = Box::pin(async move {
        let _tasks = tasks;
        tokio::pin!(wait_rx);
        let mut errors_open = true;
        loop {
            tokio::select! {
                result = &mut wait_rx => {
                    return match result {
                        Ok(exit_code) => Ok(exit_code),
                        Err(_) => Err(anyhow::anyhow!("process bridge output poller stopped")),
                    };
                }
                error = error_rx.recv(), if errors_open => match error {
                    Some(error) => return Err(error),
                    None => errors_open = false,
                },
            }
        }
    });

    crate::SandboxProcessParts {
        stdout: Box::pin(stdout_reader.compat()),
        stderr: Box::pin(stderr_reader.compat()),
        stdin: Box::pin(stdin_writer.compat_write()),
        wait,
    }
}

pub fn terminal_parts(client: Arc<dyn Client>) -> crate::SandboxTerminalParts {
    let crate::SandboxProcessParts {
        stdout,
        stderr,
        stdin,
        wait,
    } = process_parts(Arc::clone(&client));
    drop(stderr);
    crate::SandboxTerminalParts {
        output: stdout,
        input: stdin,
        wait,
        control: Arc::new(ProcessBridgeTerminalControl { client }),
    }
}

struct ProcessBridgeTerminalControl {
    client: Arc<dyn Client>,
}

#[async_trait]
impl crate::SandboxTerminalControl for ProcessBridgeTerminalControl {
    async fn resize(&self, size: crate::SandboxTerminalSize) -> crate::Result<()> {
        let response = self.client.request(Request::resize(size)).await?;
        if response.ok {
            return Ok(());
        }
        anyhow::bail!(
            "resizing sandbox terminal failed: {}",
            response.error.as_deref().unwrap_or("unknown error")
        )
    }
}

fn spawn_output_poller(
    client: Arc<dyn Client>,
    stdout_writer: tokio::io::DuplexStream,
    stderr_writer: tokio::io::DuplexStream,
    wait_tx: oneshot::Sender<i32>,
    error_tx: mpsc::UnboundedSender<anyhow::Error>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = poll_output(client, stdout_writer, stderr_writer, wait_tx).await
            && error_tx.send(error).is_err()
        {
            tracing::debug!("process bridge waiter stopped before output error could be reported");
        }
    })
}

fn spawn_stdin_forwarder(
    client: Arc<dyn Client>,
    stdin_reader: tokio::io::DuplexStream,
    error_tx: mpsc::UnboundedSender<anyhow::Error>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = forward_stdin(client, stdin_reader).await
            && error_tx.send(error).is_err()
        {
            tracing::debug!("process bridge waiter stopped before stdin error could be reported");
        }
    })
}

async fn poll_output(
    client: Arc<dyn Client>,
    mut stdout_writer: tokio::io::DuplexStream,
    mut stderr_writer: tokio::io::DuplexStream,
    wait_tx: oneshot::Sender<i32>,
) -> Result<()> {
    loop {
        let response = client.request(Request::recv()).await?;
        if response.timeout {
            continue;
        }
        for event in response.events {
            match event {
                Event::Stdout { data } => {
                    write_bridge_output(&mut stdout_writer, &data, "stdout").await?;
                }
                Event::Stderr { data } => {
                    write_bridge_output(&mut stderr_writer, &data, "stderr").await?;
                }
                Event::Exit { exit_code } => {
                    if wait_tx.send(exit_code).is_err() {
                        tracing::debug!(
                            "process bridge waiter dropped before exit could be reported"
                        );
                    }
                    return Ok(());
                }
            }
        }
    }
}

async fn write_bridge_output(
    writer: &mut tokio::io::DuplexStream,
    data: &str,
    stream: &str,
) -> Result<()> {
    let decoded = STANDARD
        .decode(data)
        .with_context(|| format!("decoding process bridge {stream}"))?;
    match writer.write_all(&decoded).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("writing process bridge {stream}"));
        }
    }
    match writer.flush().await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error).with_context(|| format!("flushing process bridge {stream}")),
    }
}

async fn forward_stdin(
    client: Arc<dyn Client>,
    mut stdin_reader: tokio::io::DuplexStream,
) -> Result<()> {
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let bytes_read = stdin_reader
            .read(&mut buffer)
            .await
            .context("reading process bridge stdin")?;
        if bytes_read == 0 {
            client.request(Request::close_stdin()).await?;
            return Ok(());
        }
        client
            .request(Request::write(&buffer[..bytes_read]))
            .await
            .context("writing process bridge stdin")?;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::Arc;

    use futures::AsyncReadExt;
    use serde_json::json;
    use tempfile::{TempDir, tempdir};
    use tokio::process::Command;
    use tokio::sync::Mutex;

    use super::*;

    struct FakeClient {
        responses: Mutex<VecDeque<Response>>,
    }

    struct BlockingClient {
        recv_started: mpsc::UnboundedSender<()>,
        recv_request: Mutex<Option<oneshot::Receiver<()>>>,
    }

    #[async_trait]
    impl Client for FakeClient {
        async fn request(&self, request: Request) -> Result<Response> {
            if request.kind != "recv" {
                return Ok(Response {
                    ok: true,
                    timeout: false,
                    events: Vec::new(),
                    error: None,
                });
            }
            let mut responses = self.responses.lock().await;
            Ok(responses.pop_front().unwrap_or(Response {
                ok: true,
                timeout: true,
                events: Vec::new(),
                error: None,
            }))
        }
    }

    #[async_trait]
    impl Client for BlockingClient {
        async fn request(&self, request: Request) -> Result<Response> {
            assert_eq!(request.kind, "recv");
            self.recv_started
                .send(())
                .expect("receive recv-start notification");
            self.recv_request
                .lock()
                .await
                .take()
                .expect("recv request should only start once")
                .await
                .expect("recv request should remain blocked");
            unreachable!("recv request unexpectedly completed")
        }
    }

    struct BridgeFixture {
        _temp: TempDir,
        script_path: PathBuf,
    }

    #[test]
    fn resize_request_matches_guest_protocol() {
        let request = Request::resize(crate::SandboxTerminalSize { rows: 24, cols: 80 });
        assert_eq!(
            serde_json::to_value(request).expect("serialize resize request"),
            json!({"type": "resize", "size": {"rows": 24, "cols": 80}})
        );
    }

    #[tokio::test]
    async fn process_parts_writes_batched_stdout_and_exit() {
        let client = FakeClient {
            responses: Mutex::new(VecDeque::from([Response {
                ok: true,
                timeout: false,
                events: vec![
                    Event::Stdout {
                        data: STANDARD.encode("hello "),
                    },
                    Event::Stdout {
                        data: STANDARD.encode("world"),
                    },
                    Event::Exit { exit_code: 0 },
                ],
                error: None,
            }])),
        };
        let parts = process_parts(Arc::new(client));
        let mut stdout = parts.stdout;
        let exit_code = parts.wait.await.expect("bridge wait should succeed");
        let mut output = String::new();
        stdout
            .read_to_string(&mut output)
            .await
            .expect("read bridge stdout");

        assert_eq!(exit_code, 0);
        assert_eq!(output, "hello world");
    }

    #[tokio::test]
    async fn dropping_wait_cancels_output_polling() {
        let (recv_started_tx, mut recv_started_rx) = mpsc::unbounded_channel();
        let (mut recv_request_tx, recv_request_rx) = oneshot::channel();
        let client = Arc::new(BlockingClient {
            recv_started: recv_started_tx,
            recv_request: Mutex::new(Some(recv_request_rx)),
        });
        let parts = process_parts(client.clone());

        tokio::time::timeout(Duration::from_secs(1), recv_started_rx.recv())
            .await
            .expect("output poll should start")
            .expect("recv-start channel should remain open");
        drop(parts.wait);
        tokio::time::timeout(Duration::from_secs(1), recv_request_tx.closed())
            .await
            .expect("in-flight recv should be cancelled");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), recv_started_rx.recv())
                .await
                .is_err(),
            "output poller started another recv request"
        );
    }

    #[tokio::test]
    async fn bridge_reports_stdout_before_exit_for_fast_process() {
        let bridge =
            start_bridge_for_python_command("import sys; sys.stdout.write('final output')").await;

        let mut output = String::new();
        let exit_code = recv_until_exit(&bridge.script_path, &mut output).await;

        assert_eq!(exit_code, 0);
        assert_eq!(output, "final output");
    }

    #[tokio::test]
    async fn bridge_server_stops_after_exit_is_observed() {
        let bridge = start_bridge_for_python_command("import sys; sys.stdout.write('done')").await;
        let mut output = String::new();
        assert_eq!(recv_until_exit(&bridge.script_path, &mut output).await, 0);

        tokio::time::sleep(Duration::from_millis(100)).await;
        let ping = bridge_ping(&bridge.script_path).await;

        assert!(
            !ping.status.success(),
            "bridge server still accepted ping after child exit"
        );
    }

    async fn start_bridge_for_python_command(source: &str) -> BridgeFixture {
        let port = unused_local_port();
        let temp = tempdir().expect("create temp dir");
        let script_path = temp.path().join("exo-process-bridge.py");
        let script = SCRIPT.replace("PORT = 48765", &format!("PORT = {port}"));
        tokio::fs::write(&script_path, script)
            .await
            .expect("write bridge script");

        let argv_json = serde_json::to_string(&vec!["python3", "-c", source]).expect("encode argv");
        let mut server = Command::new("python3")
            .arg(&script_path)
            .arg("server")
            .env("EXO_PROCESS_BRIDGE_ARGV_JSON", argv_json)
            .env("EXO_PROCESS_BRIDGE_ENV_JSON", "{}")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start bridge server");

        wait_for_ping(&script_path).await;
        tokio::spawn(async move {
            match server.wait().await {
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "process bridge test server wait failed"),
            }
        });
        BridgeFixture {
            _temp: temp,
            script_path,
        }
    }

    async fn recv_until_exit(script_path: &Path, output: &mut String) -> i32 {
        for _ in 0..10 {
            let response = bridge_client(
                script_path,
                json!({"type": "recv", "timeout_seconds": 1.0}).to_string(),
            )
            .await;
            let response: Response =
                serde_json::from_slice(&response.stdout).expect("decode bridge recv response");
            for event in response.events {
                match event {
                    Event::Stdout { data } => {
                        let decoded = STANDARD.decode(data).expect("decode stdout event");
                        output.push_str(
                            std::str::from_utf8(&decoded).expect("stdout event should be utf8"),
                        );
                    }
                    Event::Stderr { data } => {
                        let decoded = STANDARD.decode(data).expect("decode stderr event");
                        panic!(
                            "unexpected stderr event: {}",
                            String::from_utf8_lossy(&decoded)
                        );
                    }
                    Event::Exit { exit_code } => return exit_code,
                }
            }
        }
        panic!("bridge did not report process exit");
    }

    async fn wait_for_ping(script_path: &Path) {
        for _ in 0..50 {
            if bridge_ping(script_path).await.status.success() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("bridge server did not become ready");
    }

    async fn bridge_ping(script_path: &Path) -> std::process::Output {
        Command::new("python3")
            .arg(script_path)
            .arg("ping")
            .output()
            .await
            .expect("run bridge ping")
    }

    async fn bridge_client(script_path: &Path, request: String) -> std::process::Output {
        let output = Command::new("python3")
            .arg(script_path)
            .arg("client")
            .arg(request)
            .output()
            .await
            .expect("run bridge client");
        assert!(
            output.status.success(),
            "bridge client failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn unused_local_port() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local port");
        listener.local_addr().expect("read local addr").port()
    }
}
