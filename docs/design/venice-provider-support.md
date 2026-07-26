# Venice Provider Support Plan

## Goal

Add Venice as a first-class model provider while reusing Exo's existing
OpenAI-compatible Chat Completions runtimes. Users should be able to select
Venice during canonical setup, store a `VENICE_API_KEY`, and run a Venice model
through `https://api.venice.ai/api/v1`.

The initial integration will cover standard chat, streaming, tools, usage, and
cost calculation already supported by Exo's OpenAI-compatible path. It will not
add Venice-specific request extensions such as `venice_parameters`; those need
a separate provider-options design so they do not leak into the generic model
request contract.

## Implementation

1. **Add Venice to canonical setup**
   - Update `setup.sh` with:
     - `DEFAULT_VENICE_BASE_URL=https://api.venice.ai/api/v1`
     - `DEFAULT_VENICE_MODEL=zai-org-glm-5`
     - `VENICE_API_KEY` in the documented environment overrides
     - A third provider-menu choice and a `venice` branch in
       `configure_model_provider`
   - Prompt for the Venice upstream model ID just as setup does for OpenRouter,
     while preserving `EXO_UPSTREAM_MODEL` as the non-interactive override.
   - Register the model with secret name `venice`, secret environment variable
     `VENICE_API_KEY`, and the Venice base URL.

2. **Route Venice through Chat Completions in TypeScript**
   - In `typescript/model-runtime/responses.ts`, recognize bindings whose parsed
     base-URL hostname is `api.venice.ai`.
   - Route those bindings to `ChatCompletionsRuntime` before model-name-based
     Responses API selection. This prevents a Venice model whose name resembles
     an OpenAI Responses-only model from being sent to `/responses`.
   - Keep the existing OpenAI SDK client, streaming conversion, tool-call
     handling, and usage accounting unchanged.
   - Use hostname equality rather than a substring match for the new detection
     so lookalike hosts cannot select provider-specific behavior.

3. **Route Venice through Chat Completions in Rust**
   - In `crates/executor/src/harness_runtime.rs`, detect the Venice API hostname
     from `ModelRequest.base_url` before the generic OpenAI branch.
   - Resolve a Venice configuration with:
     - provider alias `venice`
     - provider kind `openai`
     - `ProviderFormat::ChatCompletions`
     - Bearer authorization
     - request API key first, then `VENICE_API_KEY`
   - Reuse the existing universal request/response conversion and streaming
     accumulator. No Venice-specific wire structs are needed.

4. **Document configuration**
   - Add `VENICE_API_KEY=` to `.env.example`.
   - Update `README.md` and
     `website/docs-src/getting-started/installation.md` to list Venice as a
     supported setup provider and explain that setup stores the key in Exo's
     secret store.
   - Include the Venice base URL, default model, and a non-interactive setup
     example using `EXO_MODEL_PROVIDER=venice`, `EXO_UPSTREAM_MODEL`, and
     `VENICE_API_KEY`.

## Tests

1. Extend `typescript/model-runtime/responses.test.ts` to verify:
   - The canonical Venice URL is recognized.
   - Null, malformed, and lookalike-host URLs are rejected.
   - A Responses-looking model bound to Venice still selects
     `ChatCompletionsRuntime`.

2. Extend the unit tests in `crates/executor/src/harness_runtime.rs` to verify:
   - Venice selects provider alias `venice`, OpenAI provider kind, and Chat
     Completions format.
   - Bearer auth uses a request-bound key.
   - `VENICE_API_KEY` is used when the request has no key.
   - An invalid Venice base URL returns an error.

3. Add shell-level setup coverage for provider selection/configuration by
   extracting the provider mapping into sourceable setup helpers or invoking
   setup functions in a test mode. Assert the Venice label, key environment
   variable, base URL, and default upstream model.

4. Run focused checks, then the repository suites:
   - `pnpm test -- typescript/model-runtime/responses.test.ts`
   - `cargo test -p executor harness_runtime`
   - `pnpm test`
   - `cargo test --workspace`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`

## Acceptance Criteria

- Interactive setup offers OpenAI, OpenRouter, and Venice.
- Non-interactive setup accepts `EXO_MODEL_PROVIDER=venice`.
- Setup registers a Venice binding with `VENICE_API_KEY` and the canonical base
  URL.
- Both TypeScript and Rust execution paths use `/chat/completions` for Venice,
  including streaming and tool calls.
- Existing OpenAI, OpenRouter, and Anthropic routing remains unchanged.
- Documentation and example environment files describe the new provider.
