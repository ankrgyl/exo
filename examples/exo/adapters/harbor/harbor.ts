import os from "node:os";
import path from "node:path";

export type HarborTaskStarted = {
  type: "task_started";
  message_id: string;
  trial_id: string;
  task_name: string;
  instruction: string;
  conversation_id: string;
  sandbox_id: string;
  deadline_at?: string | null;
};

export type HarborVerificationResult = {
  type: "verification_result";
  message_id: string;
  trial_id: string;
  task_name: string;
  conversation_id: string;
  rewards?: Record<string, number> | null;
  verifier_stdout?: string | null;
  verifier_stderr?: string | null;
  exception?: {
    type: string;
    message: string;
    traceback?: string | null;
  } | null;
};

export type HarborRequest = HarborTaskStarted | HarborVerificationResult;

export type HarborTaskComplete = {
  type: "task_complete";
  trial_id: string;
  summary?: string | null;
};

export type HarborFeedbackProcessed = {
  type: "feedback_processed";
  trial_id: string;
  summary?: string | null;
};

export type HarborResponse = HarborTaskComplete | HarborFeedbackProcessed;

export type HarborClientResponse =
  | { type: "response"; event: HarborResponse }
  | { type: "error"; message: string };

export function defaultSocketPath(homedir: string = os.homedir()): string {
  return path.join(homedir, ".exo", "harbor.sock");
}

export function requestTarget(request: HarborRequest): string {
  return `harbor:${request.trial_id}:${request.type}`;
}

export function expectedResponseType(
  request: HarborRequest,
): HarborResponse["type"] {
  return request.type === "task_started"
    ? "task_complete"
    : "feedback_processed";
}

export function parseHarborRequest(value: unknown): HarborRequest {
  const record = objectValue(value, "Harbor request");
  const type = stringValue(record.type, "request type");
  const base = {
    message_id: stringValue(record.message_id, "message_id"),
    trial_id: stringValue(record.trial_id, "trial_id"),
    task_name: stringValue(record.task_name, "task_name"),
    conversation_id: stringValue(record.conversation_id, "conversation_id"),
  };

  if (type === "task_started") {
    return {
      type,
      ...base,
      instruction: stringValue(record.instruction, "instruction"),
      sandbox_id: stringValue(record.sandbox_id, "sandbox_id"),
      deadline_at: nullableString(record.deadline_at, "deadline_at"),
    };
  }
  if (type === "verification_result") {
    return {
      type,
      ...base,
      rewards: nullableNumberMap(record.rewards, "rewards"),
      verifier_stdout: nullableString(
        record.verifier_stdout,
        "verifier_stdout",
      ),
      verifier_stderr: nullableString(
        record.verifier_stderr,
        "verifier_stderr",
      ),
      exception: nullableException(record.exception),
    };
  }
  throw new Error("request type must be task_started or verification_result");
}

export function parseHarborResponse(
  text: string,
  request: HarborRequest,
): HarborResponse {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw new Error("Harbor response text must be a JSON object");
  }
  const record = objectValue(value, "Harbor response");
  const type = stringValue(record.type, "response type");
  const expectedType = expectedResponseType(request);
  if (type !== expectedType) {
    throw new Error(`response type must be ${expectedType}`);
  }
  const trialId = stringValue(record.trial_id, "trial_id");
  if (trialId !== request.trial_id) {
    throw new Error(
      `response trial_id ${trialId} does not match active trial ${request.trial_id}`,
    );
  }
  const summary = nullableString(record.summary, "summary");
  if (type === "task_complete") {
    return { type, trial_id: trialId, summary };
  }
  return { type, trial_id: trialId, summary };
}

export function composeHarborPrompt(
  request: HarborRequest,
  adapterId: string,
  target: string,
): string {
  const responseType = expectedResponseType(request);
  const responseExample = JSON.stringify({
    type: responseType,
    trial_id: request.trial_id,
    summary: "optional short summary",
  });
  const completionInstruction =
    `When you have finished this phase, call send_adapter_message exactly once ` +
    `with adapterId \`${adapterId}\`, target \`${target}\`, and text containing ` +
    `a JSON object shaped like \`${responseExample}\`. An ordinary assistant ` +
    `response does not finish the Harbor phase.`;

  if (request.type === "task_started") {
    const deadline = request.deadline_at
      ? `\nDeadline: ${request.deadline_at}`
      : "";
    return (
      `Harbor started trial \`${request.trial_id}\` for task ` +
      `\`${request.task_name}\`. Your shell tool is attached to Exoharness ` +
      `sandbox \`${request.sandbox_id}\`, which is the running Harbor task ` +
      `container.${deadline}\n\nTask instruction:\n${request.instruction}\n\n` +
      completionInstruction
    );
  }

  const rewards = JSON.stringify(request.rewards ?? null);
  const exception = JSON.stringify(request.exception ?? null);
  return (
    `Harbor finished verification for trial \`${request.trial_id}\` ` +
    `(task \`${request.task_name}\`). The task sandbox is detached. Review the ` +
    `result, reflect on what should be retained at agent level, and make any ` +
    `appropriate durable improvements.\n\nRewards: ${rewards}\n` +
    `Verifier stdout:\n${request.verifier_stdout ?? ""}\n\n` +
    `Verifier stderr:\n${request.verifier_stderr ?? ""}\n\n` +
    `Infrastructure exception: ${exception}\n\n${completionInstruction}`
  );
}

function objectValue(value: unknown, name: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${name} must be a JSON object`);
  }
  return value as Record<string, unknown>;
}

function stringValue(value: unknown, name: string): string {
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new Error(`${name} must be a non-empty string`);
  }
  return value;
}

function nullableString(
  value: unknown,
  name: string,
): string | null | undefined {
  if (value === undefined || value === null) {
    return value;
  }
  if (typeof value !== "string") {
    throw new Error(`${name} must be a string or null`);
  }
  return value;
}

function nullableNumberMap(
  value: unknown,
  name: string,
): Record<string, number> | null | undefined {
  if (value === undefined || value === null) {
    return value;
  }
  const record = objectValue(value, name);
  for (const [key, item] of Object.entries(record)) {
    if (typeof item !== "number" || !Number.isFinite(item)) {
      throw new Error(`${name}.${key} must be a finite number`);
    }
  }
  return record as Record<string, number>;
}

function nullableException(
  value: unknown,
): HarborVerificationResult["exception"] {
  if (value === undefined || value === null) {
    return value;
  }
  const record = objectValue(value, "exception");
  return {
    type: stringValue(record.type, "exception.type"),
    message: stringValue(record.message, "exception.message"),
    traceback: nullableString(record.traceback, "exception.traceback"),
  };
}
