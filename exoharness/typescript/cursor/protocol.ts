import type { JsonValue } from "@exo/harness";

export interface CursorWorkerRequest {
  prompt: string;
  model: string;
  cwd: string;
  name?: string;
}

export type CursorWorkerEvent =
  | {
      type: "run_started";
      agentId: string;
      runId: string;
    }
  | {
      type: "delta";
      update: JsonValue;
    }
  | {
      type: "message";
      message: JsonValue;
    }
  | {
      type: "completed";
      agentId: string;
      runId: string;
      result: CursorWorkerRunResult;
    }
  | {
      type: "error";
      message: string;
      error: JsonValue;
    };

export interface CursorWorkerRunResult {
  id: string;
  status: "finished" | "error" | "cancelled";
  result?: string;
  model?: JsonValue;
  durationMs?: number;
  git?: JsonValue;
}

export function cursorModelId(model: string): string {
  return model === "auto" ? "default" : model;
}

export function toCursorJson(value: unknown): JsonValue {
  if (value === undefined) {
    return null;
  }
  return JSON.parse(JSON.stringify(value)) as JsonValue;
}
