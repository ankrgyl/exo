/** Custom event type for provider prompt usage (host may mirror to its UI). */
export const PROMPT_USAGE_EVENT_TYPE = "exo_worker.prompt_usage";
/**
 * Billable usage for LLM calls that do not emit a `messages` usage record
 * (context-compression summarizer). Host must debit these or xAI spend is lost.
 */
export const PROVIDER_USAGE_EVENT_TYPE = "exo_worker.provider_usage";

export type ProviderUsage = {
  promptTokens: number;
  completionTokens?: number;
  cachedTokens?: number;
  reasoningTokens?: number;
};

function finiteToken(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) && value >= 0
    ? value
    : undefined;
}

export function extractProviderUsage(response: {
  usage?: {
    input_tokens?: number | null;
    output_tokens?: number | null;
    prompt_tokens?: number | null;
    completion_tokens?: number | null;
    input_tokens_details?: { cached_tokens?: number | null };
    prompt_tokens_details?: { cached_tokens?: number | null };
    output_tokens_details?: { reasoning_tokens?: number | null };
    completion_tokens_details?: { reasoning_tokens?: number | null };
  } | null;
}): ProviderUsage | null {
  const usage = response.usage;
  const input =
    finiteToken(usage?.input_tokens) ?? finiteToken(usage?.prompt_tokens);
  if (input == null) return null;
  const output =
    finiteToken(usage?.output_tokens) ?? finiteToken(usage?.completion_tokens);
  const cached =
    finiteToken(usage?.input_tokens_details?.cached_tokens) ??
    finiteToken(usage?.prompt_tokens_details?.cached_tokens);
  const reasoning =
    finiteToken(usage?.output_tokens_details?.reasoning_tokens) ??
    finiteToken(usage?.completion_tokens_details?.reasoning_tokens);
  return {
    promptTokens: input,
    completionTokens: output,
    cachedTokens: cached,
    reasoningTokens: reasoning,
  };
}

export function promptUsageEvent(
  usage: ProviderUsage,
  model: string,
): {
  type: "custom";
  event_type: typeof PROMPT_USAGE_EVENT_TYPE;
  payload: {
    promptTokens: number;
    completionTokens?: number;
    model: string;
  };
} {
  return {
    type: "custom",
    event_type: PROMPT_USAGE_EVENT_TYPE,
    payload: {
      promptTokens: usage.promptTokens,
      completionTokens: usage.completionTokens,
      model,
    },
  };
}

export function providerUsageEvent(
  usage: ProviderUsage,
  model: string,
  source: "compression",
): {
  type: "custom";
  event_type: typeof PROVIDER_USAGE_EVENT_TYPE;
  payload: {
    model: string;
    source: "compression";
    prompt_tokens: number;
    completion_tokens: number;
    prompt_cached_tokens: number;
    completion_reasoning_tokens: number;
  };
} {
  return {
    type: "custom",
    event_type: PROVIDER_USAGE_EVENT_TYPE,
    payload: {
      model,
      source,
      prompt_tokens: usage.promptTokens,
      completion_tokens: usage.completionTokens ?? 0,
      prompt_cached_tokens: usage.cachedTokens ?? 0,
      completion_reasoning_tokens: usage.reasoningTokens ?? 0,
    },
  };
}
