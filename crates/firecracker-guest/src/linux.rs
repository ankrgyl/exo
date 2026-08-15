use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::chown;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

const AGENT_PORT: u32 = 10_052;
const READY_HOST_PORT: u32 = 10_053;
const GUEST_UID: u32 = 10_001;
const GUEST_GID: u32 = 10_001;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 32;
const MAX_PROCESSES: usize = 128;
const MAX_RECV_EVENTS: usize = 64;
const MAX_RECV_BYTES: usize = 1024 * 1024;
const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Ping,
    Exec {
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: String,
        timeout_ms: Option<u64>,
    },
    StartProcess {
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: String,
    },
    ProcessBridge {
        process_id: String,
        request: BridgeRequest,
    },
    KillProcess {
        process_id: String,
    },
    SyncFilesystem {
        path: String,
    },
    ConfigureNetwork {
        address: Ipv4Addr,
        gateway: Ipv4Addr,
        prefix: u8,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BridgeRequest {
    Ping,
    Write { data: String },
    CloseStdin,
    Recv { timeout_seconds: Option<f64> },
}

#[derive(Debug, Default, Serialize)]
struct Response {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    process_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    events: Option<Vec<ProcessEvent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Response {
    fn ok() -> Self {
        Self {
            ok: true,
            ..Self::default()
        }
    }

    fn error(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ProcessEvent {
    Stdout { data: String },
    Stderr { data: String },
    Exit { exit_code: i32 },
    Error { message: String },
}

impl ProcessEvent {
    fn payload_len(&self) -> usize {
        match self {
            Self::Stdout { data } | Self::Stderr { data } => data.len(),
            Self::Error { message } => message.len(),
            Self::Exit { .. } => 0,
        }
    }

    fn is_exit(&self) -> bool {
        matches!(self, Self::Exit { .. })
    }
}

#[derive(Default)]
struct EventQueueState {
    events: VecDeque<ProcessEvent>,
    bytes: usize,
    overflowed: bool,
}

#[derive(Default)]
struct EventQueue {
    state: Mutex<EventQueueState>,
    ready: Condvar,
}

impl EventQueue {
    fn push(&self, event: ProcessEvent) {
        let mut state = self.state.lock().expect("event queue lock poisoned");
        if !event.is_exit() && state.overflowed {
            return;
        }
        let event_bytes = event.payload_len();
        if !event.is_exit()
            && state
                .bytes
                .checked_add(event_bytes)
                .is_none_or(|bytes| bytes > MAX_QUEUED_BYTES)
        {
            state.events.clear();
            state.bytes = 0;
            state.overflowed = true;
            state.events.push_back(ProcessEvent::Error {
                message: "process output exceeded the guest queue limit".to_string(),
            });
            self.ready.notify_all();
            return;
        }
        state.bytes += event_bytes;
        state.events.push_back(event);
        self.ready.notify_all();
    }

    fn recv(&self, timeout: Duration) -> Result<(Vec<ProcessEvent>, bool), String> {
        let state = self.state.lock().map_err(|_| "event queue lock poisoned")?;
        let (mut state, _) = self
            .ready
            .wait_timeout_while(state, timeout, |state| state.events.is_empty())
            .map_err(|_| "event queue lock poisoned")?;
        if state.events.is_empty() {
            return Ok((Vec::new(), false));
        }

        let mut events = Vec::new();
        let mut bytes = 0usize;
        let mut exited = false;
        while events.len() < MAX_RECV_EVENTS && bytes < MAX_RECV_BYTES {
            let Some(event) = state.events.pop_front() else {
                break;
            };
            let event_bytes = event.payload_len();
            state.bytes = state.bytes.saturating_sub(event_bytes);
            if let ProcessEvent::Error { message } = event {
                return Err(message);
            }
            exited |= event.is_exit();
            bytes = bytes.saturating_add(event_bytes);
            events.push(event);
            if exited {
                break;
            }
        }
        Ok((events, exited))
    }
}

// Direct children whose exit status a dedicated wait thread will collect. The
// orphan reaper must leave these to their waiters and only reap processes that
// reparented to PID 1; stealing a tracked child's status would break exit-code
// reporting for the host.
static DIRECT_CHILDREN: Mutex<Vec<libc::pid_t>> = Mutex::new(Vec::new());

fn register_direct_child(pid: libc::pid_t) {
    DIRECT_CHILDREN
        .lock()
        .expect("direct child registry poisoned")
        .push(pid);
}

fn unregister_direct_child(pid: libc::pid_t) {
    DIRECT_CHILDREN
        .lock()
        .expect("direct child registry poisoned")
        .retain(|candidate| *candidate != pid);
}

fn is_direct_child(pid: libc::pid_t) -> bool {
    DIRECT_CHILDREN
        .lock()
        .expect("direct child registry poisoned")
        .contains(&pid)
}

// Keeps a pid registered for exactly as long as some code path still intends
// to wait on it, including early error returns, so the orphan reaper can never
// steal a tracked child's exit status.
struct DirectChildRegistration(libc::pid_t);

impl DirectChildRegistration {
    fn new(pid: libc::pid_t) -> Self {
        register_direct_child(pid);
        Self(pid)
    }
}

impl Drop for DirectChildRegistration {
    fn drop(&mut self) {
        unregister_direct_child(self.0);
    }
}

// This agent is PID 1, so every double-forked/daemonized workload descendant
// reparents to it when its parent dies. Without an init-style reaper those
// accumulate as zombies for the VM's lifetime, and enough of them exhaust the
// guest PID table — a denial of service a workload could trigger on purpose.
// WNOWAIT peeks without consuming so tracked children still deliver their real
// exit status to their wait threads.
fn reap_orphans() {
    loop {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result =
            unsafe { libc::waitid(libc::P_ALL, 0, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // ECHILD: nothing to wait for right now.
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let pid = unsafe { info.si_pid() };
        if pid <= 0 {
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        if is_direct_child(pid) {
            // A tracked child is momentarily waitable until its own wait
            // thread collects it; yield rather than stealing its status.
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        // Grace period closes the spawn-to-register window: a freshly spawned
        // direct child that exited immediately gets a chance to appear in the
        // registry before being treated as an orphan.
        thread::sleep(Duration::from_millis(100));
        if is_direct_child(pid) {
            continue;
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }
}

struct ManagedProcess {
    process_group: i32,
    stdin: Mutex<Option<ChildStdin>>,
    events: Arc<EventQueue>,
    exited: AtomicBool,
}

impl ManagedProcess {
    fn spawn(
        argv: &[String],
        cwd: &str,
        env: &HashMap<String, String>,
    ) -> Result<Arc<Self>, String> {
        validate_command(argv, cwd)?;
        let mut command = command(argv, cwd, env);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let process_group = i32::try_from(child.id()).map_err(|error| error.to_string())?;
        let registration = DirectChildRegistration::new(process_group);
        let stdin = child.stdin.take().ok_or("missing child stdin")?;
        let stdout = child.stdout.take().ok_or("missing child stdout")?;
        let stderr = child.stderr.take().ok_or("missing child stderr")?;
        let process = Arc::new(Self {
            process_group,
            stdin: Mutex::new(Some(stdin)),
            events: Arc::new(EventQueue::default()),
            exited: AtomicBool::new(false),
        });
        let stdout_thread = stream_thread(stdout, Arc::clone(&process.events), true);
        let stderr_thread = stream_thread(stderr, Arc::clone(&process.events), false);
        let wait_process = Arc::clone(&process);
        thread::spawn(move || {
            let result = child.wait();
            drop(registration);
            let stdout_result = stdout_thread.join();
            let stderr_result = stderr_thread.join();
            wait_process.exited.store(true, Ordering::Release);
            match result {
                Ok(status) if stdout_result.is_ok() && stderr_result.is_ok() => {
                    wait_process.events.push(ProcessEvent::Exit {
                        exit_code: exit_code(status),
                    });
                }
                Ok(_) => wait_process.events.push(ProcessEvent::Error {
                    message: "process output thread panicked".to_string(),
                }),
                Err(error) => wait_process.events.push(ProcessEvent::Error {
                    message: format!("waiting for process failed: {error}"),
                }),
            }
        });
        Ok(process)
    }

    fn write(&self, encoded: &str) -> Result<(), String> {
        if self.exited.load(Ordering::Acquire) {
            return Err("process is not running".to_string());
        }
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|error| format!("invalid process input: {error}"))?;
        let mut stdin = self.stdin.lock().map_err(|_| "stdin lock poisoned")?;
        let stdin = stdin.as_mut().ok_or("process stdin is closed")?;
        stdin.write_all(&bytes).map_err(|error| error.to_string())?;
        stdin.flush().map_err(|error| error.to_string())
    }

    fn close_stdin(&self) -> Result<(), String> {
        self.stdin.lock().map_err(|_| "stdin lock poisoned")?.take();
        Ok(())
    }

    fn kill(&self) -> Result<(), String> {
        if self.exited.load(Ordering::Acquire) {
            return Ok(());
        }
        // Every managed command is a process-group leader, so a kill cannot
        // leave attacker-controlled descendants behind in the microVM.
        let result = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(error.to_string())
    }
}

struct AgentState {
    processes: Mutex<HashMap<String, Arc<ManagedProcess>>>,
    next_process_id: AtomicU64,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
            next_process_id: AtomicU64::new(1),
        }
    }
}

impl AgentState {
    fn handle(&self, request: Request) -> Response {
        match request {
            Request::Ping => Response::ok(),
            Request::Exec {
                argv,
                env,
                cwd,
                timeout_ms,
            } => exec(&argv, &cwd, &env, timeout_ms),
            Request::StartProcess { argv, env, cwd } => self.start_process(&argv, &cwd, &env),
            Request::ProcessBridge {
                process_id,
                request,
            } => self.process_bridge(&process_id, request),
            Request::KillProcess { process_id } => self.kill_process(&process_id),
            Request::SyncFilesystem { path } => sync_filesystem(&path),
            Request::ConfigureNetwork {
                address,
                gateway,
                prefix,
            } => configure_restored_network(address, gateway, prefix),
        }
    }

    fn start_process(&self, argv: &[String], cwd: &str, env: &HashMap<String, String>) -> Response {
        let mut processes = match self.processes.lock() {
            Ok(processes) => processes,
            Err(_) => return Response::error("process map lock poisoned"),
        };
        if processes.len() >= MAX_PROCESSES {
            return Response::error("too many managed processes");
        }
        let process = match ManagedProcess::spawn(argv, cwd, env) {
            Ok(process) => process,
            Err(error) => return Response::error(error),
        };
        let sequence = self.next_process_id.fetch_add(1, Ordering::Relaxed);
        let process_id = format!("process-{sequence}");
        processes.insert(process_id.clone(), process);
        Response {
            ok: true,
            process_id: Some(process_id),
            ..Response::default()
        }
    }

    fn process_bridge(&self, process_id: &str, request: BridgeRequest) -> Response {
        let process = match self.processes.lock() {
            Ok(processes) => processes.get(process_id).cloned(),
            Err(_) => return Response::error("process map lock poisoned"),
        };
        let Some(process) = process else {
            return Response::error(format!("unknown process: {process_id}"));
        };
        match request {
            BridgeRequest::Ping => Response::ok(),
            BridgeRequest::Write { data } => match process.write(&data) {
                Ok(()) => Response::ok(),
                Err(error) => Response::error(error),
            },
            BridgeRequest::CloseStdin => match process.close_stdin() {
                Ok(()) => Response::ok(),
                Err(error) => Response::error(error),
            },
            BridgeRequest::Recv { timeout_seconds } => {
                let timeout = match recv_timeout(timeout_seconds) {
                    Ok(timeout) => timeout,
                    Err(error) => return Response::error(error),
                };
                match process.events.recv(timeout) {
                    Ok((events, exited)) => {
                        if exited {
                            match self.processes.lock() {
                                Ok(mut processes) => {
                                    processes.remove(process_id);
                                }
                                Err(_) => return Response::error("process map lock poisoned"),
                            }
                        }
                        Response {
                            ok: true,
                            timeout: events.is_empty().then_some(true),
                            events: (!events.is_empty()).then_some(events),
                            ..Response::default()
                        }
                    }
                    Err(error) => Response::error(error),
                }
            }
        }
    }

    fn kill_process(&self, process_id: &str) -> Response {
        let process = match self.processes.lock() {
            Ok(mut processes) => processes.remove(process_id),
            Err(_) => return Response::error("process map lock poisoned"),
        };
        let Some(process) = process else {
            return Response::error(format!("unknown process: {process_id}"));
        };
        match process.kill() {
            Ok(()) => Response::ok(),
            Err(error) => Response::error(error),
        }
    }
}

struct OutputCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

fn exec(
    argv: &[String],
    cwd: &str,
    env: &HashMap<String, String>,
    timeout_ms: Option<u64>,
) -> Response {
    if let Err(error) = validate_command(argv, cwd) {
        return Response::error(error);
    }
    let mut command = command(argv, cwd, env);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return command_error(cwd, error.to_string()),
    };
    let process_group = match i32::try_from(child.id()) {
        Ok(process_group) => process_group,
        Err(error) => return command_error(cwd, error.to_string()),
    };
    let _registration = DirectChildRegistration::new(process_group);
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return command_error(cwd, "missing child stdout"),
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => return command_error(cwd, "missing child stderr"),
    };
    let stdout_thread = capture_thread(stdout);
    let stderr_thread = capture_thread(stderr);
    let timeout = timeout_ms.map(Duration::from_millis);
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if timeout.is_some_and(|timeout| started.elapsed() >= timeout) => {
                timed_out = true;
                let result = unsafe { libc::kill(-process_group, libc::SIGKILL) };
                if result != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        break Err(error);
                    }
                }
                break child.wait();
            }
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(error) => break Err(error),
        }
    };
    let stdout = match stdout_thread.join() {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return command_error(cwd, error.to_string()),
        Err(_) => return command_error(cwd, "stdout reader panicked"),
    };
    let stderr = match stderr_thread.join() {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return command_error(cwd, error.to_string()),
        Err(_) => return command_error(cwd, "stderr reader panicked"),
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => return command_error(cwd, error.to_string()),
    };
    let output_truncated = stdout.truncated || stderr.truncated;
    let error = if timed_out {
        Some("Command timed out".to_string())
    } else if output_truncated {
        Some("Command output exceeded the guest capture limit".to_string())
    } else {
        None
    };
    Response {
        ok: status.success() && error.is_none(),
        exit_code: (!timed_out).then(|| exit_code(status)),
        stdout: Some(String::from_utf8_lossy(&stdout.bytes).into_owned()),
        stderr: Some(String::from_utf8_lossy(&stderr.bytes).into_owned()),
        cwd: Some(cwd.to_string()),
        error,
        ..Response::default()
    }
}

