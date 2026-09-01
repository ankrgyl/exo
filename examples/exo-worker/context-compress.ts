import type { Message } from "@exo/harness";

import {
  compressionBudgetsForModel,
  type CompressionBudgets,
} from "./context-window.js";

/**
 * ExoWorker context compression — target-based compression adapted to Lingua
 * message shapes (`tool_call` / `tool_result`) and ExoWorker's
 * instruction+history prompt layout.
 *
 * Testing: pass a fake `summarize` function — no live model or worker needed.
 */

export const COMPRESSED_MARKER = "[COMPRESSED PRIOR WORK]";

/**
 * Second line of a persisted compression marker. Hosts may parse this to
 * surface compression stats in their own UI / event mirrors.
 */
export const COMPRESSION_META_PREFIX = "exo_worker_compression_meta:";

/** Keep the most recent N messages out of compression (working memory). */
export const RECENT_MESSAGES_TO_KEEP_FULL = 12;

/**
 * Text/JSON heuristic for pre-call budgeting. Never use this for UI as "real"
 * usage — prefer provider `usage.input_tokens` after the round.
 */
export const TOKEN_ESTIMATE_CHARS_PER_TOKEN = 4;

/**
 * Fixed budget per vision image part. Providers do not bill images as
 * base64-char/4; counting payload bytes produced multi‑million fake totals.
 */
export const IMAGE_TOKEN_BUDGET = 1_600;

/**
 * Replace multimodal image parts with short text notes. Used on context-window
 * retry — providers may bill large PNGs far above {@link IMAGE_TOKEN_BUDGET}.
 */
export function stripVisionImageParts(messages: Message[]): {
  messages: Message[];
  strippedCount: number;
} {
  let strippedCount = 0;
  const next = messages.map((msg) => {
    if (!Array.isArray(msg.content)) return msg;
    let changed = false;
    const content = msg.content.map((part) => {
      if (!part || typeof part !== "object") return part;
      const p = part as { type?: string };
      if (p.type !== "image" && p.type !== "image-data" && p.type !== "media") {
        return part;
      }
      changed = true;
      strippedCount += 1;
      return {
        type: "text" as const,
        text: "[image omitted to fit model context window — re-run a single-slide preview/screenshot if needed]",
      };
    });
    return changed ? { ...msg, content: content as Message["content"] } : msg;
  });
  return { messages: next, strippedCount };
}

export const SUMMARY_OVERHEAD_TOKENS = 3_000;

export const DEFAULT_MAX_OUTPUT_TOKENS = 16_000;

export type SummarizeFn = (prompt: string) => Promise<string>;

export type CompressMessagesOptions = {
  model: string;
  maxOutputTokens?: number | null;
  /** Injected summarizer — tests pass a stub; production uses the LLM runtime. */
  summarize: SummarizeFn;
  /**
   * Persist the compression marker into the exo event log so later rounds
   * skip older events via {@link applyCompressionMarkerWindow}.
   */
  persistMarker?: (marker: Message) => Promise<void>;
  /** Force compression even when under the soft threshold (reactive retry). */
  force?: boolean;
  /**
   * Exact prompt tokens from the previous provider response. When near the
   * soft threshold, we compress even if the local estimate is optimistic.
   */
  lastProviderPromptTokens?: number | null;
  /** Override budgets (tests / aggressive retry). */
  budgets?: Partial<CompressionBudgets>;
};

export type CompressionMeta = {
  compressedMessageCount: number;
  keptMessageCount: number;
  /** Pre-call heuristic only — not provider usage. */
  estimatedBeforeTokens: number;
  /** Pre-call heuristic only — not provider usage. */
  estimatedAfterTokens: number;
  force: boolean;
};

export type CompressMessagesResult = {
  messages: Message[];
  compressed: boolean;
  estimatedBeforeTokens: number;
  estimatedAfterTokens: number;
  compressedCount: number;
  keptCount: number;
  meta: CompressionMeta | null;
};

export function estimateMessageTokens(message: Message): number {
  return estimateContentTokens(message.content);
}

export function estimateTokens(messages: Message[]): number {
  return messages.reduce((sum, m) => sum + estimateMessageTokens(m), 0);
}

