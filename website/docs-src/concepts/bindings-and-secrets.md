---
title: Bindings & Secrets
description: How credentials stay out of the model's reach.
---

# Bindings & Secrets

Exo splits credentials into two records so that configuration can be
shared, inspected, and versioned without ever exposing key material.

## Secrets

Secrets hold **only credential material** — opaque API keys, OAuth tokens.
They live in the exoharness secret store (file-backed by default, Apple
Keychain supported):

```bash
exo secret set openai --env OPENAI_API_KEY   # reads the variable name literally
exo secret set openai --value "$OPENAI_API_KEY"  # stores the expanded value
```

## Bindings

Bindings are **non-secret configuration that refer to secrets**:

- an *env var binding* maps a variable name to a secret,
- an *LLM binding* defines a provider/model plus optional credentials
  (`exo model register`),
- a *model provider binding* registers an LLM gateway or vendor endpoint as
  data — base URL, credential, wire format, and the models it serves
  (`exo model provider register`),
- a *sandbox binding* defines a sandbox provider plus its credentials
  (`exo provider configure`),
- an *MCP binding* defines a server URL plus optional credentials.

## Model providers

Any OpenAI-compatible gateway or vendor endpoint can be registered as data,
with no provider-specific code in the harness. Model bindings then route
through the provider record, which supplies the base URL, credential, wire
format (`chat-completions`, `responses`, or `anthropic`), and auth scheme:

```bash
exo secret set opper --env OPPER_API_KEY
exo model provider register opper \
  --base-url https://api.opper.ai/v3/compat \
  --secret opper --format chat-completions --discover \
  --cost-usage-path opper.cost.total
exo model register terra --provider opper --model openai/gpt-5.6-terra
```

- `--discover` fetches the provider's model list from the standard
  OpenAI-compatible `GET {base_url}/models` endpoint; `--models a,b,c`
  declares it manually. The list is validated (with close-match
  suggestions) when a model binding is registered; at request time the
  provider itself stays authoritative, so a stale snapshot never breaks
  existing bindings.
- `--auth` sets how the credential is sent (`bearer`, `x-api-key`, or
  `none`), independent of the wire format — e.g. a gateway can speak the
  Anthropic format but authenticate with `Bearer`. It defaults to the
  format's native scheme. A provider either authenticates with its own
  secret or is explicitly unauthenticated (`--auth none`, for local
  endpoints); ambient environment keys are never used.
- `--base-url` is the provider root; each client appends its own path
  (`/chat/completions`, `/v1/messages`, …). For the Anthropic format,
  register the root **without** `/v1` (a `/v1`-suffixed value is rejected
  at registration).
- `--cost-usage-path` points at where the provider reports spend under the
  response `usage` object (cost is not part of the standard chat
  completions schema — OpenRouter uses `cost`, Opper uses
  `opper.cost.total`).

## Scoping

Executors, agents, and individual conversations can all define bindings and
secrets. **Conversation-scoped values override agent-scoped values**, so a
single agent can talk to different endpoints or use different credentials
per conversation.

## Keeping secrets away from the model

Secrets can be used without the LLM's knowledge — for example to
authenticate MCP servers — or securely mounted inside sandboxes so
specific programs can access them while the LLM can neither view nor
exfiltrate them. This is a direct payoff of the
[exoharness/executor split](./exoharness-and-executor): the layer the
agent can modify never holds the keys.
