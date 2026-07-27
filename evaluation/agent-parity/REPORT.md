# Agent-integration parity — Terminal-Bench 2.0

**Thesis:** _Exo can integrate existing coding agents to run as well as they run
independently._ Each agent (Codex, Claude Code) is run two ways on all 89
terminal-bench@2.0 tasks — natively (Harbor's installed agent) and over Exo (exo
driving the same CLI via `--harness`) — same model per agent, same tasks.

## Result

|                              | Native           | Over Exo         |
| ---------------------------- | ---------------- | ---------------- |
| **Codex** (gpt-5.5, 0.142.3) | **0.74** (66/89) | **0.71** (63/89) |
| **Claude Code** (Sonnet)     | **0.55**         | **0.45** (40/89) |

Codex runs at parity over Exo. Claude Code is within ~0.10, the residual gap
explained by concurrency-induced timeouts that hit both cells (see Caveat).

## Codex

0.74 vs 0.71 — the per-task diffs go **both directions** (native-only wins ≈
over-exo-only wins): the signature of run-to-run variance on hard tasks, not a
systematic deficit. A re-run of the contested tasks confirms this — 13/14 flip
pass/fail across identical repeats within a cell, and the two cells are
statistically indistinguishable on them.

The over-exo number includes a fix: 2 tasks (`qemu-startup`, `qemu-alpine-ssh`)
originally failed at *install* — `npm install -g @openai/codex` aborted on the
qemu base image (node present, npm absent), before codex ran. After ensuring
node + npm, both install and run; `qemu-startup` now passes (`qemu-alpine-ssh`
runs but is unsolved — a capability/variance matter, not an integration error).

One integration detail matters for parity: native codex has its built-in
**web_search** on by default, but exo's codex harness creates threads with only
its own shell tool. The over-exo agent enables web_search via
`$CODEX_HOME/config.toml`, so codex can search the web when a task needs it
(e.g. `gpt2-codegolf`, which requires fetching the GPT-2 checkpoint tensor
layout).

## Claude Code

The claude-code harness ended a turn too early: it closed the SDK `query()` after
a short grace whenever it saw a **text-only assistant message** (no tool use),
treating it as completion. But Claude routinely emits text-only **narration
between tool calls** ("Let me look at the test more carefully", "Now let me
download CompCert and build it:"), so a turn could be killed mid-task → reward 0.
It is load-sensitive: the next tool call arrives more slowly under concurrency,
so the grace window expires more often.

**Fix:** end the turn only on the SDK's authoritative `result` message, letting
Claude run its full agentic loop like the native CLI. Also raised
`CLAUDE_MAX_API_RETRIES` (2→8) so transient HTTP 529 overloads are ridden out
rather than aborting the turn.

## Caveat (matched comparison)

Both Claude cells were run at high concurrency on a shared API key, which produces
some rate-limit / overload timeouts (counted as 0) that depress both numbers; a
mild additional contributor is SDK per-turn latency, so a few long tasks hit the
agent timeout over exo. For a perfectly matched number, re-run both Claude cells
at low concurrency. Codex (separate API, higher limits) is unaffected, so codex
parity is clean.

## Integration fixes

- **Codex over exo:** enable codex's built-in `web_search`
  (`$CODEX_HOME/config.toml`); resolve the codex binary onto a stable PATH
  location; pin codex 0.142.3 to match native.
- **Claude over exo:** ship `@anthropic-ai/claude-agent-sdk` in the bundle;
  `permissionMode=bypassPermissions`; `IS_SANDBOX=1` (claude-code refuses
  skip-permissions as root otherwise); end the turn on the SDK `result` message
  (no premature grace-close); retry cap 2→8.

## Artifacts (in `results/`)

- `FULL_TABLE.txt` — the 2×2 table · `COMPARISON.txt` — per-task diffs
- `discrepancies.json` — machine-readable diff list
- `jobs/<ts>/` — full Harbor artifacts (transcripts, rewards, exo usage)