/** Pre-call estimate: text ≈ chars/4; each image part = {@link IMAGE_TOKEN_BUDGET}. */
export function estimateContentTokens(content: unknown): number {
  if (typeof content === "string") {
    return Math.ceil(
      stripBase64LikeForEstimate(content).length /
        TOKEN_ESTIMATE_CHARS_PER_TOKEN,
    );
  }
  if (!Array.isArray(content)) {
    if (content && typeof content === "object") {
      try {
        return Math.ceil(
          stripBase64LikeForEstimate(JSON.stringify(content) ?? "").length /
            TOKEN_ESTIMATE_CHARS_PER_TOKEN,
        );
      } catch {
        return 0;
      }
    }
    return 0;
  }

  let tokens = 0;
  for (const part of content) {
    if (!part || typeof part !== "object") {
      if (typeof part === "string") {
        tokens += Math.ceil(
          stripBase64LikeForEstimate(part).length /
            TOKEN_ESTIMATE_CHARS_PER_TOKEN,
        );
      }
      continue;
    }
    const p = part as {
      type?: string;
      text?: string;
      image?: unknown;
      image_url?: unknown;
    };
    if (
      p.type === "image" ||
      p.type === "input_image" ||
      p.image != null ||
      p.image_url != null
    ) {
      tokens += IMAGE_TOKEN_BUDGET;
      continue;
    }
    if (typeof p.text === "string") {
      tokens += Math.ceil(
        stripBase64LikeForEstimate(p.text).length /
          TOKEN_ESTIMATE_CHARS_PER_TOKEN,
      );
      continue;
    }
    try {
      tokens += Math.ceil(
        stripBase64LikeForEstimate(JSON.stringify(part) ?? "").length /
          TOKEN_ESTIMATE_CHARS_PER_TOKEN,
      );
    } catch {
      // ignore
    }
  }
  return tokens;
}

/** Drop long base64-looking runs so estimates are not poisoned by image payloads. */
export function stripBase64LikeForEstimate(text: string): string {
  if (text.length < 256) return text;
  return text.replace(
    /(?:data:[^;]+;base64,)?[A-Za-z0-9+/]{256,}={0,2}/g,
    "[base64 omitted]",
  );
}

export function messageTextContent(message: Message): string {
  if (typeof message.content === "string") return message.content.trim();
  if (!Array.isArray(message.content)) return "";
  const parts: string[] = [];
  for (const block of message.content) {
    if (!block || typeof block !== "object") continue;
    const b = block as { type?: string; text?: string };
    if (b.type === "text" && typeof b.text === "string") parts.push(b.text);
  }
  return parts.join("\n").trim();
}

export function isCompressedMarkerMessage(message: Message): boolean {
  if (message.role !== "assistant") return false;
  return messageTextContent(message).startsWith(COMPRESSED_MARKER);
}

export function findLastCompressedIdx(messages: Message[]): number {
  for (let i = messages.length - 1; i >= 0; i--) {
    if (isCompressedMarkerMessage(messages[i]!)) return i;
  }
  return -1;
}

/**
 * After materializing history, drop events before the latest compression
 * marker. Keeps the earliest user message (client request) if it sits before
 * the marker so the original ask is never lost.
 */
export function applyCompressionMarkerWindow(messages: Message[]): Message[] {
  const markerIdx = findLastCompressedIdx(messages);
  if (markerIdx < 0) return messages;

  const kept: Message[] = [];
  const firstUserIdx = messages.findIndex((m) => m.role === "user");
  if (firstUserIdx >= 0 && firstUserIdx < markerIdx) {
    kept.push(messages[firstUserIdx]!);
  }
  kept.push(...messages.slice(markerIdx));
  return kept;
}

export function hasToolCallPart(message: Message): boolean {
  if (message.role !== "assistant" || !Array.isArray(message.content)) {
    return false;
  }
  return message.content.some(
    (p) =>
      p &&
      typeof p === "object" &&
      ((p as { type?: string }).type === "tool_call" ||
        (p as { type?: string }).type === "tool-call"),
  );
}

