// Post-training family of a model, used to pick the edit-tool shape. Keys off
// the upstream model id, not the arbitrary local binding name.
export type ModelFamily = "anthropic" | "openai" | "unknown";

// Exact token membership, not substring: "claudette-7b" is not Claude.
const ANTHROPIC_TOKENS = new Set(["claude", "anthropic"]);
const OPENAI_TOKENS = new Set(["gpt", "codex", "openai"]);

function tokens(modelId: string): string[] {
  return modelId
    .split("/")
    .flatMap((part) => part.split("."))
    .flatMap((part) => part.split("-"))
    .filter((part) => part.length > 0);
}

// Router aliases ("auto") and self-hosted models have no static family;
// "unknown" is a real state, not an error.
export function modelFamily(upstreamModel: string): ModelFamily {
  const parts = tokens(upstreamModel.trim().toLowerCase());
  if (parts.some((part) => ANTHROPIC_TOKENS.has(part))) {
    return "anthropic";
  }
  if (parts.some((part) => OPENAI_TOKENS.has(part))) {
    return "openai";
  }
  return "unknown";
}
