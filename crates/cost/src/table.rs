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
    ///
    /// Additionally, if no prefix or exact match succeeded, try keys that do
    /// not contain a `/` (non-route-qualified keys) as a fallback so that
    /// models reached directly from vendor endpoints (e.g. bare `gpt-4o`)
    /// can still be priced.
    pub fn lookup(&self, model: &str) -> Option<&ModelEntry> {
        // 1. Exact match.
        if let Some(entry) = self.entries.get(model) {
            return Some(entry);
        }
        // 2. Prefix match against route-qualified keys (contain `/`).
        let prefix = self.entries
            .iter()
            .filter(|(key, _)| {
                model.starts_with(key.as_str())
                    && matches! {
                        model.as_bytes().get(key.len()),
                        None | Some(b'-') | Some(b':')
                    }
            })
            .max_by_key(|(key, _)| key.len())
            .map(|(_, entry)| entry);

        // 3. Fallback against non-route-qualified keys (no `/` in key).
        let fallback = if prefix.is_none() {
            self.entries
                .iter()
                .filter(|(key, _)| key.as_bytes().iter().all(|b| *b != b'/'))
                .filter(|(key, _)| {
                    model.starts_with(key.as_str())
                        && matches! {
                            model.as_bytes().get(key.len()),
                            None | Some(b'-') | Some(b':')
                        }
                })
                .max_by_key(|(key, _)| key.len())
                .map(|(_, entry)| entry)
        } else {
            None
        };

        prefix.or(fallback)
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
            "cache_creation_input_token_cost": 3.75e-06
        },
        "gpt-4o-mini": {
            "litellm_provider": "openai", "input_cost_per_token": 1.5e-07,
            "output_cost_per_token": 6e-07, "cache_read_input_token_cost": 7.5e-08
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
}
