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

export type TrialFeedback = {
  type: "trial_feedback";
  request_id: string;
  target: string;
  instructions: string;
  feedback: string;
  deadline_at?: string | null;
};

export type TrialRequest = TrialRun | TrialFeedback;

export type TrialCancel = {
  type: "trial_cancel";
  request_id: string;
  target: string;
};

export type TrialSocketRequest = TrialRequest | TrialCancel;

export type TrialComplete = {
  type: "trial_complete";
  request_id: string;
  target: string;
  conversation_id: string;
  snapshot_id: string;
  summary?: string | null;
};

export type FeedbackComplete = {
  type: "feedback_complete";
  request_id: string;
  target: string;
  conversation_id: string;
  summary?: string | null;
};

export type TrialCancelled = {
  type: "trial_cancelled";
  request_id: string;
  target: string;
  conversation_id: string;
  snapshot_id: string;
};

export type TrialStarted = {
  type: "trial_started";
  request_id: string;
  target: string;
  conversation_id: string;
};

export type FeedbackStarted = {
  type: "feedback_started";
  request_id: string;
  target: string;
  conversation_id: string;
  sandbox_id: string;
};

export type TrialResponse =
  | TrialStarted
  | FeedbackStarted
  | TrialComplete
  | FeedbackComplete
  | TrialCancelled;

export function defaultSocketPath(homedir: string = os.homedir()): string {
  return path.join(homedir, ".exo", "trial.sock");
}

export function parseTrialRequest(value: unknown): TrialRequest {
  const record = objectValue(value, "trial request");
  const common = {
    request_id: stringValue(record.request_id, "request_id"),
    target: stringValue(record.target, "target"),
    instructions: stringValue(record.instructions, "instructions"),
    deadline_at: nullableString(record.deadline_at, "deadline_at"),
  };
  if (record.type === "trial_run") {
    return {
      type: "trial_run",
      ...common,
      container_id: stringValue(record.container_id, "container_id"),
    };
  }
  if (record.type === "trial_feedback") {
    return {
      type: "trial_feedback",
      ...common,
      feedback: stringValue(record.feedback, "feedback"),
    };
  }
  throw new Error("request type must be trial_run or trial_feedback");
}

export function parseTrialSocketRequest(value: unknown): TrialSocketRequest {
  const record = objectValue(value, "trial socket request");
  if (record.type === "trial_cancel") {
    return {
      type: "trial_cancel",
      request_id: stringValue(record.request_id, "request_id"),
      target: stringValue(record.target, "target"),
    };
  }
  return parseTrialRequest(record);
}

export function parseTrialResponse(
  text: string,
  request: TrialRequest,
): TrialResponse {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw new Error("trial response must be a JSON object");
  }
  const record = objectValue(value, "trial response");
  const expectedStarted =
    request.type === "trial_run" ? "trial_started" : "feedback_started";
  const expectedComplete =
    request.type === "trial_run" ? "trial_complete" : "feedback_complete";
  if (
    record.type !== expectedStarted &&
    record.type !== expectedComplete &&
    record.type !== "trial_cancelled"
  ) {
    throw new Error(
      `response type must be ${expectedStarted} or ${expectedComplete}`,
    );
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
  if (record.type === "feedback_started") {
    return {
      type: "feedback_started",
      request_id: requestId,
      target,
      conversation_id: conversationId,
      sandbox_id: stringValue(record.sandbox_id, "sandbox_id"),
    };
  }
  if (record.type === "trial_cancelled") {
    return {
      type: "trial_cancelled",
      request_id: requestId,
      target,
      conversation_id: conversationId,
      snapshot_id: stringValue(record.snapshot_id, "snapshot_id"),
    };
  }
  const summary = nullableString(record.summary, "summary");
  if (record.type === "trial_complete") {
    return {
      type: "trial_complete",
      request_id: requestId,
      target,
      conversation_id: conversationId,
      snapshot_id: stringValue(record.snapshot_id, "snapshot_id"),
      summary,
    };
  }
  return {
    type: "feedback_complete",
    request_id: requestId,
    target,
    conversation_id: conversationId,
    summary,
  };
}

export function composeTrialPrompt(
  request: TrialRequest,
  adapterId: string,
  resuming: boolean,
): string {
  const feedback = request.type === "trial_feedback";
  const completion = JSON.stringify({
    type: feedback ? "feedback_complete" : "trial_complete",
    request_id: request.request_id,
    summary: "optional short summary",
  });
  const opening = feedback
    ? resuming
      ? `Feedback for trial \`${request.target}\` is still active after Exo restarted. Continue reflecting in the restored submitted environment.`
      : `Feedback for trial \`${request.target}\` is ready. Your shell is attached to a restored snapshot of the submitted task environment.`
    : resuming
      ? `Trial \`${request.target}\` is still active after Exo restarted. Continue the work in its attached container.`
      : `Trial \`${request.target}\` has started. Your shell is attached to its task container.`;
  const body = feedback
    ? [
        "Feedback:",
        request.feedback,
        "",
        "Reflection instructions:",
        request.instructions,
      ]
    : ["Instructions:", request.instructions];
  return [
    opening,
    request.deadline_at ? `Deadline: ${request.deadline_at}` : "",
    "",
    ...body,
    "",
    `When this ${feedback ? "reflection" : "trial"} is finished, call send_adapter_message exactly once with adapterId \`${adapterId}\`, target \`${request.target}\`, and text containing ${completion}. Ending a model turn does not complete the phase.`,
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