export function hasToolResultPart(message: Message): boolean {
  if (!Array.isArray(message.content)) return false;
  return message.content.some(
    (p) =>
      p &&
      typeof p === "object" &&
      ((p as { type?: string }).type === "tool_result" ||
        (p as { type?: string }).type === "tool-result"),
  );
}

function consecutiveToolMessageIndices(
  messages: Message[],
  start: number,
): number[] {
  const indices: number[] = [];
  for (let j = start; j < messages.length; j++) {
    if (messages[j]!.role !== "tool") break;
    indices.push(j);
  }
  return indices;
}

/** Extend a set so tool_call / tool_result groups stay intact. */
export function extendToolPairIndices(
  messages: Message[],
  indices: Set<number>,
): void {
  for (const i of Array.from(indices)) {
    const m = messages[i]!;
    if (hasToolResultPart(m) && i - 1 >= 0) {
      const prev = messages[i - 1]!;
      if (prev.role === "assistant" && hasToolCallPart(prev)) {
        indices.add(i - 1);
        for (const j of consecutiveToolMessageIndices(messages, i)) {
          indices.add(j);
        }
      }
    }
    if (
      m.role === "assistant" &&
      hasToolCallPart(m) &&
      i + 1 < messages.length
    ) {
      for (const j of consecutiveToolMessageIndices(messages, i + 1)) {
        if (hasToolResultPart(messages[j]!)) indices.add(j);
      }
    }
  }
}

/**
 * Pin: leading developer/system instructions, first user (client request),
 * recent tail, existing compression markers. Then pair-extend.
 */
export function selectPinIndices(messages: Message[]): Set<number> {
  const pinIndices = new Set<number>();
  if (messages.length === 0) return pinIndices;

  let i = 0;
  while (
    i < messages.length &&
    (messages[i]!.role === "developer" || messages[i]!.role === "system")
  ) {
    pinIndices.add(i);
    i += 1;
  }

  const firstUserIdx = messages.findIndex((m) => m.role === "user");
  if (firstUserIdx >= 0) pinIndices.add(firstUserIdx);

  const keepFromIdx = Math.max(
    0,
    messages.length - RECENT_MESSAGES_TO_KEEP_FULL,
  );
  for (let idx = keepFromIdx; idx < messages.length; idx++) {
    pinIndices.add(idx);
  }

  for (let idx = 0; idx < messages.length; idx++) {
    if (isCompressedMarkerMessage(messages[idx]!)) pinIndices.add(idx);
  }

  extendToolPairIndices(messages, pinIndices);
  return pinIndices;
}

/**
 * Pure selection of indices to compress. Returns null when compression is
 * not warranted (under threshold, too few candidates, etc.).
 */
export function selectCompressIndices(
  messages: Message[],
  thresholdTokens: number,
  targetTokens: number,
  opts: { force?: boolean } = {},
): number[] | null {
  const totalTokens = estimateTokens(messages);
  if (!opts.force && totalTokens < thresholdTokens) return null;
  if (messages.length <= RECENT_MESSAGES_TO_KEEP_FULL + 2) return null;

  const pinIndices = selectPinIndices(messages);
  const nonPinnedOldestFirst: number[] = [];
  for (let idx = 0; idx < messages.length; idx++) {
    if (!pinIndices.has(idx)) nonPinnedOldestFirst.push(idx);
  }
  if (nonPinnedOldestFirst.length < 3) return null;

  const effectiveTarget = opts.force
    ? Math.min(targetTokens, Math.floor(totalTokens * 0.5))
    : targetTokens;

  const toCompressSet = new Set<number>();
  let accumulatedSavings = 0;
  for (const idx of nonPinnedOldestFirst) {
    const projectedAfter =
      totalTokens - accumulatedSavings + SUMMARY_OVERHEAD_TOKENS;
    if (!opts.force && projectedAfter <= effectiveTarget) break;
    if (
      opts.force &&
      projectedAfter <= effectiveTarget &&
      toCompressSet.size >= 3
    ) {
      break;
    }
    toCompressSet.add(idx);
    accumulatedSavings += estimateMessageTokens(messages[idx]!);
  }
  if (toCompressSet.size < 3) return null;

  extendToolPairIndices(messages, toCompressSet);
  for (const idx of Array.from(toCompressSet)) {
    if (pinIndices.has(idx)) toCompressSet.delete(idx);
  }
  if (toCompressSet.size < 3) return null;

  return Array.from(toCompressSet).sort((a, b) => a - b);
}

