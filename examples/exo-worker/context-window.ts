/**
 * Per-model context window resolution for ExoWorker prompt budgeting.
 *
 * Declared defaults cover curated worker models; unknown models fall back to a
 * conservative default. Provider 400s that include the real limit (e.g.
 * "maximum prompt length is 500000") update a process-local learned cache.
 */

/** Conservative default when the model id is unknown. */
export const DEFAULT_CONTEXT_WINDOW_TOKENS = 200_000;

/**
 * Known prompt/context windows (tokens). Keys are bare API model ids or
 * common prefixes; matching is substring / exact after stripping provider.
 *
 * Keep in sync with the host app's worker-model catalog when one exists
 * (ExoWorker ships standalone and does not import host packages).
 */
export const KNOWN_CONTEXT_WINDOWS: Readonly<Record<string, number>> = {
  // Anthropic — Fable / Opus / Sonnet 5 (+ Sonnet 4.6) are 1M; Haiku 4.5 is 200K.
  "claude-sonnet-5": 1_000_000,
  "claude-sonnet-4-6": 1_000_000,
  "claude-sonnet-4-5": 200_000,
  "claude-opus-5": 1_000_000,
  "claude-opus-4-8": 1_000_000,
  "claude-opus-4-7": 1_000_000,
  "claude-opus-4-6": 1_000_000,
  "claude-fable-5": 1_000_000,
  "claude-mythos": 1_000_000,
  "claude-haiku-4-5": 200_000,
  "claude-haiku": 200_000,
  // xAI
  "grok-4.6": 500_000,
  "grok-4.5": 500_000,
  "grok-4.3": 1_000_000,
  // Moonshot Kimi
  "kimi-k3": 1_000_000,
  "kimi-k2.7-code": 262_000,
  "kimi-k2.7": 262_000,
  "kimi-k2.6": 262_000,
  "kimi-k2.5": 262_000,
  // OpenAI GPT-5.x family
  "gpt-5.6-sol": 1_050_000,
  "gpt-5.6-terra": 1_050_000,
  "gpt-5.6-luna": 1_050_000,
  "gpt-5.6": 1_050_000,
  "gpt-5.5-pro": 1_050_000,
  "gpt-5.5": 1_050_000,
  "gpt-5.4": 1_050_000,
  "gpt-5": 1_050_000,
};

const learnedContextWindows = new Map<string, number>();

/** @internal test helper */
export function clearLearnedContextWindows(): void {
  learnedContextWindows.clear();
}

export function stripModelProviderPrefix(model: string): string {
  const idx = model.indexOf(":");
  return idx >= 0 ? model.slice(idx + 1) : model;
}

export function resolveContextWindowTokens(model: string): number {
  const id = stripModelProviderPrefix(model).trim().toLowerCase();
  if (!id) return DEFAULT_CONTEXT_WINDOW_TOKENS;

  const learned = learnedContextWindows.get(id);
  if (learned && learned > 0) return learned;

  const exact = KNOWN_CONTEXT_WINDOWS[id];
  if (exact) return exact;

  for (const [key, tokens] of Object.entries(KNOWN_CONTEXT_WINDOWS)) {
    if (id.startsWith(key) || key.startsWith(id)) return tokens;
  }

  return DEFAULT_CONTEXT_WINDOW_TOKENS;
}

export function learnContextWindowTokens(model: string, tokens: number): void {
  if (!Number.isFinite(tokens) || tokens < 1_000) return;
  const id = stripModelProviderPrefix(model).trim().toLowerCase();
  if (!id) return;
  const prev = learnedContextWindows.get(id);
  if (prev === tokens) return;
  learnedContextWindows.set(id, Math.floor(tokens));
  console.warn(
    `[exo-worker] learned context window for ${id}: ${Math.floor(tokens)} tokens` +
      (prev ? ` (was ${prev})` : ""),
  );
}

export type CompressionBudgets = {
  contextWindowTokens: number;
  usableInputTokens: number;
  thresholdTokens: number;
  targetTokens: number;
};

export function compressionBudgetsForModel(
  model: string,
  maxOutputTokens: number,
  safetyTokens = 4_000,
): CompressionBudgets {
  const contextWindowTokens = resolveContextWindowTokens(model);
  const reserved = Math.max(0, maxOutputTokens) + safetyTokens;
  const usableInputTokens = Math.max(1_000, contextWindowTokens - reserved);
  return {
    contextWindowTokens,
    usableInputTokens,
    thresholdTokens: Math.floor(usableInputTokens * 0.55),
    targetTokens: Math.floor(usableInputTokens * 0.35),
  };
}

/** True when an LLM error looks like a context / prompt length overflow. */
export function isContextWindowError(error: unknown): boolean {
  const msg = errorMessage(error).toLowerCase();
  if (!msg) return false;
  return (
    msg.includes("maximum prompt length") ||
    msg.includes("context length") ||
    msg.includes("context window") ||
    msg.includes("prompt is too long") ||
    msg.includes("prompt_too_long") ||
    msg.includes("too many tokens") ||
    (msg.includes("max_tokens") && msg.includes("exceed")) ||
    /request.*tokens?.*exceed/i.test(msg) ||
    /exceeds?\s+(the\s+)?(context|maximum|model)/i.test(msg)
  );
}

/**
 * Parse a concrete context-window limit from provider error text.
 * Examples:
 *   - `maximum prompt length is 500000 but the request contains 6113480 tokens`
 *   - `maximum context length is 200000 tokens`
 */
export function parseContextWindowFromError(error: unknown): number | null {
  const msg = errorMessage(error);
  if (!msg) return null;

  const patterns = [
    /maximum prompt length is\s+(\d+)/i,
    /maximum context length is\s+(\d+)/i,
    /context window.*?(\d+)\s*tokens/i,
    /max(?:imum)?(?:\s+prompt)?(?:\s+context)?(?:\s+length)?[^\d]{0,40}(\d{4,})/i,
  ];
  for (const pattern of patterns) {
    const match = pattern.exec(msg);
    if (!match?.[1]) continue;
    const n = Number.parseInt(match[1], 10);
    if (Number.isFinite(n) && n >= 1_000) return n;
  }
  return null;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error) ?? "";
  } catch {
    return String(error);
  }
}