fn command(argv: &[String], cwd: &str, env: &HashMap<String, String>) -> Command {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .env("HOME", "/home/exo")
        .env(
            "PATH",
            "/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .envs(env)
        .process_group(0);
    unsafe {
        command.pre_exec(|| {
            if libc::setgroups(0, ptr::null()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setresgid(GUEST_GID, GUEST_GID, GUEST_GID) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setresuid(GUEST_UID, GUEST_UID, GUEST_UID) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

fn validate_command(argv: &[String], cwd: &str) -> Result<(), String> {
    if argv.is_empty() || argv.iter().any(|argument| argument.as_bytes().contains(&0)) {
        return Err("command requires non-empty NUL-free argv".to_string());
    }
    if !Path::new(cwd).is_absolute() {
        return Err("command cwd must be an absolute path".to_string());
    }
    Ok(())
}

fn command_error(cwd: &str, error: impl Into<String>) -> Response {
    Response {
        cwd: Some(cwd.to_string()),
        error: Some(error.into()),
        ..Response::default()
    }
}

fn capture_thread<R>(mut stream: R) -> thread::JoinHandle<std::io::Result<OutputCapture>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut truncated = false;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            let available = MAX_CAPTURE_BYTES.saturating_sub(bytes.len());
            let retained = available.min(count);
            bytes.extend_from_slice(&buffer[..retained]);
            truncated |= retained < count;
        }
        Ok(OutputCapture { bytes, truncated })
    })
}

fn stream_thread<R>(mut stream: R, events: Arc<EventQueue>, stdout: bool) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = match stream.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => count,
                Err(error) => {
                    events.push(ProcessEvent::Error {
                        message: format!("reading process output failed: {error}"),
                    });
                    return;
                }
            };
            let data = STANDARD.encode(&buffer[..count]);
            if stdout {
                events.push(ProcessEvent::Stdout { data });
            } else {
                events.push(ProcessEvent::Stderr { data });
            }
        }
    })
}

