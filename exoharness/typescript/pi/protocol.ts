import type { JsonValue } from "@exo/harness";

export interface PiWorkerRequest {
  requestId: string;
  prompt: string;
  systemPrompt: string;
  model: string;
  baseUrl?: string;
  cwd: string;
  maxOutputTokens?: number;
  maxToolRoundTrips?: number;
}

export interface PiUsage {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
  reasoning?: number;
  totalTokens: number;
  cost: number;
}

export interface PiWorkerRunResult {
  status: "finished" | "error";
  finalText: string;
  model: string;
  provider: string;
  usage: PiUsage;
  durationMs: number;
  error?: string;
}

type RequestEvent<T> = T & { requestId: string };

export type PiWorkerEvent =
  | RequestEvent<{ type: "run_started" }>
  | RequestEvent<{ type: "delta"; text: string }>
  | RequestEvent<{
      type: "message";
      message: JsonValue;
      durationMs?: number;
      ttftMs?: number;
    }>
  | RequestEvent<{
      type: "tool_start";
      callId: string;
      name: string;
      args: JsonValue;
    }>
  | RequestEvent<{
      type: "tool_end";
      callId: string;
      name: string;
      result: JsonValue;
      isError: boolean;
    }>
  | RequestEvent<{
      type: "retry";
      phase: "start" | "end";
      details: JsonValue;
    }>
  | RequestEvent<{
      type: "compaction";
      phase: "start" | "end";
      details: JsonValue;
    }>
  | RequestEvent<{ type: "completed"; result: PiWorkerRunResult }>
  | RequestEvent<{ type: "error"; message: string; error: JsonValue }>;

export interface PiModelReference {
  provider: string;
  model: string;
}

export function parsePiWorkerEvent(line: string): PiWorkerEvent {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line) as unknown;
  } catch {
    throw invalidWorkerEvent(line);
  }
  if (
    !isRecord(parsed) ||
    typeof parsed.type !== "string" ||
    typeof parsed.requestId !== "string" ||
    !parsed.requestId
  ) {
    throw new Error(`invalid Pi sandbox worker event: ${line}`);
  }
  switch (parsed.type) {
    case "run_started":
      break;
    case "delta":
      requireString(parsed, "text", line);
      break;
    case "message":
      requireField(parsed, "message", line);
      requireOptionalNonNegativeNumber(parsed, "durationMs", line);
      requireOptionalNonNegativeNumber(parsed, "ttftMs", line);
      break;
    case "tool_start":
      requireString(parsed, "callId", line);
      requireString(parsed, "name", line);
      requireField(parsed, "args", line);
      break;
    case "tool_end":
      requireString(parsed, "callId", line);
      requireString(parsed, "name", line);
      requireField(parsed, "result", line);
      if (typeof parsed.isError !== "boolean") {
        throw invalidWorkerEvent(line);
      }
      break;
    case "retry":
    case "compaction":
      if (parsed.phase !== "start" && parsed.phase !== "end") {
        throw invalidWorkerEvent(line);
      }
      requireField(parsed, "details", line);
      break;
    case "completed":
      if (!isRecord(parsed.result)) {
        throw invalidWorkerEvent(line);
      }
      if (
        parsed.result.status !== "finished" &&
        parsed.result.status !== "error"
      ) {
        throw invalidWorkerEvent(line);
      }
      requireString(parsed.result, "finalText", line);
      requireString(parsed.result, "model", line);
      requireString(parsed.result, "provider", line);
      if (
        !isRecord(parsed.result.usage) ||
        !isFiniteNumber(parsed.result.durationMs) ||
        parsed.result.durationMs < 0
      ) {
        throw invalidWorkerEvent(line);
      }
      for (const field of [
        "input",
        "output",
        "cacheRead",
        "cacheWrite",
        "totalTokens",
        "cost",
      ]) {
        const value = parsed.result.usage[field];
        if (!isFiniteNumber(value) || value < 0) {
          throw invalidWorkerEvent(line);
        }
      }
      if (
        parsed.result.usage.reasoning !== undefined &&
        (!isFiniteNumber(parsed.result.usage.reasoning) ||
          parsed.result.usage.reasoning < 0)
      ) {
        throw invalidWorkerEvent(line);
      }
      break;
    case "error":
      requireString(parsed, "message", line);
      requireField(parsed, "error", line);
      break;
    default:
      throw invalidWorkerEvent(line);
  }
  return parsed as PiWorkerEvent;
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

function requireString(
  record: Record<string, unknown>,
  field: string,
  line: string,
): void {
  if (typeof record[field] !== "string") {
    throw invalidWorkerEvent(line);
  }
}

function requireField(
  record: Record<string, unknown>,
  field: string,
  line: string,
): void {
  if (!(field in record)) {
    throw invalidWorkerEvent(line);
  }
}

function requireOptionalNonNegativeNumber(
  record: Record<string, unknown>,
  field: string,
  line: string,
): void {
  const value = record[field];
  if (value !== undefined && (!isFiniteNumber(value) || value < 0)) {
    throw invalidWorkerEvent(line);
  }
}

function invalidWorkerEvent(line: string): Error {
  return new Error(`invalid Pi sandbox worker event: ${line}`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
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
