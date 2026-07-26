//! Per-model price data and cost math, parsed from LiteLLM's pricing database.
//! Pure: no network, no globals.

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelEntry {
    #[serde(default)]
    pub litellm_provider: Option<String>,
    #[serde(default)]
    pub input_cost_per_token: Option<f64>,
    #[serde(default)]
    pub output_cost_per_token: Option<f64>,
    #[serde(default)]
    pub cache_read_input_token_cost: Option<f64>,
    #[serde(default)]
    pub cache_creation_input_token_cost: Option<f64>,
    /// Present in the upstream LiteLLM data; compaction reads it to size the
    /// prompt budget. Not every priced entry carries one.
    #[serde(default)]
    pub max_input_tokens: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCounts {
    pub prompt: Option<i64>,
    pub completion: Option<i64>,
    pub prompt_cached: Option<i64>,
    pub prompt_cache_creation: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    entries: HashMap<String, ModelEntry>,
}

impl PricingTable {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a LiteLLM `model_prices_and_context_window.json` document. The
    /// `sample_spec` doc entry and any entry without per-token rates are skipped.
    pub fn from_json_str(s: &str) -> anyhow::Result<Self> {
        let raw: HashMap<String, serde_json::Value> = serde_json::from_str(s)?;
        let entries = raw
            .into_iter()
            .filter(|(key, _)| key != "sample_spec")
            .filter_map(|(key, value)| {
                Some((key, serde_json::from_value::<ModelEntry>(value).ok()?))
            })
            .collect();
        Ok(Self { entries })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Exact match, else the longest entry key that is a prefix of `model` at a
    /// token boundary (next char absent or `-`/`:`), so dated revisions resolve
    /// (`claude-sonnet-4-6-20251022` -> `claude-sonnet-4-6`) without sliding
    /// `gpt-4o-mini` onto a `gpt-4` entry when `gpt-4o` is missing.
    pub fn lookup(&self, model: &str) -> Option<&ModelEntry> {
        if let Some(entry) = self.entries.get(model) {
            return Some(entry);
        }
        self.entries
            .iter()
            .filter(|(key, _)| {
                model.starts_with(key.as_str())
                    && matches!(
                        model.as_bytes().get(key.len()),
                        None | Some(b'-') | Some(b':')
                    )
            })
            .max_by_key(|(key, _)| key.len())
            .map(|(_, entry)| entry)
    }

    /// The model's input-token limit, or `None` when unknown. Resolves through
    /// the same prefix matching as pricing, so dated revisions inherit it.
    pub fn max_input_tokens(&self, model: &str) -> Option<i64> {
        self.lookup(model)?
            .max_input_tokens
            .filter(|limit| *limit > 0)
    }

    /// Total tokens occupying the model's input window for one call — the number
    /// to compare against `max_input_tokens`.
    ///
    /// This is *not* always `prompt_tokens`. For Anthropic-family providers that
    /// number counts only the fresh slice, with cache reads and cache writes
    /// reported separately and billed additively (the same asymmetry
    /// `compute_cost_usd` handles). Comparing the fresh slice alone against the
    /// context limit understates occupancy by exactly the cached prefix — which
    /// on a long, well-cached conversation is nearly the whole window, so a
    /// threshold check would never fire on the workload that needs it most.
    ///
    /// `None` when the model is unknown or reported no prompt tokens; callers
    /// fall back to their own budget in that case.
    pub fn input_occupancy(&self, model: &str, tokens: TokenCounts) -> Option<u64> {
        let entry = self.lookup(model)?;
        let prompt = tokens.prompt?.max(0) as u64;
        if !is_additive(entry.litellm_provider.as_deref()) {
            // Already inclusive of cache reads.
            return Some(prompt);
        }
        let cached = tokens.prompt_cached.unwrap_or(0).max(0) as u64;
        let created = tokens.prompt_cache_creation.unwrap_or(0).max(0) as u64;
        Some(prompt + cached + created)
    }

    /// USD cost for one call, or `None` if the model is unknown or unpriced.
    pub fn compute_cost_usd(&self, model: &str, tokens: TokenCounts) -> Option<f64> {
        let entry = self.lookup(model)?;
        let input = entry.input_cost_per_token?;
        let output = entry.output_cost_per_token.unwrap_or(0.0);
        let cache_read = entry.cache_read_input_token_cost.unwrap_or(input);
        let cache_write = entry.cache_creation_input_token_cost.unwrap_or(input);

        let prompt = tokens.prompt.unwrap_or(0).max(0) as f64;
        let completion = tokens.completion.unwrap_or(0).max(0) as f64;
        let cached = tokens.prompt_cached.unwrap_or(0).max(0) as f64;
        let created = tokens.prompt_cache_creation.unwrap_or(0).max(0) as f64;

        // Anthropic-family `prompt_tokens` excludes cached (bill additively);
        // everyone else includes it (subtract before billing fresh input).
        let fresh = if is_additive(entry.litellm_provider.as_deref()) {
            prompt
        } else {
            (prompt - cached).max(0.0)
        };
        Some(fresh * input + cached * cache_read + created * cache_write + completion * output)
    }
}

fn is_additive(provider: Option<&str>) -> bool {
    match provider {
        Some(p) => {
            p.starts_with("anthropic") || p.starts_with("vertex_ai-anthropic") || p == "azure_ai"
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "sample_spec": { "comment": "ignored" },
        "claude-sonnet-4-6": {
            "litellm_provider": "anthropic", "input_cost_per_token": 3e-06,
            "output_cost_per_token": 1.5e-05, "cache_read_input_token_cost": 3e-07,
            "cache_creation_input_token_cost": 3.75e-06,
            "max_input_tokens": 200000
        },
        "gpt-4o-mini": {
            "litellm_provider": "openai", "input_cost_per_token": 1.5e-07,
            "output_cost_per_token": 6e-07, "cache_read_input_token_cost": 7.5e-08,
            "max_input_tokens": 128000
        },
        "gpt-4": { "litellm_provider": "openai", "input_cost_per_token": 3e-05, "output_cost_per_token": 6e-05 },
        "us.anthropic.claude-sonnet-4-6": {
            "litellm_provider": "bedrock_converse", "input_cost_per_token": 3.3e-06,
            "output_cost_per_token": 1.65e-05, "cache_read_input_token_cost": 3.3e-07,
            "cache_creation_input_token_cost": 4.125e-06
        }
    }"#;

    fn table() -> PricingTable {
        PricingTable::from_json_str(FIXTURE).unwrap()
    }

    fn approx(a: f64, b: f64) {
        assert!(
            (a - b).abs() / b.abs().max(1e-12) < 1e-9,
            "expected {b}, got {a}"
        );
    }

    fn counts(prompt: i64, completion: i64, cached: i64, created: i64) -> TokenCounts {
        TokenCounts {
            prompt: Some(prompt),
            completion: Some(completion),
            prompt_cached: Some(cached),
            prompt_cache_creation: Some(created),
        }
    }

    #[test]
    fn input_occupancy_adds_cached_tokens_for_additive_providers() {
        // A window that is nearly all cache hits: 5k fresh over a 185k cached
        // prefix. Anthropic reports the 5k as `prompt_tokens`, so counting that
        // alone would put a 195k-token prompt at 2.5% of a 200k window and
        // compaction would never fire.
        let occupancy = table()
            .input_occupancy("claude-sonnet-4-6", counts(5_000, 100, 185_000, 5_000))
            .expect("priced model");
        assert_eq!(occupancy, 195_000);

        let limit = table().max_input_tokens("claude-sonnet-4-6").unwrap();
        assert!(occupancy as f64 > 0.7 * limit as f64);
    }

    #[test]
    fn input_occupancy_trusts_prompt_tokens_for_inclusive_providers() {
        // OpenAI's `prompt_tokens` already includes cache reads; adding them
        // again would double-count the cached prefix and compact too eagerly.
        assert_eq!(
            table()
                .input_occupancy("gpt-4o-mini", counts(50_000, 100, 40_000, 0))
                .expect("priced model"),
            50_000
        );
    }

    #[test]
    fn input_occupancy_resolves_dated_revisions_and_unknown_models() {
        assert_eq!(
            table()
                .input_occupancy("claude-sonnet-4-6-20251022", counts(10, 0, 90, 0))
                .expect("prefix match"),
            100
        );
        assert!(
            table()
                .input_occupancy("some-unlisted-model", counts(10, 0, 0, 0))
                .is_none()
        );
    }

    #[test]
    fn empty_table_is_none() {
        assert!(
            PricingTable::empty()
                .compute_cost_usd("claude-sonnet-4-6", counts(100, 50, 0, 0))
                .is_none()
        );
    }

    #[test]
    fn skips_sample_spec() {
        assert!(table().lookup("sample_spec").is_none());
        assert_eq!(table().len(), 4);
    }

    #[test]
    fn dated_revision_resolves_to_base() {
        let table = table();
        let entry = table.lookup("claude-sonnet-4-6-20251022").unwrap();
        assert_eq!(entry.litellm_provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn prefix_match_respects_token_boundary() {
        // `gpt-4o` is absent; lookup must NOT fall back to `gpt-4`.
        assert!(table().lookup("gpt-4o").is_none());
        // `gpt-4-0613` is a boundary extension of `gpt-4` and should match it.
        assert!(table().lookup("gpt-4-0613").is_some());
    }

    #[test]
    fn anthropic_additive() {
        // 500 fresh + 10k cache-read + 200 completion (prompt excludes cached).
        approx(
            table()
                .compute_cost_usd("claude-sonnet-4-6", counts(500, 200, 10_000, 0))
                .unwrap(),
            0.0075,
        );
    }

    #[test]
    fn anthropic_cache_creation() {
        approx(
            table()
                .compute_cost_usd("claude-sonnet-4-6", counts(0, 100, 0, 5_000))
                .unwrap(),
            0.02025,
        );
    }

    #[test]
    fn openai_inclusive_subtracts_cached() {
        // prompt=2000 includes 500 cached -> 1500 fresh.
        approx(
            table()
                .compute_cost_usd("gpt-4o-mini", counts(2_000, 1_000, 500, 0))
                .unwrap(),
            0.0008625,
        );
    }

    #[test]
    fn bedrock_is_inclusive_not_additive() {
        // prompt=2000 includes 500 cached -> fresh 1500 @ 3.3e-6, cached 500 @ 3.3e-7, 1000 out @ 1.65e-5.
        approx(
            table()
                .compute_cost_usd(
                    "us.anthropic.claude-sonnet-4-6",
                    counts(2_000, 1_000, 500, 0),
                )
                .unwrap(),
            1_500.0 * 3.3e-6 + 500.0 * 3.3e-7 + 1_000.0 * 1.65e-5,
        );
    }

    #[test]
    fn unknown_model_is_none() {
        assert!(
            table()
                .compute_cost_usd("acme-llm-9000", counts(100, 50, 0, 0))
                .is_none()
        );
    }

    #[test]
    fn max_input_tokens_reads_the_model_limit() {
        assert_eq!(table().max_input_tokens("claude-sonnet-4-6"), Some(200_000));
        assert_eq!(table().max_input_tokens("gpt-4o-mini"), Some(128_000));
    }

    #[test]
    fn max_input_tokens_resolves_dated_revisions() {
        assert_eq!(
            table().max_input_tokens("claude-sonnet-4-6-20251022"),
            Some(200_000)
        );
    }

    #[test]
    fn max_input_tokens_is_none_when_absent_or_unknown() {
        // Priced entries need not carry a limit; callers must fall back rather
        // than read a missing limit as zero.
        assert_eq!(table().max_input_tokens("gpt-4"), None);
        assert_eq!(table().max_input_tokens("acme-llm-9000"), None);
    }
}
