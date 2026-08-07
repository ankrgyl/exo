// Cost policy for the TypeScript harness: a self-contained port of the `cost`
// crate's math and loader. It owns its own price data (env override, on-disk
// cache, or its own fetch) and does not depend on the Rust loader having run.

import { mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

export type ModelEntry = {
  litellm_provider?: string;
  input_cost_per_token?: number;
  output_cost_per_token?: number;
  cache_read_input_token_cost?: number;
  cache_creation_input_token_cost?: number;
};

export type PricingTable = Map<string, ModelEntry>;

export type TokenCounts = {
  prompt?: number;
  completion?: number;
  cached?: number;
  cacheCreation?: number;
};

export function parseTable(json: string): PricingTable {
  const raw = JSON.parse(json) as Record<string, unknown>;
  const table: PricingTable = new Map();
  for (const [key, value] of Object.entries(raw)) {
    if (key !== "sample_spec" && value !== null && typeof value === "object") {
      table.set(key, value as ModelEntry);
    }
  }
  return table;
}

// Exact match, else the longest key that is a prefix of `model` at a token
// boundary (next char absent or `-`/`:`).
export function lookup(
  table: PricingTable,
  model: string,
): ModelEntry | undefined {
  const exact = table.get(model);
  if (exact) return exact;
  let best: ModelEntry | undefined;
  let bestLen = -1;
  for (const [key, entry] of table) {
    const next = model[key.length];
    if (
      key.length > bestLen &&
      model.startsWith(key) &&
      (next === undefined || next === "-" || next === ":")
    ) {
      best = entry;
      bestLen = key.length;
    }
  }
  return best;
}

export function computeCostUsd(
  table: PricingTable,
  model: string,
  tokens: TokenCounts,
): number | null {
  const entry = lookup(table, model);
  if (!entry || entry.input_cost_per_token == null) return null;
  const input = entry.input_cost_per_token;
  const output = entry.output_cost_per_token ?? 0;
  const cacheRead = entry.cache_read_input_token_cost ?? input;
  const cacheWrite = entry.cache_creation_input_token_cost ?? input;

  const prompt = Math.max(0, tokens.prompt ?? 0);
  const completion = Math.max(0, tokens.completion ?? 0);
  const cached = Math.max(0, tokens.cached ?? 0);
  const created = Math.max(0, tokens.cacheCreation ?? 0);

  // Anthropic-family `prompt_tokens` excludes cached (additive); else inclusive.
  const fresh = isAdditive(entry.litellm_provider)
    ? prompt
    : Math.max(0, prompt - cached);
  return (
    fresh * input +
    cached * cacheRead +
    created * cacheWrite +
    completion * output
  );
}

function isAdditive(provider?: string): boolean {
  return (
    provider != null &&
    (provider.startsWith("anthropic") ||
      provider.startsWith("vertex_ai-anthropic") ||
      provider === "azure_ai")
  );
}

const DEFAULT_URL =
  "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const CACHE_TTL_MS = 24 * 60 * 60 * 1000;
const FETCH_TIMEOUT_MS = 5000;

let table: PricingTable | null | undefined;
let loading: Promise<PricingTable | null> | undefined;

// The in-memory table once loaded; null until ensureTable() has resolved.
export function getTable(): PricingTable | null {
  return table ?? null;
}

// Loads the table once (memoized), owning its own data: env override -> fresh
// cache -> own fetch (cached) -> stale cache -> null. Independent of Rust.
export function ensureTable(): Promise<PricingTable | null> {
  loading ??= load().then((loaded) => (table = loaded));
  return loading;
}

async function load(): Promise<PricingTable | null> {
  const override = process.env.EXO_LITELLM_PRICES_PATH;
  if (override) return readTable(override);

  const cache = cachePath();
  if (cache && isFresh(cache)) {
    const fresh = readTable(cache);
    if (fresh) return fresh;
  }
  const fetched = await fetchTable(
    process.env.EXO_LITELLM_PRICES_URL ?? DEFAULT_URL,
  );
  if (fetched) {
    if (cache) writeCache(cache, fetched.body);
    return fetched.table;
  }
  return cache ? readTable(cache) : null; // stale cache fallback
}

async function fetchTable(
  url: string,
): Promise<{ table: PricingTable; body: string } | null> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);
  try {
    const response = await fetch(url, { signal: controller.signal });
    if (!response.ok) return null;
    const body = await response.text();
    return { table: parseTable(body), body };
  } catch {
    return null; // network/parse failure -> fall back to cache or null
  } finally {
    clearTimeout(timer);
  }
}

function readTable(path: string): PricingTable | null {
  try {
    return parseTable(readFileSync(path, "utf8"));
  } catch {
    return null; // missing or unparseable -> no table
  }
}

function isFresh(path: string): boolean {
  try {
    return Date.now() - statSync(path).mtimeMs < CACHE_TTL_MS;
  } catch {
    return false;
  }
}

function writeCache(path: string, body: string): void {
  try {
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, body);
  } catch {
    // best effort; a missing cache just means we re-fetch next time
  }
}

function cachePath(): string | null {
  const base =
    process.env.XDG_CACHE_HOME ??
    (process.env.HOME ? `${process.env.HOME}/.cache` : null);
  return base ? `${base}/exo/litellm_prices.json` : null;
}
