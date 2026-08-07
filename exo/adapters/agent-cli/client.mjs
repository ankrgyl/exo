#!/usr/bin/env node
// Thin client for the agent-cli adapter. Sends the prompt plus the current
// working directory to the adapter worker's unix socket and prints the
// agent's reply. Plain node with no dependencies so it runs from anywhere.
import net from "node:net";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import readline from "node:readline";

const DEFAULT_TIMEOUT_MS = 15 * 60 * 1000;

const prompt = process.argv.slice(2).join(" ").trim();
if (prompt.length === 0) {
  process.stderr.write(
    'Usage: exo-cli "<prompt>"\n\nSends the prompt to the Exo agent-cli adapter with access to the current directory.\nEnvironment: EXO_AGENT_CLI_SOCKET (socket path), EXO_AGENT_CLI_TIMEOUT_MS (reply timeout).\n',
  );
  process.exit(2);
}

const socketPath =
  process.env.EXO_AGENT_CLI_SOCKET ??
  path.join(os.homedir(), ".exo", "agent-cli.sock");
const timeoutMs = Number(
  process.env.EXO_AGENT_CLI_TIMEOUT_MS ?? DEFAULT_TIMEOUT_MS,
);

const socket = net.createConnection(socketPath);
socket.setEncoding("utf8");

const timeout = setTimeout(() => {
  process.stderr.write(
    `exo-cli: timed out after ${Math.round(timeoutMs / 1000)}s waiting for a reply\n`,
  );
  process.exit(1);
}, timeoutMs);
timeout.unref();
socket.on("connect", () => {
  timeout.ref();
  process.stderr.write("exo-cli: waiting for the agent...\n");
});

socket.on("error", (error) => {
  if (error.code === "ENOENT" || error.code === "ECONNREFUSED") {
    process.stderr.write(
      `exo-cli: agent-cli adapter is not listening on ${socketPath}\n`,
    );
    // Exit code 3 tells the exo-cli wrapper to attempt a bootstrap.
    process.exit(3);
  }
  process.stderr.write(`exo-cli: socket error: ${error.message}\n`);
  process.exit(1);
});

socket.write(`${JSON.stringify({ cwd: process.cwd(), prompt })}\n`);

const lines = readline.createInterface({
  input: socket,
  crlfDelay: Number.POSITIVE_INFINITY,
});

lines.on("line", (line) => {
  if (line.trim().length === 0) {
    return;
  }
  let reply;
  try {
    reply = JSON.parse(line);
  } catch {
    process.stderr.write(`exo-cli: unexpected response: ${line}\n`);
    process.exit(1);
  }
  if (reply.type === "reply" && typeof reply.text === "string") {
    process.stdout.write(`${reply.text}\n`);
    process.exit(0);
  }
  if (reply.type === "error" && typeof reply.message === "string") {
    process.stderr.write(`exo-cli: ${reply.message}\n`);
    process.exit(1);
  }
  process.stderr.write(`exo-cli: unexpected response: ${line}\n`);
  process.exit(1);
});

socket.on("close", () => {
  process.stderr.write("exo-cli: connection closed before a reply arrived\n");
  process.exit(1);
});
