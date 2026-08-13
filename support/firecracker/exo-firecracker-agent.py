#!/usr/bin/env python3

# Firecracker maps guest AF_VSOCK connections onto a host Unix socket; keeping
# process control on this transport avoids opening a TCP service in the guest.
# https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md

import base64
import concurrent.futures
import json
import os
import queue
import signal
import socket
import struct
import subprocess
import threading

AGENT_PORT = 10052
MAX_REQUEST_BYTES = 1024 * 1024
MAX_RESPONSE_BYTES = 16 * 1024 * 1024
MAX_RECV_EVENTS = 64
MAX_RECV_BYTES = 1024 * 1024
MAX_PROCESSES = 128
MAX_CONNECTIONS = 32

PROCESSES = {}
PROCESS_LOCK = threading.Lock()
NEXT_PROCESS_ID = 1


class ManagedProcess:
    def __init__(self, argv, cwd, env):
        process_env = os.environ.copy()
        process_env.update(env)
        self.process = subprocess.Popen(
            argv,
            cwd=cwd,
            env=process_env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
            start_new_session=True,
        )
        self.events = queue.Queue(maxsize=1024)
        self.write_lock = threading.Lock()
        self.stdout_thread = threading.Thread(
            target=self._read_stream,
            args=("stdout", self.process.stdout),
            daemon=True,
        )
        self.stderr_thread = threading.Thread(
            target=self._read_stream,
            args=("stderr", self.process.stderr),
            daemon=True,
        )
        self.stdout_thread.start()
        self.stderr_thread.start()
        threading.Thread(target=self._wait, daemon=True).start()

    def _read_stream(self, stream, handle):
        try:
            while True:
                data = handle.read(65536)
                if not data:
                    break
                self.events.put(
                    {
                        "type": stream,
                        "data": base64.b64encode(data).decode("ascii"),
                    }
                )
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
            raise RuntimeError(f"process is not running: {self.process.returncode}")
        payload = base64.b64decode(data.encode("ascii"), validate=True)
        with self.write_lock:
            if self.process.stdin is None:
                raise RuntimeError("process stdin is closed")
            self.process.stdin.write(payload)
            self.process.stdin.flush()

    def close_stdin(self):
        with self.write_lock:
            if self.process.stdin is not None:
                self.process.stdin.close()

    def kill(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)

    def recv(self, timeout_seconds):
        try:
            event = self.events.get(timeout=timeout_seconds)
        except queue.Empty:
            return {"ok": True, "timeout": True}, False

        events = []
        total_bytes = 0
        exited = False
        while True:
            if event.get("type") == "error":
                raise RuntimeError(event.get("message") or "process failed")
            events.append(event)
            total_bytes += len(event.get("data", ""))
            if event.get("type") == "exit":
                exited = True
                break
            if len(events) >= MAX_RECV_EVENTS or total_bytes >= MAX_RECV_BYTES:
                break
            try:
                event = self.events.get_nowait()
            except queue.Empty:
                break
        return {"ok": True, "events": events}, exited


def get_process(process_id):
    with PROCESS_LOCK:
        return PROCESSES.get(process_id)


def remove_process(process_id):
    with PROCESS_LOCK:
        return PROCESSES.pop(process_id, None)


def valid_argv(argv):
    return isinstance(argv, list) and argv and all(isinstance(arg, str) for arg in argv)


def valid_cwd(cwd):
    return isinstance(cwd, str) and cwd.startswith("/")


def valid_env(env):
    return isinstance(env, dict) and all(
        isinstance(key, str) and isinstance(value, str) for key, value in env.items()
    )


def output_text(value):
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return str(value)


def process_error(cwd, message):
    return {
        "ok": False,
        "exit_code": None,
        "stdout": "",
        "stderr": "",
        "cwd": cwd,
        "error": message,
    }


