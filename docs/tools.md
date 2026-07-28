# Tools

Tools are model-callable functions executed by the TypeScript harness during a
turn. The model supplies structured arguments, the harness executes or rejects
the call, and the result is returned on the next model round.

## Implementation

The harness supports:

- the bootstrap tools `shell`, `inspect_tools`, `manage_tool`, and
  `rebuild_and_restart_exo`
- library tool modules selected in agent configuration
- manifest tools installed from workspace-relative directories or exact pinned
  Git commits

These modules run as trusted code in the harness process. They are not confined
by a tool capability sandbox. A handler can use the turn context to run sandbox
processes, read durable state, write artifacts, and resolve approved secret
references.

The legacy `install_agent_tool` and `uninstall_agent_tool` tools and
`.exo/agent-tools/` directory are opt-in compatibility paths. They are
available only when `enable_agent_tool_creation` is `true`; it defaults to
`false`. Configured library modules remain supported.

## Manifest and module

Every installable source has an `exo-tool.json` with exactly three fields:

```json
{
  "schemaVersion": 1,
  "id": "tool:local/example",
  "module": "index.ts"
}
```

The manifest owns only stable identity and the module entrypoint. The
TypeScript module owns its model-facing name, description, input and output
schemas, execution handler, and initialization contract.

Argument schemas must satisfy the model API's strict mode: every key in
`properties` also appears in `required`, optional parameters use nullable types
(for example `{"type": ["string", "null"]}`), and `additionalProperties` is
`false` at every object level. Installs that violate these rules are rejected,
and a previously installed tool that violates them is skipped at registration
with a logged error rather than failing the whole turn.

Initialization values are harness configuration, not model arguments. Keep
credentials out of the lockfile by passing an initialization value of exactly
`${ENV_VAR}`; the harness resolves it from the host environment each time the
tool loads. Never put raw secret values in definitions, prompts, or results.

## Workspace registry

There is one workspace-local registry at `.exo/tools/`. Agent and conversation
tool scopes, scope precedence, and remote discovery are not part of this
architecture. Exo copies installed sources into the managed store and executes
that copy.

The lockfile is `.exo/tools/tools.lock.json`. Each operational tool entry
contains only:

- `id`
- `source`
- `initialization`
- `installPath`

The lockfile does not copy manifest fields or track names, versions, timestamps,
status, errors, provenance, scopes, audit records, or quarantine state. A
malformed lockfile fails clearly. A broken installed module is logged and
skipped without writing audit or quarantine sidecars. Failed installs remove
their staging data.

## Management and inspection

`manage_tool` is the only write surface:

- `install` copies a workspace-relative directory or a Git repository at an
  exact commit, optionally selecting a contained subdirectory
- `remove` deletes an installation by stable tool id

Install is an upsert: installing the same stable manifest id replaces the
existing installation. There is no separate upgrade action. Successful changes
are available on the next model round.

For an agent-authored local tool, create the source under the mounted repository
at `/workspace/exo/.exo/tool-sources/<name>` and pass
`.exo/tool-sources/<name>` to `manage_tool`. Local source paths are resolved by
the host from the workspace root. Absolute sandbox paths such as `/tmp/...` or
`/workspace/...` are rejected because those names do not identify the same
filesystem location in the host harness.

`inspect_tools` is read-only. Its `list` and `get` operations inspect either
tools active in the current round or tools installed in the workspace registry.

The operator CLI is read-only:

```text
exo tools list
exo tools get <id>
```

It does not install, replace, or remove tools.

## Bootstrap surface

The bootstrap profile has four built-in tools:

```text
shell
inspect_tools
manage_tool
rebuild_and_restart_exo
```

`rebuild_and_restart_exo` uses the guarded host service rather than accepting
arbitrary deployment commands. Pass a short `reason` so the durable update
record under `.exo/guardian-updates/` is self-describing. When the deferred
job finishes, the same outcome is appended to the requesting conversation's
event log as a `rebuild_and_restart_exo` host event (including failures).

The practical profile adds the current scheduler and adapter tools plus the
shipped sandbox recovery, introspection, memory, todo, skill, and web tools.
Bootstrap and practical are the only profiles. Profiles are curated defaults,
not alternate registries, scopes, or trust mechanisms.

## Execution and Events

The harness validates arguments, dispatches the handler, and records
`tool_requested` and `tool_result` events. Failures become structured tool
results rather than crashing the turn. Large output is stored in artifacts and
represented in model context by a compact preview and reference.

Loading a local or Git-backed module is an explicit trust decision. Strong
module confinement and declared capabilities are deferred. The conversation
sandbox still constrains commands run through `shell` or sandbox process APIs,
but it does not sandbox the harness process that loaded the tool module.

Also deferred are code mode, generic adapter commands, remote registries, and
signatures.

## Scheduler and Adapters

The scheduler remains the current scheduler service with its existing tools; it
is not represented as an adapter in this phase. Adapters remain long-running
supervised workers that can wake conversations. Their existing lifecycle and
messaging tools may be included by practical profiles, while a generic
manifest-driven adapter command protocol is deferred.
