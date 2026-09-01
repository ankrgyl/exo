import {
  toolResultMessage,
  type Conversation,
  type Event,
  type EventData,
  type JsonObject,
  type Message,
  type ToolResult,
} from "@exo/harness";

import { applyCompressionMarkerWindow } from "./context-compress.js";

/**
 * ExoWorker-local conversation materialization.
 *
 * exo's event log often splits one model turn into multiple consecutive
 * assistant messages (text, then one message per tool_call). The Chat API
 * expects a single assistant message with all tool_calls, followed by tool
 * messages. We coalesce, then repair missing/orphan pairings before each call.
 *
 * Large tool results (screenshots / slide previews) are stored as conversation
 * artifacts. Vision policy:
 * - Trailing tool-result group (not yet followed by an assistant reply):
 *   rehydrate once, attach real image parts, strip all base64 from the JSON.
 * - Older history: never rehydrate artifacts; strip every base64 payload so the
 *   model does not re-read the same image bytes on later rounds.
 *
 * After vision strip/hydrate, tool-result JSON is size-capped so a single
 * runaway dump (shell stdout, HTML, rehydrated artifact) cannot poison the
 * prompt. Trailing (latest) results get a higher budget; history is tighter.
 */

const IMAGE_BASE64_KEYS = new Set([
  "screenshotBase64",
  "imageBase64",
  "base64",
]);

/** Strings longer than this that look like base64 are dropped from prompts. */
const BASE64_STRIP_MIN_CHARS = 256;

/**
 * Char budget for tool-result JSON still awaiting an assistant reply.
 * ~8–10K tokens worst case; enough for real shell/logs, not multi‑MB dumps.
 */
export const LATEST_TOOL_RESULT_CHAR_CAP = 32_000;

/** Char budget for tool-result JSON the model has already seen. */
export const HISTORY_TOOL_RESULT_CHAR_CAP = 4_000;

/** Max vision image attaches per trailing tool round. */
export const MAX_VISION_IMAGES_PER_ROUND = 1;

/**
 * Max decoded image bytes (~120KB) for a single vision attach. Larger PNG
 * slide previews were counting as ~200K+ tokens each toward provider limits.
 */
export const MAX_VISION_IMAGE_BYTES = 120_000;

export type VisionImageCandidate = {
  toolName: string;
  base64: string;
  mediaType: string;
  label?: string;
};

export type VisionImageDecision =
  | { action: "attach"; image: VisionImageCandidate }
  | { action: "omit"; reason: string; toolName: string; label?: string };

export async function materializeExoWorkerPromptMessages(
  conversation: Conversation,
  instructions: Message[],
): Promise<Message[]> {
  const history = await materializeExoWorkerConversationMessages(conversation);
  const withVision = await hydrateToolResultsForVision(conversation, history);
  // Reasoning models emit separate assistant rows for reasoning vs tool_calls.
  // Coalescing those into one message makes Lingua reject the request
  // ("Mixed reasoning and other content parts"). Drop reasoning on replay —
  // encrypted_content cannot be reused with store:false anyway.
  //
  // If a prior round persisted [COMPRESSED PRIOR WORK], drop older history
  // before that marker (client request is kept — see applyCompressionMarkerWindow).
  const historyForPrompt = applyCompressionMarkerWindow(
    repairLinguaToolPairing(stripReasoningParts(withVision)),
  );
  return [...instructions, ...historyForPrompt];
}

/** @internal exported for tests */
export function stripReasoningParts(messages: Message[]): Message[] {
  const out: Message[] = [];
  for (const msg of messages) {
    if (msg.role !== "assistant" || !Array.isArray(msg.content)) {
      out.push(msg);
      continue;
    }
    const content = msg.content.filter(
      (part) =>
        !(
          part &&
          typeof part === "object" &&
          (part as { type?: string }).type === "reasoning"
        ),
    );
    if (content.length === 0) {
      continue;
    }
    out.push({ ...msg, content });
  }
  return out;
}

