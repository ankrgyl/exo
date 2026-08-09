import os from "node:os";
import path from "node:path";

export type TrialRun = {
  type: "trial_run";
  request_id: string;
  target: string;
  container_id: string;
  instructions: string;
  deadline_at?: string | null;
};

export type TrialComplete = {
  type: "trial_complete";
  request_id: string;
  target: string;
  conversation_id: string;
  summary?: string | null;
};

export type TrialStarted = {
  type: "trial_started";
  request_id: string;
  target: string;
  conversation_id: string;
};

export type TrialResponse = TrialStarted | TrialComplete;

export function defaultSocketPath(homedir: string = os.homedir()): string {
  return path.join(homedir, ".exo", "trial.sock");
}

export function parseTrialRun(value: unknown): TrialRun {
  const record = objectValue(value, "trial_run");
  if (record.type !== "trial_run") {
    throw new Error("request type must be trial_run");
  }
  return {
    type: "trial_run",
    request_id: stringValue(record.request_id, "request_id"),
    target: stringValue(record.target, "target"),
    container_id: stringValue(record.container_id, "container_id"),
    instructions: stringValue(record.instructions, "instructions"),
    deadline_at: nullableString(record.deadline_at, "deadline_at"),
  };
}

export function parseTrialResponse(
  text: string,
  request: TrialRun,
): TrialResponse {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw new Error("trial response must be a JSON object");
  }
  const record = objectValue(value, "trial response");
  if (record.type !== "trial_started" && record.type !== "trial_complete") {
    throw new Error("response type must be trial_started or trial_complete");
  }
  const requestId = stringValue(record.request_id, "request_id");
  if (requestId !== request.request_id) {
    throw new Error(
      `response request_id ${requestId} does not match ${request.request_id}`,
    );
  }
  const target = stringValue(record.target, "target");
  if (target !== request.target) {
    throw new Error(
      `response target ${target} does not match ${request.target}`,
    );
  }
  const conversationId = stringValue(record.conversation_id, "conversation_id");
  if (record.type === "trial_started") {
    return {
      type: "trial_started",
      request_id: requestId,
      target,
      conversation_id: conversationId,
    };
  }
  return {
    type: "trial_complete",
    request_id: requestId,
    target,
    conversation_id: conversationId,
    summary: nullableString(record.summary, "summary"),
  };
}

export function composeTrialPrompt(
  request: TrialRun,
  adapterId: string,
  resuming: boolean,
): string {
  const completion = JSON.stringify({
    type: "trial_complete",
    request_id: request.request_id,
    summary: "optional short summary",
  });
  return [
    resuming
      ? `Trial \`${request.target}\` is still active after Exo restarted. Continue the work in its attached container.`
      : `Trial \`${request.target}\` has started. Your shell is attached to its task container.`,
    request.deadline_at ? `Deadline: ${request.deadline_at}` : "",
    "",
    "Instructions:",
    request.instructions,
    "",
    `When the trial is finished, call send_adapter_message exactly once with adapterId \`${adapterId}\`, target \`${request.target}\`, and text containing ${completion}. Ending a model turn does not complete the trial.`,
  ]
    .filter((line) => line !== "")
    .join("\n");
}

type JsonObject = Record<string, unknown>;

function objectValue(value: unknown, name: string): JsonObject {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${name} must be a JSON object`);
  }
  return value as JsonObject;
}

function stringValue(value: unknown, name: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${name} must be a non-empty string`);
  }
  return value;
}

function nullableString(value: unknown, name: string): string | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "string") {
    throw new Error(`${name} must be null or a string`);
  }
  return value;
}