def handle_exec(payload):
    argv = payload.get("argv")
    if not valid_argv(argv):
        return {"ok": False, "error": "exec requires non-empty string argv"}
    cwd = payload.get("cwd", os.getcwd())
    if not valid_cwd(cwd):
        return {"ok": False, "error": "exec cwd must be an absolute path"}
    env = payload.get("env", {})
    if not valid_env(env):
        return {"ok": False, "error": "exec env must be an object of strings"}
    timeout_ms = payload.get("timeout_ms")
    if timeout_ms is not None and (not isinstance(timeout_ms, int) or timeout_ms < 0):
        return {"ok": False, "error": "exec timeout_ms must be a positive integer"}

    process_env = os.environ.copy()
    process_env.update(env)
    try:
        process = subprocess.Popen(
            argv,
            cwd=cwd,
            env=process_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(
                timeout=(timeout_ms / 1000) if timeout_ms is not None else None
            )
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate()
            return {
                "ok": False,
                "exit_code": None,
                "stdout": output_text(stdout),
                "stderr": output_text(stderr),
                "cwd": cwd,
                "error": "Command timed out",
            }
    except OSError as exc:
        return process_error(cwd, str(exc))
    return {
        "ok": process.returncode == 0,
        "exit_code": process.returncode,
        "stdout": output_text(stdout),
        "stderr": output_text(stderr),
        "cwd": cwd,
    }


def handle_start_process(payload):
    argv = payload.get("argv")
    if not valid_argv(argv):
        return {"ok": False, "error": "start_process requires non-empty string argv"}
    cwd = payload.get("cwd", os.getcwd())
    if not valid_cwd(cwd):
        return {"ok": False, "error": "start_process cwd must be an absolute path"}
    env = payload.get("env", {})
    if not valid_env(env):
        return {"ok": False, "error": "start_process env must be an object of strings"}
    with PROCESS_LOCK:
        if len(PROCESSES) >= MAX_PROCESSES:
            return {"ok": False, "error": "too many managed processes"}
    try:
        process = ManagedProcess(argv, cwd, env)
    except OSError as exc:
        return process_error(cwd, str(exc))
    global NEXT_PROCESS_ID
    with PROCESS_LOCK:
        if len(PROCESSES) >= MAX_PROCESSES:
            process.kill()
            return {"ok": False, "error": "too many managed processes"}
        process_id = f"process-{NEXT_PROCESS_ID}"
        NEXT_PROCESS_ID += 1
        PROCESSES[process_id] = process
    return {"process_id": process_id}


def handle_process_bridge(payload):
    process_id = payload.get("process_id")
    if not isinstance(process_id, str):
        return {"ok": False, "error": "process_id is required"}
    request = payload.get("request")
    if not isinstance(request, dict):
        return {"ok": False, "error": "request object is required"}
    process = get_process(process_id)
    if process is None:
        return {"ok": False, "error": f"unknown process: {process_id}"}
    kind = request.get("type")
    if kind == "ping":
        return {"ok": True}
    if kind == "write":
        process.write(request["data"])
        return {"ok": True}
    if kind == "close_stdin":
        process.close_stdin()
        return {"ok": True}
    if kind == "recv":
        response, exited = process.recv(min(float(request.get("timeout_seconds", 30)), 30))
        if exited:
            remove_process(process_id)
        return response
    return {"ok": False, "error": f"unknown bridge request type: {kind}"}


def handle_kill_process(payload):
    process_id = payload.get("process_id")
    if not isinstance(process_id, str):
        return {"ok": False, "error": "process_id is required"}
    process = remove_process(process_id)
    if process is None:
        return {"ok": False, "error": f"unknown process: {process_id}"}
    process.kill()
    return {"ok": True}


def handle_request(payload):
    kind = payload.get("type")
    if kind == "ping":
        return {"ok": True}
    if kind == "exec":
        return handle_exec(payload)
    if kind == "start_process":
        return handle_start_process(payload)
    if kind == "process_bridge":
        return handle_process_bridge(payload)
    if kind == "kill_process":
        return handle_kill_process(payload)
    return {"ok": False, "error": "unsupported request type"}


def recv_exact(connection, length):
    chunks = []
    remaining = length
    while remaining:
        chunk = connection.recv(remaining)
        if not chunk:
            raise EOFError("connection closed")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def send_response(connection, payload):
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    if len(body) > MAX_RESPONSE_BYTES:
        body = b'{"ok":false,"error":"response exceeds size limit"}'
    connection.sendall(struct.pack(">I", len(body)) + body)


def serve_connection(connection):
    try:
        request_length = struct.unpack(">I", recv_exact(connection, 4))[0]
        if request_length > MAX_REQUEST_BYTES:
            raise ValueError("request exceeds size limit")
        payload = json.loads(recv_exact(connection, request_length).decode("utf-8"))
        if not isinstance(payload, dict):
            raise ValueError("request must be a JSON object")
        try:
            response = handle_request(payload)
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        send_response(connection, response)
    except Exception as exc:
        try:
            send_response(connection, {"ok": False, "error": str(exc)})
        except OSError:
            pass
    finally:
        connection.close()


def main():
    if not hasattr(socket, "AF_VSOCK"):
        raise RuntimeError("Python and the guest kernel must support AF_VSOCK")
    server = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind((socket.VMADDR_CID_ANY, AGENT_PORT))
    server.listen(MAX_CONNECTIONS)
    with concurrent.futures.ThreadPoolExecutor(max_workers=MAX_CONNECTIONS) as executor:
        while True:
            connection, _address = server.accept()
            executor.submit(serve_connection, connection)


if __name__ == "__main__":
    main()
