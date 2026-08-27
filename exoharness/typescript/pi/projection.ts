import type { JsonObject, JsonValue } from "@exo/harness";

import type { PiWorkerRunResult } from "./protocol";

export interface PiAssistantProjection {
  text: string;
  usage?: JsonObject;
}

export interface PiAssistantProjectionOptions {
  upstreamModel: string;
  durationMs?: number;
  ttftMs?: number | null;
}

export function projectPiAssistantMessage(
  message: JsonValue,
  options: PiAssistantProjectionOptions,
): PiAssistantProjection | null {
  if (!isRecord(message) || message.role !== "assistant") {
    return null;
  }
  const content = Array.isArray(message.content) ? message.content : [];
  const text = content
    .map((part) => {
      return isRecord(part) &&
        part.type === "text" &&
        typeof part.text === "string"
        ? part.text
        : "";
    })
    .join("");
  const usage = usageRecordFromRawMessage(message, options);
  return { text, usage };
}

export function piResultUsageRecord(
  result: PiWorkerRunResult,
  ttftMs: number | null,
): JsonObject {
  const record: JsonObject = {
    model: result.model,
    prompt_tokens: result.usage.input,
    completion_tokens: result.usage.output,
    prompt_cached_tokens: result.usage.cacheRead,
    prompt_cache_creation_tokens: result.usage.cacheWrite,
    cost_usd: result.usage.cost,
    duration_ms: result.durationMs,
  };
  if (result.usage.reasoning !== undefined) {
    record.completion_reasoning_tokens = result.usage.reasoning;
  }
  if (ttftMs !== null) {
    record.ttft_ms = ttftMs;
  }
  return record;
}

function usageRecordFromRawMessage(
  message: Record<string, JsonValue>,
  options: PiAssistantProjectionOptions,
): JsonObject | undefined {
  const usage = isRecord(message.usage) ? message.usage : null;
  if (!usage) {
    return undefined;
  }
  const record: JsonObject = {
    model:
      typeof message.model === "string" ? message.model : options.upstreamModel,
  };
  copyNumber(record, "prompt_tokens", usage.input);
  copyNumber(record, "completion_tokens", usage.output);
  copyNumber(record, "prompt_cached_tokens", usage.cacheRead);
  copyNumber(record, "prompt_cache_creation_tokens", usage.cacheWrite);
  copyNumber(record, "completion_reasoning_tokens", usage.reasoning);
  const cost = isRecord(usage.cost) ? usage.cost.total : undefined;
  copyNumber(record, "cost_usd", cost);
  if (options.durationMs !== undefined) {
    record.duration_ms = options.durationMs;
  }
  if (options.ttftMs !== null && options.ttftMs !== undefined) {
    record.ttft_ms = options.ttftMs;
  }
  return record;
}

function copyNumber(
  target: JsonObject,
  key: string,
  value: JsonValue | undefined,
): void {
  if (typeof value === "number" && Number.isFinite(value)) {
    target[key] = value;
  }
}

function isRecord(value: unknown): value is Record<string, JsonValue> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