export function buildSummaryPrompt(
  messages: Message[],
  toCompressIndices: number[],
): string {
  const summaryInput = toCompressIndices
    .map((idx) => {
      const msg = messages[idx]!;
      const content =
        typeof msg.content === "string"
          ? msg.content
          : (() => {
              try {
                return JSON.stringify(msg.content);
              } catch {
                return "[unserializable]";
              }
            })();
      return `[${msg.role}]: ${content.slice(0, 1500)}`;
    })
    .join("\n\n");

  return `Summarize these older turns of an agent's work into a detailed status report. You MUST preserve every piece of specific, actionable evidence — file paths, URLs, tool outputs, decisions, errors and their fixes. Omit only fluff, narrative filler, and failed exploration that was abandoned without learning.

Target length: 1500-3000 words (~2000-4000 tokens). This is the ONLY memory the worker has of these ${toCompressIndices.length} messages going forward, so err on the side of keeping detail.

Conversation segment to summarize:
${summaryInput}

Output format — plain markdown with these sections (skip any that have no content):

## Files
Full paths of files created, modified, or read. One per line with a short note on what's in each.

## Deliverables and URLs
URLs, deployed endpoints, filenames, artifact types produced. One per line.

## Decisions
Framework, config, design, or implementation choices. Include the reasoning briefly.

## Errors and resolutions
Every error encountered, root cause, and how it was fixed. CRITICAL — do not drop these.

## Commands run
Non-trivial shell commands executed and their outcome.

## Visual artifacts
Screenshots taken and what they showed.

## Current state
A paragraph describing where the worker is in the task right now — what's done, what's in progress, any open thread.`;
}

export function rebuildWithCompression(
  messages: Message[],
  toCompressIndices: number[],
  summaryText: string,
  meta?: CompressionMeta,
): Message[] {
  const toCompressSet = new Set(toCompressIndices);
  const rebuilt: Message[] = [];
  let summaryInserted = false;
  for (let i = 0; i < messages.length; i++) {
    if (toCompressSet.has(i)) {
      if (!summaryInserted) {
        rebuilt.push(compressionMarkerMessage(summaryText, meta));
        summaryInserted = true;
      }
    } else {
      rebuilt.push(messages[i]!);
    }
  }
  return rebuilt;
}

/** Build the durable assistant marker written to exo's event log + in-memory prompt. */
export function compressionMarkerMessage(
  summaryText: string,
  meta?: CompressionMeta,
): Message {
  return {
    role: "assistant",
    content: formatCompressionMarkerContent(summaryText, meta),
  };
}

export function formatCompressionMarkerContent(
  summaryText: string,
  meta?: CompressionMeta,
): string {
  const lines = [COMPRESSED_MARKER];
  if (meta) {
    lines.push(`${COMPRESSION_META_PREFIX}${JSON.stringify(meta)}`);
  }
  lines.push(summaryText.trim());
  return lines.join("\n");
}

/**
 * Parse structured compression stats from a marker message body.
 * Returns null when the text is not a compression marker.
 */
export function parseCompressionMarkerMeta(
  text: string,
): CompressionMeta | null {
  const trimmed = text.trim();
  if (!trimmed.startsWith(COMPRESSED_MARKER)) return null;
  const lines = trimmed.split("\n");
  for (const line of lines.slice(1, 4)) {
    const raw = line.trim();
    if (!raw.startsWith(COMPRESSION_META_PREFIX)) continue;
    try {
      const parsed = JSON.parse(
        raw.slice(COMPRESSION_META_PREFIX.length),
      ) as Record<string, unknown>;
      if (
        typeof parsed.compressedMessageCount !== "number" ||
        typeof parsed.keptMessageCount !== "number"
      ) {
        return null;
      }
      // Accept legacy beforeTokens/afterTokens keys from older markers.
      const estimatedBefore =
        typeof parsed.estimatedBeforeTokens === "number"
          ? parsed.estimatedBeforeTokens
          : typeof parsed.beforeTokens === "number"
            ? parsed.beforeTokens
            : 0;
      const estimatedAfter =
        typeof parsed.estimatedAfterTokens === "number"
          ? parsed.estimatedAfterTokens
          : typeof parsed.afterTokens === "number"
            ? parsed.afterTokens
            : 0;
      return {
        compressedMessageCount: parsed.compressedMessageCount,
        keptMessageCount: parsed.keptMessageCount,
        estimatedBeforeTokens: estimatedBefore,
        estimatedAfterTokens: estimatedAfter,
        force: Boolean(parsed.force),
      };
    } catch {
      return null;
    }
  }
  return null;
}

