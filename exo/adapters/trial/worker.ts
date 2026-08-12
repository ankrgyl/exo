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
  parseTrialResponse,
  parseTrialSocketRequest,
  type TrialRequest,
  type TrialResponse,
  type TrialSocketRequest,
} from "./trial";

type PendingTrial = {
  request: TrialRequest;
  sockets: Set<net.Socket>;
};

type CompletedTrial = {
  commandIds: string[];
  response: TrialResponse;
};

type StartedTrial = {
  commandIds: string[];
  response: TrialResponse;
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
    let request: TrialSocketRequest;
    try {
      request = parseTrialSocketRequest(JSON.parse(line));
    } catch (error) {
      sendToClient(socket, { type: "error", message: errorText(error) });
      return;
    }

    if (request.type === "trial_cancel") {
      cancelTrial(request.request_id, request.target, socket);
      return;
    }

    const completed = readCompletedTrial(request.request_id);
    if (completed) {
      sendToClient(socket, { type: "response", event: completed.response });
      return;
    }

    const targetResponses = completedResponsesForTarget(request.target);
    const completedTrial = targetResponses.some(
      ({ response }) =>
        response.type === "trial_complete" ||
        response.type === "trial_cancelled",
    );
    const completedFeedback = targetResponses.some(
      ({ response }) => response.type === "feedback_complete",
    );
    if (request.type === "trial_run" && completedTrial) {
      sendToClient(socket, {
        type: "error",
        message: `trial target ${request.target} was already used`,
      });
      return;
    }
    if (request.type === "trial_feedback" && !completedTrial) {
      sendToClient(socket, {
        type: "error",
        message: `trial target ${request.target} has no completed trial`,
      });
      return;
    }
    if (request.type === "trial_feedback" && completedFeedback) {
      sendToClient(socket, {
        type: "error",
        message: `trial target ${request.target} already completed feedback`,
      });
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
      const started = readStartedTrial(request.request_id);
      if (started) {
        sendToClient(socket, { type: "event", event: started.response });
      }
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

    const response = parseTrialResponse(command.text, pending.request);
    if (
      response.type === "trial_started" ||
      response.type === "feedback_started"
    ) {
      const started = readStartedTrial(pending.request.request_id);
      if (
        started &&
        started.response.conversation_id !== response.conversation_id
      ) {
        throw new Error(
          `trial target ${target} changed conversations while active`,
        );
      }
      writeStartedTrial(pending.request.request_id, {
        commandIds: [...(started?.commandIds ?? []), command.id],
        response,
      });
      if (!started) {
        for (const socket of pending.sockets) {
          sendToClient(socket, { type: "event", event: response });
        }
      }
      writeWorkerEvent({ type: "command_ack", command_id: command.id });
      continue;
    }

    const started = readStartedTrial(pending.request.request_id);
    writeCompletedTrial(pending.request.request_id, {
      commandIds: [...(started?.commandIds ?? []), command.id],
      response,
    });
    removePendingTrial(pending.request.request_id);
    removeStartedTrial(pending.request.request_id);
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

function emitTrialRun(request: TrialRequest, resuming: boolean): void {
  writeWorkerEvent({
    type: "message",
    target: request.target,
    sender: "trial_runner",
    text: composeTrialPrompt(request, adapterId, resuming),
    message_id: resuming
      ? `${request.request_id}:resume:${startupId}`
      : request.request_id,
    metadata: {
      type: request.type,
      request_id: request.request_id,
      ...(request.type === "trial_run"
        ? { container_id: request.container_id }
        : {}),
    },
  });
}

function loadPendingTrials(): void {
  for (const name of fs.readdirSync(stateDir)) {
    if (!name.startsWith("pending-") || !name.endsWith(".json")) {
      continue;
    }
    const file = path.join(stateDir, name);
    const request = parseTrialSocketRequest(
      JSON.parse(fs.readFileSync(file, "utf8")),
    );
    if (request.type === "trial_cancel") {
      throw new Error("trial cancellation cannot be pending");
    }
    if (readCompletedTrial(request.request_id)) {
      fs.rmSync(file, { force: true });
      removeStartedTrial(request.request_id);
      continue;
    }
    if (pendingByTarget.has(request.target)) {
      throw new Error(`multiple pending requests for target ${request.target}`);
    }
    pendingByTarget.set(request.target, { request, sockets: new Set() });
  }
}

function cancelTrial(
  requestId: string,
  target: string,
  socket: net.Socket,
): void {
  const pending = pendingByTarget.get(target);
  if (pending && pending.request.request_id !== requestId) {
    sendToClient(socket, {
      type: "error",
      message: `trial target ${target} is active under another request`,
    });
    return;
  }
  if (pending) {
    pending.sockets.add(socket);
    writeWorkerEvent({
      type: "control",
      target,
      metadata: { type: "trial_cancel", request_id: requestId },
    });
    return;
  }
  sendToClient(socket, {
    type: "error",
    message: `trial target ${target} is not active`,
  });
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

function startedPath(requestId: string): string {
  return statePath("started", requestId);
}

function statePath(kind: string, requestId: string): string {
  const digest = crypto.createHash("sha256").update(requestId).digest("hex");
  return path.join(stateDir, `${kind}-${digest}.json`);
}

function writePendingTrial(request: TrialRequest): void {
  writeJsonAtomically(pendingPath(request.request_id), request);
}

function removePendingTrial(requestId: string): void {
  fs.rmSync(pendingPath(requestId), { force: true });
}

function readStartedTrial(requestId: string): StartedTrial | null {
  try {
    return JSON.parse(
      fs.readFileSync(startedPath(requestId), "utf8"),
    ) as StartedTrial;
  } catch {
    return null;
  }
}

function writeStartedTrial(requestId: string, started: StartedTrial): void {
  writeJsonAtomically(startedPath(requestId), started);
}

function removeStartedTrial(requestId: string): void {
  fs.rmSync(startedPath(requestId), { force: true });
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

function completedResponsesForTarget(target: string): CompletedTrial[] {
  const completed: CompletedTrial[] = [];
  for (const name of fs.readdirSync(stateDir)) {
    if (!name.startsWith("response-") || !name.endsWith(".json")) {
      continue;
    }
    const response = JSON.parse(
      fs.readFileSync(path.join(stateDir, name), "utf8"),
    ) as CompletedTrial;
    if (response.response.target === target) {
      completed.push(response);
    }
  }
  return completed;
}

function writeCompletedTrial(
  requestId: string,
  completed: CompletedTrial,
): void {
  writeJsonAtomically(completedPath(requestId), completed);
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
      return completed.commandIds.includes(commandId);
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
