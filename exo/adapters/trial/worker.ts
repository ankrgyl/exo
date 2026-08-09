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
  composeTrialPrompt,
  defaultSocketPath,
  parseTrialComplete,
  parseTrialRun,
  type TrialComplete,
  type TrialRun,
} from "./trial";

type PendingTrial = {
  request: TrialRun;
  sockets: Set<net.Socket>;
};

type CompletedTrial = {
  commandId: string;
  response: TrialComplete;
};

const config = adapterConfig();
const socketPath =
  optionalStringField(config, "socketPath") ?? defaultSocketPath();
const stateDir = process.env.EXO_ADAPTER_STATE_DIR ?? ".";
const adapterId = process.env.EXO_ADAPTER_ID ?? "";
const startupId = crypto.randomUUID();
const pendingByTarget = new Map<string, PendingTrial>();

fs.mkdirSync(path.dirname(socketPath), { recursive: true });
fs.mkdirSync(stateDir, { recursive: true });
fs.rmSync(socketPath, { force: true });
loadPendingTrials();

const server = net.createServer((socket) => {
  socket.setEncoding("utf8");
  const lines = readline.createInterface({
    input: socket,
    crlfDelay: Number.POSITIVE_INFINITY,
  });

  lines.on("line", (line) => {
    if (line.trim().length === 0) {
      return;
    }
    let request: TrialRun;
    try {
      request = parseTrialRun(JSON.parse(line));
    } catch (error) {
      sendToClient(socket, { type: "error", message: errorText(error) });
      return;
    }

    const completed = readCompletedTrial(request.request_id);
    if (completed) {
      sendToClient(socket, { type: "response", event: completed.response });
      return;
    }

    const pending = pendingByTarget.get(request.target);
    if (pending) {
      if (pending.request.request_id !== request.request_id) {
        sendToClient(socket, {
          type: "error",
          message: `trial target ${request.target} already has an active request`,
        });
        return;
      }
      pending.sockets.add(socket);
      return;
    }
    if (findPendingRequest(request.request_id)) {
      sendToClient(socket, {
        type: "error",
        message: `request_id ${request.request_id} is already active for another target`,
      });
      return;
    }

    writePendingTrial(request);
    pendingByTarget.set(request.target, {
      request,
      sockets: new Set([socket]),
    });
    emitTrialRun(request, false);
  });

  socket.on("error", () => {
    // A waiting evaluator may disconnect during an Exo restart or timeout.
  });
  socket.on("close", () => {
    for (const pending of pendingByTarget.values()) {
      pending.sockets.delete(socket);
    }
  });
});

server.on("error", (error) => {
  writeWorkerEvent({
    type: "error",
    message: `trial listener error: ${error.message}`,
  });
  process.exit(1);
});

server.listen(socketPath, () => {
  fs.chmodSync(socketPath, 0o600);
  process.stderr.write(`[trial-adapter] listening on ${socketPath}\n`);
  writeWorkerEvent({
    type: "connected",
    subject: socketPath,
    metadata: { socketPath },
  });
  for (const pending of pendingByTarget.values()) {
    emitTrialRun(pending.request, true);
  }
});

process.on("exit", () => {
  fs.rmSync(socketPath, { force: true });
});

const input = inputReadline.createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

for await (const line of input) {
  if (line.trim().length === 0) {
    continue;
  }
  let commandId: string | null = null;
  try {
    const command = parseWorkerCommand(JSON.parse(line));
    commandId = command.id;
    const target = command.target;
    if (!target) {
      throw new Error("trial completion requires its inbound target");
    }
    const pending = pendingByTarget.get(target);
    if (!pending) {
      if (hasCompletedCommand(command.id)) {
        writeWorkerEvent({ type: "command_ack", command_id: command.id });
        continue;
      }
      throw new Error(`trial target ${target} is not awaiting completion`);
    }

    const response = parseTrialComplete(command.text, pending.request);
    writeCompletedTrial(pending.request.request_id, command.id, response);
    removePendingTrial(pending.request.request_id);
    pendingByTarget.delete(target);
    for (const socket of pending.sockets) {
      sendToClient(socket, { type: "response", event: response });
    }
    writeWorkerEvent({ type: "command_ack", command_id: command.id });
  } catch (error) {
    const message = errorText(error);
    if (commandId === null) {
      writeWorkerEvent({ type: "error", message });
    } else {
      writeWorkerEvent({
        type: "command_nack",
        command_id: commandId,
        message,
      });
    }
  }
}

function emitTrialRun(request: TrialRun, resuming: boolean): void {
  writeWorkerEvent({
    type: "message",
    target: request.target,
    sender: "trial_runner",
    text: composeTrialPrompt(request, adapterId, resuming),
    message_id: resuming
      ? `${request.request_id}:resume:${startupId}`
      : request.request_id,
    metadata: {
      request_id: request.request_id,
      container_id: request.container_id,
    },
  });
}

function loadPendingTrials(): void {
  for (const name of fs.readdirSync(stateDir)) {
    if (!name.startsWith("pending-") || !name.endsWith(".json")) {
      continue;
    }
    const file = path.join(stateDir, name);
    const request = parseTrialRun(JSON.parse(fs.readFileSync(file, "utf8")));
    if (readCompletedTrial(request.request_id)) {
      fs.rmSync(file, { force: true });
      continue;
    }
    if (pendingByTarget.has(request.target)) {
      throw new Error(`multiple pending requests for target ${request.target}`);
    }
    pendingByTarget.set(request.target, { request, sockets: new Set() });
  }
}

function findPendingRequest(requestId: string): PendingTrial | null {
  for (const pending of pendingByTarget.values()) {
    if (pending.request.request_id === requestId) {
      return pending;
    }
  }
  return null;
}

function pendingPath(requestId: string): string {
  return statePath("pending", requestId);
}

function completedPath(requestId: string): string {
  return statePath("response", requestId);
}

function statePath(kind: string, requestId: string): string {
  const digest = crypto.createHash("sha256").update(requestId).digest("hex");
  return path.join(stateDir, `${kind}-${digest}.json`);
}

function writePendingTrial(request: TrialRun): void {
  writeJsonAtomically(pendingPath(request.request_id), request);
}

function removePendingTrial(requestId: string): void {
  fs.rmSync(pendingPath(requestId), { force: true });
}

function readCompletedTrial(requestId: string): CompletedTrial | null {
  try {
    return JSON.parse(
      fs.readFileSync(completedPath(requestId), "utf8"),
    ) as CompletedTrial;
  } catch {
    return null;
  }
}

function writeCompletedTrial(
  requestId: string,
  commandId: string,
  response: TrialComplete,
): void {
  writeJsonAtomically(completedPath(requestId), { commandId, response });
}

function writeJsonAtomically(destination: string, value: object): void {
  const temporary = `${destination}.${crypto.randomUUID()}.tmp`;
  fs.writeFileSync(temporary, JSON.stringify(value));
  fs.renameSync(temporary, destination);
}

function hasCompletedCommand(commandId: string): boolean {
  return fs.readdirSync(stateDir).some((name) => {
    if (!name.startsWith("response-") || !name.endsWith(".json")) {
      return false;
    }
    try {
      const completed = JSON.parse(
        fs.readFileSync(path.join(stateDir, name), "utf8"),
      ) as CompletedTrial;
      return completed.commandId === commandId;
    } catch {
      return false;
    }
  });
}

function sendToClient(socket: net.Socket, payload: object): void {
  socket.write(`${JSON.stringify(payload)}\n`);
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