fn recv_timeout(seconds: Option<f64>) -> Result<Duration, String> {
    let seconds = seconds.unwrap_or(30.0);
    if !seconds.is_finite() || seconds < 0.0 {
        return Err("recv timeout must be a finite non-negative number".to_string());
    }
    Ok(Duration::from_secs_f64(seconds.min(30.0)))
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
        .unwrap_or(-1)
}

fn sync_filesystem(path: &str) -> Response {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return Response::error(error.to_string()),
    };
    let result = unsafe { libc::syncfs(file.as_raw_fd()) };
    if result == 0 {
        Response::ok()
    } else {
        Response::error(std::io::Error::last_os_error().to_string())
    }
}

fn configure_restored_network(address: Ipv4Addr, gateway: Ipv4Addr, prefix: u8) -> Response {
    if prefix > 32 {
        return Response::error("guest prefix must be at most 32");
    }
    let result = set_interface_address("eth0", address, prefix)
        .and_then(|()| set_interface_up("eth0"))
        .and_then(|()| replace_default_route("eth0", gateway));
    match result {
        Ok(()) => Response::ok(),
        Err(error) => Response::error(error),
    }
}

pub fn run() -> Result<(), String> {
    initialize_guest()?;
    thread::spawn(reap_orphans);
    let listener = vsock_listener()?;
    signal_ready_to_host()?;
    let state = Arc::new(AgentState::default());
    let active_connections = Arc::new(AtomicUsize::new(0));
    loop {
        let mut peer = unsafe { std::mem::zeroed::<libc::sockaddr_vm>() };
        let mut peer_length = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        let descriptor = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                (&raw mut peer).cast::<libc::sockaddr>(),
                &mut peer_length,
                libc::SOCK_CLOEXEC,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("accepting vsock connection: {error}"));
        }
        let connection = unsafe { OwnedFd::from_raw_fd(descriptor) };
        // Only Firecracker's host CID may drive the privileged supervisor.
        // Workloads inside the guest cannot use a loopback vsock connection to
        // ask PID 1 to execute a command before it drops the child privileges.
        // https://github.com/torvalds/linux/blob/master/include/uapi/linux/vm_sockets.h
        if peer.svm_family != libc::AF_VSOCK as libc::sa_family_t
            || peer.svm_cid != libc::VMADDR_CID_HOST
        {
            drop(connection);
            continue;
        }
        if active_connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            active_connections.fetch_sub(1, Ordering::AcqRel);
            drop(connection);
            continue;
        }
        // Release the slot from a drop guard so a panic inside the handler
        // cannot leak it: leaked slots accumulate until the agent refuses all
        // host connections, permanently wedging the sandbox's control channel.
        let slot = ConnectionSlot(Arc::clone(&active_connections));
        let state = Arc::clone(&state);
        thread::spawn(move || {
            let _slot = slot;
            if let Err(error) = serve_connection(connection, &state) {
                eprintln!("exo-firecracker-guest connection failed: {error}");
            }
        });
    }
}