/**
 * Compress oldest non-pinned messages when estimated tokens cross the
 * model-specific threshold (or when `force` is set after a context 400).
 */
export async function compressMessagesIfNeeded(
  messages: Message[],
  options: CompressMessagesOptions,
): Promise<CompressMessagesResult> {
  const maxOutput = options.maxOutputTokens ?? DEFAULT_MAX_OUTPUT_TOKENS;
  const base = compressionBudgetsForModel(options.model, maxOutput);
  const budgets: CompressionBudgets = { ...base, ...options.budgets };
  const estimatedBeforeTokens = estimateTokens(messages);
  const lastProvider = options.lastProviderPromptTokens;
  const providerNearLimit =
    typeof lastProvider === "number" && lastProvider >= budgets.thresholdTokens;

  const indices = selectCompressIndices(
    messages,
    budgets.thresholdTokens,
    budgets.targetTokens,
    { force: Boolean(options.force || providerNearLimit) },
  );
  if (!indices) {
    return {
      messages,
      compressed: false,
      estimatedBeforeTokens,
      estimatedAfterTokens: estimatedBeforeTokens,
      compressedCount: 0,
      keptCount: messages.length,
      meta: null,
    };
  }

  const prompt = buildSummaryPrompt(messages, indices);
  let summaryText = "";
  try {
    summaryText = (await options.summarize(prompt)).trim();
  } catch (err) {
    console.warn(
      "[exo-worker] context compression summarizer failed:",
      err instanceof Error ? err.message : err,
    );
    return {
      messages,
      compressed: false,
      estimatedBeforeTokens,
      estimatedAfterTokens: estimatedBeforeTokens,
      compressedCount: 0,
      keptCount: messages.length,
      meta: null,
    };
  }
  if (!summaryText) {
    console.warn(
      "[exo-worker] context compression produced empty summary; skipping",
    );
    return {
      messages,
      compressed: false,
      estimatedBeforeTokens,
      estimatedAfterTokens: estimatedBeforeTokens,
      compressedCount: 0,
      keptCount: messages.length,
      meta: null,
    };
  }

  const keptCount = messages.length - indices.length + 1;
  const draft = rebuildWithCompression(messages, indices, summaryText);
  const estimatedAfterTokens = estimateTokens(draft);
  const meta: CompressionMeta = {
    compressedMessageCount: indices.length,
    keptMessageCount: keptCount,
    estimatedBeforeTokens,
    estimatedAfterTokens,
    force: Boolean(options.force || providerNearLimit),
  };
  const messagesWithMeta = rebuildWithCompression(
    messages,
    indices,
    summaryText,
    meta,
  );
  const marker = compressionMarkerMessage(summaryText, meta);

  if (options.persistMarker) {
    try {
      await options.persistMarker(marker);
    } catch (err) {
      console.warn(
        "[exo-worker] failed to persist compression marker (continuing in-memory):",
        err instanceof Error ? err.message : err,
      );
    }
  }

  console.warn(
    `[exo-worker] compressed ${indices.length} oldest messages; ` +
      `est ~${estimatedBeforeTokens}→~${estimatedAfterTokens} tokens ` +
      `(window=${budgets.contextWindowTokens}, threshold=${budgets.thresholdTokens}, ` +
      `target=${budgets.targetTokens}` +
      `${options.force ? ", force" : ""}` +
      `${providerNearLimit ? ", provider-near-limit" : ""})`,
  );

  return {
    messages: messagesWithMeta,
    compressed: true,
    estimatedBeforeTokens,
    estimatedAfterTokens,
    compressedCount: indices.length,
    keptCount,
    meta,
  };
}
