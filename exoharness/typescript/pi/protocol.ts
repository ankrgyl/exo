import type { JsonValue } from "@exo/harness";

export interface PiWorkerRequest {
  prompt: string;
  systemPrompt: string;
  model: string;
  baseUrl?: string;
  cwd: string;
}

export interface PiUsage {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
  totalTokens: number;
  cost: number;
}

export interface PiWorkerRunResult {
  status: "finished" | "error";
  finalText: string;
  model: string;
  provider: string;
  usage: PiUsage;
  error?: string;
}

export type PiWorkerEvent =
  | { type: "delta"; text: string }
  | { type: "message"; message: JsonValue }
  | {
      type: "tool_start";
      callId: string;
      name: string;
      args: JsonValue;
    }
  | {
      type: "tool_end";
      callId: string;
      name: string;
      result: JsonValue;
      isError: boolean;
    }
  | { type: "retry"; phase: "start" | "end"; details: JsonValue }
  | {
      type: "compaction";
      phase: "start" | "end";
      details: JsonValue;
    }
  | { type: "completed"; result: PiWorkerRunResult }
  | { type: "error"; message: string; error: JsonValue };

export interface PiModelReference {
  provider: string;
  model: string;
}

export function parsePiModelReference(value: string): PiModelReference {
  const separator = value.indexOf("/");
  if (separator <= 0 || separator === value.length - 1) {
    throw new Error(
      `Pi model must be provider-qualified (for example anthropic/claude-sonnet-4-6); received: ${value}`,
    );
  }
  return {
    provider: value.slice(0, separator),
    model: value.slice(separator + 1),
  };
}

export function toPiJson(value: unknown): JsonValue {
  if (value === undefined) {
    return null;
  }
  const seen = new WeakSet<object>();
  const serialized = JSON.stringify(value, (_key, item: unknown) => {
    if (typeof item === "bigint") {
      return item.toString();
    }
    if (item instanceof Error) {
      return {
        name: item.name,
        message: item.message,
        stack: item.stack,
      };
    }
    if (item && typeof item === "object") {
      if (seen.has(item)) {
        return "[circular]";
      }
      seen.add(item);
    }
    return item;
  });
  return serialized === undefined
    ? null
    : (JSON.parse(serialized) as JsonValue);
}