struct ConnectionSlot(Arc<AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn signal_ready_to_host() -> Result<(), String> {
    // Firecracker forwards a guest connection to the host listener at
    // `<uds_path>_<port>`, giving the host an event-driven readiness edge.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md#guest-initiated-connections
    let descriptor =
        unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let address = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: READY_HOST_PORT,
        svm_cid: libc::VMADDR_CID_HOST,
        svm_zero: [0; 4],
    };
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(format!(
            "connecting guest-ready vsock: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut socket = File::from(socket);
    socket
        .write_all(&[1])
        .and_then(|()| socket.flush())
        .map_err(|error| error.to_string())
}

fn initialize_guest() -> Result<(), String> {
    mount_pseudo_filesystems()?;
    let command_line = fs::read_to_string("/proc/cmdline").map_err(|error| error.to_string())?;
    setup_root_overlay()?;
    configure_network(&command_line)?;
    let workspace = command_line_value(&command_line, "exo_workspace")
        .unwrap_or_else(|| "/home/exo/workspace".to_string());
    if let Some(path) = command_line_value(&command_line, "exo_workspace")
        && Path::new("/dev/vdc").exists()
    {
        fs::create_dir_all(&path).map_err(|error| error.to_string())?;
        mount_filesystem(Some("/dev/vdc"), &path, Some("ext4"), 0, None)?;
        chown(&path, Some(GUEST_UID), Some(GUEST_GID)).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(&workspace).map_err(|error| error.to_string())?;
    chown(&workspace, Some(GUEST_UID), Some(GUEST_GID)).map_err(|error| error.to_string())?;
    std::env::set_current_dir(&workspace).map_err(|error| error.to_string())?;
    Ok(())
}

fn setup_root_overlay() -> Result<(), String> {
    // The immutable OCI filesystem is shared by every VM. A separate sparse
    // ext4 disk holds only this VM's changes, matching the lower/upper layout
    // used by Hypeman instead of copying the full base image per launch.
    // https://github.com/kernel/hypeman/blob/main/lib/system/init/mount.go
    wait_for_device("/dev/vda")?;
    wait_for_device("/dev/vdb")?;
    for path in ["/mnt/lower", "/mnt/upper", "/mnt/newroot"] {
        fs::create_dir_all(path).map_err(|error| error.to_string())?;
    }
    mount_filesystem(
        Some("/dev/vda"),
        "/mnt/lower",
        Some("ext4"),
        libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
        None,
    )?;
    mount_filesystem(
        Some("/dev/vdb"),
        "/mnt/upper",
        Some("ext4"),
        libc::MS_NOSUID | libc::MS_NODEV,
        None,
    )?;
    for path in ["/mnt/upper/upper", "/mnt/upper/work"] {
        fs::create_dir_all(path).map_err(|error| error.to_string())?;
    }
    mount_filesystem(
        Some("overlay"),
        "/mnt/newroot",
        Some("overlay"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("lowerdir=/mnt/lower,upperdir=/mnt/upper/upper,workdir=/mnt/upper/work"),
    )?;
    for path in ["proc", "sys", "dev"] {
        let target = format!("/mnt/newroot/{path}");
        fs::create_dir_all(&target).map_err(|error| error.to_string())?;
        mount_filesystem(
            Some(&format!("/{path}")),
            &target,
            None,
            libc::MS_BIND | libc::MS_REC,
            None,
        )?;
    }
    let newroot = c_string("/mnt/newroot")?;
    if unsafe { libc::chroot(newroot.as_ptr()) } != 0 {
        return Err(format!(
            "switching to merged root filesystem: {}",
            std::io::Error::last_os_error()
        ));
    }
    std::env::set_current_dir("/").map_err(|error| error.to_string())
}

fn wait_for_device(path: &str) -> Result<(), String> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if Path::new(path).exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(1));
    }
    Err(format!("timed out waiting for block device {path}"))
}

fn mount_pseudo_filesystems() -> Result<(), String> {
    for path in ["/proc", "/sys", "/dev"] {
        fs::create_dir_all(path).map_err(|error| error.to_string())?;
    }
    mount_filesystem(
        Some("proc"),
        "/proc",
        Some("proc"),
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        None,
    )?;
    mount_filesystem(
        Some("sysfs"),
        "/sys",
        Some("sysfs"),
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        None,
    )?;
    mount_filesystem(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        libc::MS_NOSUID | libc::MS_NOEXEC,
        Some("mode=0755"),
    )?;
    Ok(())
}

fn mount_filesystem(
    source: Option<&str>,
    target: &str,
    filesystem_type: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<(), String> {
    let source = source.map(c_string).transpose()?;
    let target = c_string(target)?;
    let filesystem_type = filesystem_type.map(c_string).transpose()?;
    let data = data.map(c_string).transpose()?;
    let result = unsafe {
        libc::mount(
            source.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
            target.as_ptr(),
            filesystem_type
                .as_ref()
                .map_or(ptr::null(), |value| value.as_ptr()),
            flags,
            data.as_ref()
                .map_or(ptr::null(), |value| value.as_ptr().cast()),
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EBUSY) {
        return Ok(());
    }
    Err(format!("mounting {}: {error}", target.to_string_lossy()))
}

fn configure_network(command_line: &str) -> Result<(), String> {
    set_interface_up("lo")?;
    let Some(guest_ip) = command_line_value(command_line, "exo_guest_ip") else {
        return Ok(());
    };
    let gateway = required_command_line_value(command_line, "exo_gateway")?;
    let prefix = required_command_line_value(command_line, "exo_prefix")?;
    let dns = required_command_line_value(command_line, "exo_dns")?;
    let guest_ip = guest_ip
        .parse::<Ipv4Addr>()
        .map_err(|error| format!("invalid guest IP: {error}"))?;
    let gateway = gateway
        .parse::<Ipv4Addr>()
        .map_err(|error| format!("invalid guest gateway: {error}"))?;
    let prefix = prefix
        .parse::<u8>()
        .map_err(|error| format!("invalid guest prefix: {error}"))?;
    if prefix > 32 {
        return Err("guest prefix must be at most 32".to_string());
    }
    set_interface_address("eth0", guest_ip, prefix)?;
    set_interface_up("eth0")?;
    add_default_route("eth0", gateway)?;
    replace_resolver(&dns)
}

fn set_interface_up(name: &str) -> Result<(), String> {
    let socket = ipv4_control_socket()?;
    let mut request = interface_request(name)?;
    let get_result = unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS, &mut request) };
    if get_result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    unsafe {
        request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
    }
    let set_result = unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS, &request) };
    if set_result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn set_interface_address(name: &str, address: Ipv4Addr, prefix: u8) -> Result<(), String> {
    let socket = ipv4_control_socket()?;
    let mut address_request = interface_request(name)?;
    address_request.ifr_ifru.ifru_addr = ipv4_sockaddr(address);
    let address_result =
        unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFADDR, &address_request) };
    if address_result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let mask = if prefix == 0 {
        Ipv4Addr::UNSPECIFIED
    } else {
        Ipv4Addr::from(u32::MAX << (32 - prefix))
    };
    let mut mask_request = interface_request(name)?;
    mask_request.ifr_ifru.ifru_netmask = ipv4_sockaddr(mask);
    let mask_result =
        unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFNETMASK, &mask_request) };
    if mask_result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn add_default_route(name: &str, gateway: Ipv4Addr) -> Result<(), String> {
    let socket = ipv4_control_socket()?;
    let device = c_string(name)?;
    let mut route = unsafe { std::mem::zeroed::<libc::rtentry>() };
    route.rt_gateway = ipv4_sockaddr(gateway);
    route.rt_dst = ipv4_sockaddr(Ipv4Addr::UNSPECIFIED);
    route.rt_genmask = ipv4_sockaddr(Ipv4Addr::UNSPECIFIED);
    route.rt_flags = (libc::RTF_UP | libc::RTF_GATEWAY) as libc::c_ushort;
    route.rt_dev = device.as_ptr().cast_mut();
    let result = unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCADDRT, &route) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn replace_default_route(name: &str, gateway: Ipv4Addr) -> Result<(), String> {
    // A restored guest still has the source route in kernel memory. Delete it
    // before adding the clone gateway so the old default cannot win route lookup.
    // These ioctl values and rtentry fields are the Linux UAPI used at boot too.
    // https://github.com/torvalds/linux/blob/master/include/uapi/linux/sockios.h
    // https://github.com/torvalds/linux/blob/master/include/uapi/linux/route.h
    let socket = ipv4_control_socket()?;
    let device = c_string(name)?;
    let mut route = unsafe { std::mem::zeroed::<libc::rtentry>() };
    route.rt_dst = ipv4_sockaddr(Ipv4Addr::UNSPECIFIED);
    route.rt_genmask = ipv4_sockaddr(Ipv4Addr::UNSPECIFIED);
    route.rt_flags = libc::RTF_UP as libc::c_ushort;
    route.rt_dev = device.as_ptr().cast_mut();
    let result = unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCDELRT, &route) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) && error.raw_os_error() != Some(libc::ENOENT) {
            return Err(error.to_string());
        }
    }
    add_default_route(name, gateway)
}