/**
 * Lingua converts one assistant message into one Responses item. ExoWorker's
 * Chat-compatible materialization coalesces parallel tool calls into one
 * assistant message, so split that message back into one item per call before
 * sending history to the Responses API.
 *
 * @internal exported for tests
 */
export function splitAssistantToolCallsForResponses(
  messages: Message[],
): Message[] {
  return messages.flatMap((message): Message[] => {
    if (message.role !== "assistant" || !Array.isArray(message.content)) {
      return [message];
    }

    const toolCalls = message.content.filter(isToolCallPart);
    if (toolCalls.length === 0) {
      return [message];
    }

    const otherParts = message.content.filter((part) => !isToolCallPart(part));
    if (toolCalls.length === 1 && otherParts.length === 0) {
      return [message];
    }

    const { id: _id, ...messageWithoutId } = message;
    return [
      ...(otherParts.length > 0
        ? [{ ...messageWithoutId, content: otherParts } as Message]
        : []),
      ...toolCalls.map(
        (toolCall) => ({ ...messageWithoutId, content: [toolCall] }) as Message,
      ),
    ];
  });
}

function isToolCallPart(part: unknown): boolean {
  return (
    Boolean(part) &&
    typeof part === "object" &&
    (part as { type?: string }).type === "tool_call"
  );
}

export async function materializeExoWorkerConversationMessages(
  conversation: Conversation,
): Promise<Message[]> {
  const result = await conversation.getEvents({
    direction: "asc",
    types: ["messages", "tool_requested", "tool_result"],
  });
  return materializeExoWorkerEventsToMessages(result.events);
}

/**
 * Prepare tool results for the next model call.
 *
 * Fresh trailing tool results: rehydrate + attach images once, then cap at
 * {@link LATEST_TOOL_RESULT_CHAR_CAP}.
 * Historical tool results: strip base64 (no artifact re-read), then cap at
 * {@link HISTORY_TOOL_RESULT_CHAR_CAP}.
 */
export async function hydrateToolResultsForVision(
  conversation: Conversation,
  messages: Message[],
): Promise<Message[]> {
  const out: Message[] = [];
  const pendingImages: VisionImageCandidate[] = [];
  const latestToolIndices = findTrailingToolMessageIndices(messages);

  const flushImages = () => {
    if (pendingImages.length === 0) return;
    const decisions = budgetVisionImages(pendingImages);
    for (const decision of decisions) {
      if (decision.action === "attach") {
        const img = decision.image;
        const label = img.label ? ` (${img.label})` : "";
        out.push({
          role: "user",
          content: [
            {
              type: "text",
              text:
                `Visual result of the previous \`${img.toolName}\` tool call${label}. ` +
                `Inspect this image carefully — do not claim you can see it unless you use this attachment.`,
            },
            {
              type: "image",
              image: img.base64,
              media_type: img.mediaType,
            },
          ],
        });
        continue;
      }
      const label = decision.label ? ` (${decision.label})` : "";
      out.push({
        role: "user",
        content: [
          {
            type: "text",
            text:
              `Visual result of \`${decision.toolName}\`${label} was not attached: ${decision.reason}. ` +
              `Re-run the tool with a smaller capture if you need to inspect it.`,
          },
        ],
      });
      console.warn(
        `[exo-worker] skipped vision attach tool=${decision.toolName}` +
          `${label}: ${decision.reason}`,
      );
    }
    pendingImages.length = 0;
  };

  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i]!;
    if (msg.role !== "tool" || !Array.isArray(msg.content)) {
      flushImages();
      out.push(msg);
      continue;
    }

    const isLatestRound = latestToolIndices.has(i);
    const newContent: unknown[] = [];
    for (const part of msg.content) {
      if (!part || typeof part !== "object") {
        newContent.push(part);
        continue;
      }
      const p = part as {
        type?: string;
        tool_call_id?: string;
        tool_name?: string;
        output?: unknown;
      };
      if (p.type !== "tool_result") {
        newContent.push(part);
        continue;
      }

      const toolName =
        typeof p.tool_name === "string" && p.tool_name.trim()
          ? p.tool_name
          : "tool";

      if (isLatestRound) {
        const full = await resolveFullToolOutput(conversation, p.output);
        const images = extractAllImagesFromToolOutput(full);
        const slim = stripAllImagePayloads(full, {
          omittedNote:
            images.length > 0
              ? "Image(s) attached as the next user message(s) for vision."
              : undefined,
        });
        newContent.push({
          ...p,
          output: capToolOutputForPrompt(slim, LATEST_TOOL_RESULT_CHAR_CAP, {
            keepHead: 24_000,
            keepTail: 4_000,
          }),
        });
        for (const image of images) {
          pendingImages.push({
            toolName,
            base64: image.base64,
            mediaType: image.mediaType,
            label: image.label,
          });
        }
      } else {
        // Historical: never rehydrate full artifacts — strip whatever is inline.
        newContent.push({
          ...p,
          output: capToolOutputForPrompt(
            stripToolOutputForHistory(p.output),
            HISTORY_TOOL_RESULT_CHAR_CAP,
            { keepHead: 3_000, keepTail: 500 },
          ),
        });
      }
    }

    out.push({ role: "tool", content: newContent as Message["content"] });
  }

  flushImages();
  return out;
}

