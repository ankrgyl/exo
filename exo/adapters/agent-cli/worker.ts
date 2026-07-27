import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import readline from "node:readline";
import inputReadline from "node:readline/promises";

import {
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  stringField,
  writeWorkerEvent,
} from "../protocol";
import {
  composeMessageText,
  defaultSocketPath,
  parseAgentCliRequest,
} from "./agent-cli";

const config = adapterConfig();
const socketPath =
  optionalStringField(config, "socketPath") ?? defaultSocketPath();
const mountRoot = stringField(config, "mountRoot");
const mountPath = stringField(config, "mountPath");
const sender = os.userInfo().username;

if (!mountRoot.startsWith("/")) {
  throw new Error("agent-cli mountRoot must be an absolute host path");
}
if (!mountPath.startsWith("/")) {
  throw new Error("agent-cli mountPath must be an absolute sandbox path");
}

// One live client connection per target id; replies route back through it.
const connections = new Map<string, net.Socket>();
let connectionCounter = 0;

fs.mkdirSync(path.dirname(socketPath), { recursive: true });
// The runner guarantees a single worker per adapter, so a leftover socket
// file is always stale (e.g. after a SIGKILL) and safe to remove.
fs.rmSync(socketPath, { force: true });

const server = net.createServer((socket) => {
  connectionCounter += 1;
  const target = `cli-${process.pid}-${connectionCounter}`;
  let messageCounter = 0;
  connections.set(target, socket);
  socket.setEncoding("utf8");
  socket.on("error", (error) => {
    writeWorkerEvent({
      type: "error",
      message: `agent-cli client ${target} socket error: ${error.message}`,
    });
  });
  socket.on("close", () => {
    connections.delete(target);
  });

  const lines = readline.createInterface({
    input: socket,
    crlfDelay: Number.POSITIVE_INFINITY,
  });
  lines.on("line", (line) => {
    if (line.trim().length === 0) {
      return;
    }
    try {
      const request = parseAgentCliRequest(JSON.parse(line));
      messageCounter += 1;
      process.stderr.write(
        `[agent-cli-adapter] request from ${sender} (cwd ${request.cwd})\n`,
      );
      writeWorkerEvent({
        type: "message",
        target,
        sender,
        text: composeMessageText(request, mountRoot, mountPath),
        message_id: `${target}-${messageCounter}`,
        metadata: { cwd: request.cwd, socketPath },
      });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      sendToClient(socket, { type: "error", message });
    }
  });
});

server.on("error", (error) => {
  writeWorkerEvent({
    type: "error",
    message: `agent-cli listener error: ${error.message}`,
  });
  process.exit(1);
});

server.listen(socketPath, () => {
  fs.chmodSync(socketPath, 0o600);
  process.stderr.write(`[agent-cli-adapter] listening on ${socketPath}\n`);
  writeWorkerEvent({
    type: "connected",
    subject: socketPath,
    metadata: { socketPath, mountRoot, mountPath },
  });
});

process.on("exit", () => {
  fs.rmSync(socketPath, { force: true });
});

const input = inputReadline.createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

input.on("error", (error) => {
  writeWorkerEvent({
    type: "error",
    message: `agent-cli command stream error: ${error.message}`,
  });
});

for await (const line of input) {
  if (line.trim().length === 0) {
    continue;
  }
  let commandId: string | null = null;
  try {
    const command = parseWorkerCommand(JSON.parse(line));
    commandId = command.id;
    if (command.attachments.length > 0) {
      throw new Error("agent-cli does not support attachments");
    }
    const target = command.target;
    if (target === null || target === undefined) {
      throw new Error(
        "agent-cli send_message requires the target from the inbound message",
      );
    }
    const socket = connections.get(target);
    if (!socket) {
      throw new Error(
        `agent-cli client ${target} is no longer connected; the reply cannot be delivered`,
      );
    }
    sendToClient(socket, { type: "reply", text: command.text });
    writeWorkerEvent({ type: "command_ack", command_id: command.id });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    writeWorkerEvent({ type: "error", message });
    if (commandId !== null) {
      writeWorkerEvent({
        type: "command_nack",
        command_id: commandId,
        message,
      });
    }
  }
}

function sendToClient(socket: net.Socket, payload: object): void {
  socket.write(`${JSON.stringify(payload)}\n`, (error) => {
    if (error) {
      writeWorkerEvent({
        type: "error",
        message: `agent-cli client write error: ${error.message}`,
      });
    }
  });
}