fn ipv4_control_socket() -> Result<OwnedFd, String> {
    let descriptor =
        unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn interface_request(name: &str) -> Result<libc::ifreq, String> {
    let bytes = name.as_bytes();
    if bytes.len() >= libc::IFNAMSIZ {
        return Err(format!("interface name is too long: {name}"));
    }
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    for (destination, source) in request.ifr_name.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }
    Ok(request)
}

fn ipv4_sockaddr(address: Ipv4Addr) -> libc::sockaddr {
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from(address).to_be(),
        },
        sin_zero: [0; 8],
    };
    unsafe { ptr::read((&raw const address).cast::<libc::sockaddr>()) }
}

fn replace_resolver(dns: &str) -> Result<(), String> {
    fs::create_dir_all("/etc").map_err(|error| error.to_string())?;
    let resolver = Path::new("/etc/resolv.conf");
    match fs::symlink_metadata(resolver) {
        Ok(metadata) if metadata.is_dir() => {
            return Err("/etc/resolv.conf is a directory".to_string());
        }
        Ok(_) => fs::remove_file(resolver).map_err(|error| error.to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    fs::write(
        resolver,
        format!("nameserver {dns}\noptions single-request-reopen\n"),
    )
    .map_err(|error| error.to_string())
}

fn vsock_listener() -> Result<OwnedFd, String> {
    // Firecracker's host-initiated vsock transport forwards a jailed Unix
    // socket to this guest listener without exposing a TCP control port.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md#host-initiated-connections
    let descriptor =
        unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let listener = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let address = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: AGENT_PORT,
        svm_cid: libc::VMADDR_CID_ANY,
        svm_zero: [0; 4],
    };
    let result = unsafe {
        libc::bind(
            listener.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(format!(
            "binding guest vsock: {}",
            std::io::Error::last_os_error()
        ));
    }
    let result = unsafe { libc::listen(listener.as_raw_fd(), MAX_CONNECTIONS as libc::c_int) };
    if result != 0 {
        return Err(format!(
            "listening on guest vsock: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(listener)
}

fn serve_connection(connection: OwnedFd, state: &AgentState) -> Result<(), String> {
    set_socket_timeout(
        connection.as_raw_fd(),
        libc::SO_RCVTIMEO,
        CONNECTION_TIMEOUT,
    )?;
    set_socket_timeout(
        connection.as_raw_fd(),
        libc::SO_SNDTIMEO,
        CONNECTION_TIMEOUT,
    )?;
    let mut connection = File::from(connection);
    let mut length = [0_u8; 4];
    connection
        .read_exact(&mut length)
        .map_err(|error| error.to_string())?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_REQUEST_BYTES {
        return write_response(
            &mut connection,
            &Response::error("request exceeds size limit"),
        );
    }
    let mut payload = vec![0_u8; length];
    connection
        .read_exact(&mut payload)
        .map_err(|error| error.to_string())?;
    let response = match serde_json::from_slice::<Request>(&payload) {
        Ok(request) => state.handle(request),
        Err(error) => Response::error(format!("invalid request: {error}")),
    };
    write_response(&mut connection, &response)
}

fn write_response(connection: &mut File, response: &Response) -> Result<(), String> {
    let mut payload = serde_json::to_vec(response).map_err(|error| error.to_string())?;
    if payload.len() > MAX_RESPONSE_BYTES {
        payload = serde_json::to_vec(&Response::error("response exceeds size limit"))
            .map_err(|error| error.to_string())?;
    }
    let length = u32::try_from(payload.len()).map_err(|error| error.to_string())?;
    connection
        .write_all(&length.to_be_bytes())
        .and_then(|()| connection.write_all(&payload))
        .and_then(|()| connection.flush())
        .map_err(|error| error.to_string())
}

fn set_socket_timeout(
    descriptor: libc::c_int,
    option: libc::c_int,
    timeout: Duration,
) -> Result<(), String> {
    let timeout = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: libc::suseconds_t::from(timeout.subsec_micros()),
    };
    let result = unsafe {
        libc::setsockopt(
            descriptor,
            libc::SOL_SOCKET,
            option,
            (&raw const timeout).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn command_line_value(command_line: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    command_line
        .split_ascii_whitespace()
        .find_map(|argument| argument.strip_prefix(&prefix).map(str::to_string))
}

fn required_command_line_value(command_line: &str, key: &str) -> Result<String, String> {
    command_line_value(command_line, key).ok_or_else(|| format!("missing kernel argument {key}"))
}

fn c_string(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| format!("value contains NUL: {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typed_requests() {
        let request = serde_json::from_str::<Request>(
            r#"{"type":"exec","argv":["/bin/echo","hi"],"env":{},"cwd":"/","timeout_ms":1000}"#,
        )
        .unwrap();
        assert!(matches!(request, Request::Exec { .. }));
    }

    #[test]
    fn command_line_values_are_exact() {
        let command_line = "root=/dev/vda exo_guest_ip=10.0.0.2 unrelated_exo_guest_ip=bad";
        assert_eq!(
            command_line_value(command_line, "exo_guest_ip").as_deref(),
            Some("10.0.0.2")
        );
    }

    #[test]
    fn event_queue_bounds_buffered_output() {
        let queue = EventQueue::default();
        queue.push(ProcessEvent::Stdout {
            data: "x".repeat(MAX_QUEUED_BYTES + 1),
        });
        assert_eq!(
            queue.recv(Duration::ZERO).unwrap_err(),
            "process output exceeded the guest queue limit"
        );
    }
}