/**
 * Bound tool-result JSON for the prompt. Under the cap the original value is
 * returned unchanged; over the cap we keep head + tail of the serialized form
 * so the model still sees the start and end of large dumps.
 */
export function capToolOutputForPrompt(
  output: unknown,
  maxChars: number,
  opts: { keepHead?: number; keepTail?: number } = {},
): unknown {
  const serialized = safeSerializeToolOutput(output);
  if (serialized.length <= maxChars) return output;

  const noteOverhead = 80;
  let keepHead = opts.keepHead ?? Math.floor(maxChars * 0.75);
  let keepTail = opts.keepTail ?? Math.floor(maxChars * 0.125);
  if (keepHead + keepTail + noteOverhead > maxChars) {
    keepTail = Math.min(keepTail, Math.floor(maxChars * 0.125));
    keepHead = Math.max(0, maxChars - keepTail - noteOverhead);
  }

  const omitted = Math.max(0, serialized.length - keepHead - keepTail);
  const head = serialized.slice(0, keepHead);
  const tail = keepTail > 0 ? serialized.slice(-keepTail) : "";
  const preview =
    keepTail > 0
      ? `${head}\n…[${omitted} chars truncated]…\n${tail}`
      : `${head}\n…[${omitted} chars truncated]…`;

  return {
    truncated: true,
    originalChars: serialized.length,
    preview,
    note: `Tool result truncated from ${serialized.length} chars (cap ${maxChars}).`,
  };
}

function safeSerializeToolOutput(output: unknown): string {
  if (typeof output === "string") return output;
  try {
    return JSON.stringify(output) ?? "";
  } catch {
    return String(output);
  }
}

/**
 * Apply per-round vision budget: max image count + max decoded bytes each.
 * Oversized / excess images become text-only notes (no downscale library).
 */
export function budgetVisionImages(
  images: VisionImageCandidate[],
  opts: {
    maxImages?: number;
    maxBytes?: number;
  } = {},
): VisionImageDecision[] {
  const maxImages = opts.maxImages ?? MAX_VISION_IMAGES_PER_ROUND;
  const maxBytes = opts.maxBytes ?? MAX_VISION_IMAGE_BYTES;
  const decisions: VisionImageDecision[] = [];
  let attached = 0;

  for (const image of images) {
    if (attached >= maxImages) {
      decisions.push({
        action: "omit",
        toolName: image.toolName,
        label: image.label,
        reason: `round vision budget exceeded (max ${maxImages} images)`,
      });
      continue;
    }
    const bytes = estimateDecodedImageBytes(image.base64);
    if (bytes > maxBytes) {
      decisions.push({
        action: "omit",
        toolName: image.toolName,
        label: image.label,
        reason: `image too large (~${Math.round(bytes / 1024)}KB > max ${Math.round(maxBytes / 1024)}KB)`,
      });
      continue;
    }
    decisions.push({ action: "attach", image });
    attached += 1;
  }
  return decisions;
}

