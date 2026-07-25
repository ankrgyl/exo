import crypto from "node:crypto";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import process from "node:process";
import readline from "node:readline";
import inputReadline from "node:readline/promises";

import {
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";
import {
  HarborClientResponse,
  HarborRequest,
  HarborResponse,
  composeHarborPrompt,
  defaultSocketPath,
  parseHarborRequest,
  parseHarborResponse,
  requestTarget,
} from "./harbor";

type PendingRequest = {
  request: HarborRequest;
  sockets: Set<net.Socket>;
};

const config = adapterConfig();
const socketPath =
  optionalStringField(config, "socketPath") ?? defaultSocketPath();
const adapterId = requiredEnvironment("EXO_ADAPTER_ID");
const stateDir = requiredEnvironment("EXO_ADAPTER_STATE_DIR");

const responseDir = path.join(stateDir, "responses");
const pending = new Map<string, PendingRequest>();
fs.mkdirSync(path.dirname(socketPath), { recursive: true });
fs.mkdirSync(responseDir, { recursive: true });
fs.rmSync(socketPath, { force: true });

const server = net.createServer((socket) => {
  socket.setEncoding("utf8");
  socket.on("error", (error) => {
    writeWorkerEvent({
      type: "error",
      message: `Harbor client socket error: ${error.message}`,
    });
  });
  socket.on("close", () => {
    for (const entry of pending.values()) {
      entry.sockets.delete(socket);
    }
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
      handleRequest(socket, parseHarborRequest(JSON.parse(line)));
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      sendToClient(socket, { type: "error", message });
    }
  });
});

server.on("error", (error) => {
  writeWorkerEvent({
    type: "error",
    message: `Harbor adapter listener error: ${error.message}`,
  });
  process.exit(1);
});

server.listen(socketPath, () => {
  fs.chmodSync(socketPath, 0o600);
  process.stderr.write(`[harbor-adapter] listening on ${socketPath}\n`);
  writeWorkerEvent({
    type: "connected",
    subject: socketPath,
    metadata: { socketPath },
  });
});

process.on("exit", () => {
  fs.rmSync(socketPath, { force: true });
});

function handleRequest(socket: net.Socket, request: HarborRequest): void {
  const target = requestTarget(request);
  const completed = readCompletedResponse(target);
  if (completed) {
    sendToClient(socket, { type: "response", event: completed });
    return;
  }

  const existing = pending.get(target);
  if (existing) {
    if (existing.request.message_id !== request.message_id) {
      throw new Error(
        `target ${target} is already active with message_id ${existing.request.message_id}`,
      );
    }
    existing.sockets.add(socket);
    return;
  }

  pending.set(target, { request, sockets: new Set([socket]) });
  writeWorkerEvent({
    type: "message",
    target,
    sender: "harbor",
    text: composeHarborPrompt(request, adapterId, target),
    message_id: request.message_id,
    metadata: request,
  });
}

const input = inputReadline.createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

input.on("error", (error) => {
  writeWorkerEvent({
    type: "error",
    message: `Harbor adapter command stream error: ${error.message}`,
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
      throw new Error("Harbor adapter does not support attachments");
    }
    if (!command.target) {
      throw new Error(
        "Harbor send_message requires the target from the inbound message",
      );
    }
    const entry = pending.get(command.target);
    if (!entry) {
      const completed = readCompletedResponse(command.target);
      if (completed) {
        writeWorkerEvent({
          type: "command_ack",
          command_id: command.id,
        });
        continue;
      }
      throw new Error(
        `Harbor target ${command.target} is not awaiting a response`,
      );
    }
    const response = parseHarborResponse(command.text, entry.request);
    writeCompletedResponse(command.target, response);
    pending.delete(command.target);
    for (const socket of entry.sockets) {
      sendToClient(socket, { type: "response", event: response });
    }
    writeWorkerEvent({ type: "command_ack", command_id: command.id });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    writeWorkerEvent({ type: "error", message });
    if (commandId) {
      writeWorkerEvent({
        type: "command_nack",
        command_id: commandId,
        message,
      });
    }
  }
}

function responsePath(target: string): string {
  const digest = crypto.createHash("sha256").update(target).digest("hex");
  return path.join(responseDir, `${digest}.json`);
}

function readCompletedResponse(target: string): HarborResponse | null {
  try {
    return JSON.parse(
      fs.readFileSync(responsePath(target), "utf8"),
    ) as HarborResponse;
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

function writeCompletedResponse(
  target: string,
  response: HarborResponse,
): void {
  const destination = responsePath(target);
  const temporary = `${destination}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, JSON.stringify(response), { mode: 0o600 });
  fs.renameSync(temporary, destination);
}

function sendToClient(socket: net.Socket, payload: HarborClientResponse): void {
  socket.write(`${JSON.stringify(payload)}\n`);
}

function requiredEnvironment(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} must be set`);
  }
  return value;
}