/** Approximate decoded byte size from a base64 (or data-URL) payload. */
export function estimateDecodedImageBytes(base64OrDataUrl: string): number {
  let b64 = base64OrDataUrl.trim();
  if (b64.startsWith("data:")) {
    const comma = b64.indexOf(",");
    if (comma >= 0) b64 = b64.slice(comma + 1);
  }
  b64 = b64.replace(/\s+/g, "");
  const padding = b64.endsWith("==") ? 2 : b64.endsWith("=") ? 1 : 0;
  return Math.max(0, Math.floor((b64.length * 3) / 4) - padding);
}

/**
 * Tool messages at the end of history (after skipping trailing user nudges)
 * are the only ones that still need vision — the model has not replied yet.
 */
export function findTrailingToolMessageIndices(
  messages: Message[],
): Set<number> {
  const indices = new Set<number>();
  let i = messages.length - 1;
  while (i >= 0 && messages[i]!.role === "user") {
    i -= 1;
  }
  while (i >= 0 && messages[i]!.role === "tool") {
    indices.add(i);
    i -= 1;
  }
  return indices;
}

async function resolveFullToolOutput(
  conversation: Conversation,
  output: unknown,
): Promise<unknown> {
  if (!isRecord(output)) return output;

  // Compact exo envelope: prefer artifact, then inline value.
  const artifact = output.resultArtifact;
  if (isRecord(artifact) && typeof artifact.artifactId === "string") {
    try {
      const text = await conversation.readArtifactText({
        artifactId: artifact.artifactId,
        version:
          typeof artifact.version === "number" ? artifact.version : undefined,
      });
      if (text?.trim()) {
        return JSON.parse(text) as unknown;
      }
    } catch (err) {
      console.warn(
        "[exo-worker] failed to rehydrate tool result artifact:",
        err instanceof Error ? err.message : err,
      );
    }
  }

  if (output.value != null) return output.value;
  return output;
}

/** Historical tool output: keep metadata, drop artifact rehydration + all base64. */
export function stripToolOutputForHistory(output: unknown): unknown {
  if (!isRecord(output)) {
    return stripAllImagePayloads(output);
  }

  // Compact envelope from exo tool runner.
  if (
    output.resultArtifact != null ||
    output.value != null ||
    typeof output.preview === "string"
  ) {
    const value =
      output.value != null
        ? stripAllImagePayloads(output.value, {
            omittedNote: "Image omitted (already shown in an earlier turn).",
          })
        : null;
    const preview =
      typeof output.preview === "string"
        ? stripBase64FromString(output.preview)
        : undefined;
    return {
      ok: output.ok,
      toolName: output.toolName,
      toolCallId: output.toolCallId,
      source: output.source,
      truncated: true,
      preview,
      value,
      imageOmitted: true,
      note: "Image payload omitted from history — already shown once to the model.",
    };
  }

  return stripAllImagePayloads(output, {
    omittedNote: "Image omitted (already shown in an earlier turn).",
  });
}

export type ExtractedToolImage = {
  base64: string;
  mediaType: string;
  label?: string;
};

/** Collect every image payload (top-level or nested, e.g. slides[].imageBase64). */
export function extractAllImagesFromToolOutput(
  output: unknown,
): ExtractedToolImage[] {
  const found: ExtractedToolImage[] = [];
  walkForImages(output, found, "");
  return found;
}

function walkForImages(
  value: unknown,
  found: ExtractedToolImage[],
  path: string,
): void {
  if (Array.isArray(value)) {
    value.forEach((item, idx) => walkForImages(item, found, `${path}[${idx}]`));
    return;
  }
  if (!isRecord(value)) return;

  for (const [key, child] of Object.entries(value)) {
    if (IMAGE_BASE64_KEYS.has(key) && typeof child === "string") {
      const parsed = parseBase64Image(child);
      if (parsed) {
        const slideNum =
          typeof value.slideNumber === "number"
            ? `slide ${value.slideNumber}`
            : typeof value.slide === "string"
              ? value.slide
              : undefined;
        found.push({
          ...parsed,
          label: slideNum ?? (path ? path : undefined),
        });
      }
      continue;
    }
    walkForImages(child, found, path ? `${path}.${key}` : key);
  }
}

/**
 * Recursively delete image base64 fields and base64-looking strings.
 * Does not truncate — removes the payload entirely.
 */
export function stripAllImagePayloads(
  value: unknown,
  opts?: { omittedNote?: string },
): unknown {
  if (typeof value === "string") {
    return looksLikeBase64Payload(value) ? undefined : value;
  }
  if (Array.isArray(value)) {
    return value.map((item) => stripAllImagePayloads(item, opts));
  }
  if (!isRecord(value)) return value;

  const out: Record<string, unknown> = {};
  let omitted = false;
  for (const [key, child] of Object.entries(value)) {
    // Host-only vision batches (previewPresentation) — never keep the array
    // skeleton in the prompt after images were extracted.
    if (key === "_visionSlides" || key === "_previewFiles") {
      omitted = true;
      out[`${key}Omitted`] = true;
      if (Array.isArray(child)) out[`${key}Count`] = child.length;
      continue;
    }
    if (IMAGE_BASE64_KEYS.has(key) && typeof child === "string") {
      omitted = true;
      out[`${key}Omitted`] = true;
      continue;
    }
    if (typeof child === "string" && looksLikeBase64Payload(child)) {
      omitted = true;
      continue;
    }
    const next = stripAllImagePayloads(child, opts);
    if (next !== undefined) {
      out[key] = next;
    }
  }
  if (omitted) {
    out.imageOmitted = true;
    if (opts?.omittedNote && typeof out.note !== "string") {
      out.note = opts.omittedNote;
    }
  }
  return out;
}

function stripBase64FromString(text: string): string {
  if (!looksLikeBase64Payload(text)) return text;
  return "[image base64 omitted]";
}

function parseBase64Image(
  raw: string,
): { base64: string; mediaType: string } | null {
  if (!raw || raw.length < 32) return null;
  if (raw.includes("[truncated]") || raw.includes("[content truncated")) {
    return null;
  }
  let base64 = raw.trim();
  let mediaType = "image/jpeg";
  if (base64.startsWith("data:")) {
    const match = /^data:([^;]+);base64,/i.exec(base64);
    if (!match) return null;
    mediaType = match[1] || mediaType;
    base64 = base64.slice(match[0].length);
  }
  base64 = base64.replace(/\s+/g, "");
  if (base64.length < 32 || !/^[A-Za-z0-9+/]*={0,2}$/.test(base64)) {
    return null;
  }
  if (base64.startsWith("iVBOR")) mediaType = "image/png";
  return { base64, mediaType };
}

export function looksLikeBase64Payload(value: string): boolean {
  if (value.length < BASE64_STRIP_MIN_CHARS) return false;
  if (value.startsWith("data:") && value.includes("base64,")) return true;
  const sample = value.slice(0, 512).replace(/\s+/g, "");
  return (
    sample.length >= BASE64_STRIP_MIN_CHARS && /^[A-Za-z0-9+/]+=*$/.test(sample)
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

export function materializeExoWorkerEventsToMessages(
  events: Event[],
): Message[] {
  const messages: Message[] = [];
  const toolCallNames = new Map<string, string>();
  const pendingToolCallIds: string[] = [];

  for (const event of events) {
    extendExoWorkerMaterializedMessages(
      messages,
      toolCallNames,
      pendingToolCallIds,
      event,
    );
  }
  flushDanglingToolResults(messages, toolCallNames, pendingToolCallIds);
  return messages;
}

/** @internal exported for tests */
export function repairLinguaToolPairing(messages: Message[]): Message[] {
  return normalizeToolRoundPairing(
    coalesceConsecutiveAssistantMessages(messages),
  );
}

/**
 * Anthropic/OpenAI-compatible APIs emit parallel tool calls as separate
 * assistant rows in exo's messages events. Merge them before tool results.
 */
function coalesceConsecutiveAssistantMessages(messages: Message[]): Message[] {
  const out: Message[] = [];

  for (const msg of messages) {
    const last = out[out.length - 1];
    if (msg.role === "assistant" && last?.role === "assistant") {
      last.content = mergeMessageContent(last.content, msg.content);
      if (msg.id) {
        last.id = msg.id;
      }
      continue;
    }
    out.push({ ...msg });
  }

  return out;
}

function normalizeToolRoundPairing(messages: Message[]): Message[] {
  const out: Message[] = [];
  let synthesized = 0;
  let dropped = 0;

  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i]!;
    if (msg.role === "tool") {
      continue;
    }

    out.push(msg);

    const calls = toolCallsFromAssistant(msg);
    if (calls.length === 0) {
      continue;
    }

    const callIds = new Set(calls.map((call) => call.id));
    const needed = new Map(calls.map((call) => [call.id, call.name]));

    let j = i + 1;
    while (j < messages.length && messages[j]!.role === "tool") {
      const toolMsg = messages[j]!;
      const keptContent = filterToolResultContent(toolMsg, callIds, () => {
        dropped++;
      });
      if (keptContent.length > 0) {
        out.push({ role: "tool", content: keptContent });
      }
      for (const id of toolResultIdsFromParts(keptContent)) {
        needed.delete(id);
      }
      j++;
    }

    for (const [id, name] of needed) {
      out.push(
        toolResultMessage(id, name, {
          ok: false,
          error: "tool result missing from event log; synthesized by ExoWorker",
        }),
      );
      synthesized++;
    }

    // Preserve vision user messages that hydrateToolResultsForVision inserted
    // immediately after a tool-result group (must stay after all tool results).
    while (
      j < messages.length &&
      messages[j]!.role === "user" &&
      isScreenshotVisionFollowUp(messages[j]!)
    ) {
      out.push(messages[j]!);
      j++;
    }
    i = j - 1;
  }

  if (synthesized > 0 || dropped > 0) {
    console.warn(
      `[exo-worker] repaired tool pairing: synthesized=${synthesized} dropped_orphans=${dropped}`,
    );
  }

  return out;
}

function isScreenshotVisionFollowUp(message: Message): boolean {
  if (message.role !== "user" || !Array.isArray(message.content)) return false;
  return message.content.some(
    (part) =>
      part &&
      typeof part === "object" &&
      (part as { type?: string }).type === "image",
  );
}

function filterToolResultContent(
  toolMsg: Message,
  allowedCallIds: Set<string>,
  onDrop: () => void,
): unknown[] {
  if (!Array.isArray(toolMsg.content)) {
    return [];
  }
  const kept: unknown[] = [];
  for (const part of toolMsg.content) {
    if (!part || typeof part !== "object") {
      kept.push(part);
      continue;
    }
    const p = part as { type?: string; tool_call_id?: string };
    if (p.type === "tool_result") {
      if (
        typeof p.tool_call_id === "string" &&
        allowedCallIds.has(p.tool_call_id)
      ) {
        kept.push(part);
      } else {
        onDrop();
      }
      continue;
    }
    kept.push(part);
  }
  return kept;
}

function mergeMessageContent(existing: unknown, incoming: unknown): unknown[] {
  return [...contentParts(existing), ...contentParts(incoming)];
}

function contentParts(content: unknown): unknown[] {
  if (Array.isArray(content)) {
    return [...content];
  }
  if (typeof content === "string" && content.length > 0) {
    return [{ type: "text", text: content }];
  }
  return [];
}

function toolResultIdsFromParts(content: unknown[]): string[] {
  const ids: string[] = [];
  for (const part of content) {
    if (!part || typeof part !== "object") {
      continue;
    }
    const p = part as { type?: string; tool_call_id?: string };
    if (p.type === "tool_result" && typeof p.tool_call_id === "string") {
      ids.push(p.tool_call_id);
    }
  }
  return ids;
}

function extendExoWorkerMaterializedMessages(
  messages: Message[],
  toolCallNames: Map<string, string>,
  pendingToolCallIds: string[],
  event: Event,
): void {
  if (isMessagesEvent(event.data)) {
    flushDanglingToolResults(messages, toolCallNames, pendingToolCallIds);
    for (const message of event.data.messages) {
      registerToolCallsInMessage(message, toolCallNames, pendingToolCallIds);
    }
    messages.push(...event.data.messages);
    return;
  }

  if (isToolRequestedEvent(event.data)) {
    toolCallNames.set(
      event.data.tool_call_id,
      event.data.request.function_name,
    );
    pushPendingToolCall(pendingToolCallIds, event.data.tool_call_id);
    return;
  }

  if (isToolResultEvent(event.data)) {
    const toolName =
      toolCallNames.get(event.data.tool_call_id) ??
      findToolNameInMessages(messages, event.data.tool_call_id) ??
      "unknown";
    toolCallNames.set(event.data.tool_call_id, toolName);
    removePendingToolCall(pendingToolCallIds, event.data.tool_call_id);
    messages.push(
      toolResultMessage(event.data.tool_call_id, toolName, event.data.result),
    );
  }
}

function registerToolCallsInMessage(
  message: Message,
  toolCallNames: Map<string, string>,
  pendingToolCallIds: string[],
): void {
  for (const call of toolCallsFromAssistant(message)) {
    toolCallNames.set(call.id, call.name);
    pushPendingToolCall(pendingToolCallIds, call.id);
  }
}

function toolCallsFromAssistant(
  message: Message,
): Array<{ id: string; name: string }> {
  if (message.role !== "assistant" || !Array.isArray(message.content)) {
    return [];
  }
  const calls: Array<{ id: string; name: string }> = [];
  for (const part of message.content) {
    if (!part || typeof part !== "object") {
      continue;
    }
    const p = part as {
      type?: string;
      tool_call_id?: string;
      tool_name?: string;
    };
    if (
      p.type === "tool_call" &&
      typeof p.tool_call_id === "string" &&
      typeof p.tool_name === "string"
    ) {
      calls.push({ id: p.tool_call_id, name: p.tool_name });
    }
  }
  return calls;
}

function findToolNameInMessages(
  messages: Message[],
  toolCallId: string,
): string | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    for (const call of toolCallsFromAssistant(messages[i]!)) {
      if (call.id === toolCallId) {
        return call.name;
      }
    }
  }
  return null;
}

function flushDanglingToolResults(
  messages: Message[],
  toolCallNames: Map<string, string>,
  pendingToolCallIds: string[],
): void {
  while (pendingToolCallIds.length > 0) {
    const toolCallId = pendingToolCallIds.shift();
    if (!toolCallId) {
      continue;
    }
    const toolName = toolCallNames.get(toolCallId);
    if (!toolName) {
      continue;
    }
    messages.push(
      toolResultMessage(toolCallId, toolName, {
        ok: false,
        error: "tool execution did not complete before the previous turn ended",
      } satisfies ToolResult),
    );
  }
}

function pushPendingToolCall(pendingToolCallIds: string[], toolCallId: string) {
  if (!pendingToolCallIds.includes(toolCallId)) {
    pendingToolCallIds.push(toolCallId);
  }
}

function removePendingToolCall(
  pendingToolCallIds: string[],
  toolCallId: string,
): void {
  const index = pendingToolCallIds.indexOf(toolCallId);
  if (index >= 0) {
    pendingToolCallIds.splice(index, 1);
  }
}

function isMessagesEvent(
  data: EventData,
): data is EventData & { type: "messages"; messages: Message[] } {
  return data.type === "messages" && Array.isArray(data.messages);
}

function isToolRequestedEvent(data: EventData): data is EventData & {
  type: "tool_requested";
  tool_call_id: string;
  request: { function_name: string; arguments: JsonObject };
} {
  if (data.type !== "tool_requested") {
    return false;
  }
  if (typeof data.tool_call_id !== "string") {
    return false;
  }
  if (!data.request || typeof data.request !== "object") {
    return false;
  }
  return (
    typeof (data.request as { function_name?: unknown }).function_name ===
    "string"
  );
}

function isToolResultEvent(data: EventData): data is EventData & {
  type: "tool_result";
  tool_call_id: string;
  result: ToolResult;
} {
  return data.type === "tool_result" && typeof data.tool_call_id === "string";
}
