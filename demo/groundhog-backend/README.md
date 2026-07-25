# Groundhog conversation-event backend demo

This harness-level demo exercises Groundhog as the canonical store for exo
conversation events. It appends synthetic turn and tool events through the
`BasicExoHarness` API, starts fresh harness processes to replay them, verifies a
sealed Groundhog log, queries event counts, and exercises an experimental
kernel-config mismatch policy that retires one Groundhog source and records a
successor.

This is not a model or tool-execution demo. The `seed` command constructs the
events directly, so no API credentials are required. A real agent turn uses
exo's separate TypeScript model runner, a registered model binding, and provider
credentials. This demo does not claim that an agent produced or remembered the
synthetic content.

This is also not a complete implementation of the current scope of
[exo issue #154](https://github.com/exoharness/exo/issues/154), which proposes
tagging all canonical state and explicitly defers seal/fork policy. Only
conversation events use Groundhog here. Agent records, conversation records,
artifacts, bindings, secrets, and sandbox state retain their existing storage
behavior. The source-retirement behavior demonstrates one possible mismatch
policy outside that current scope.

## Prerequisites

- Bash, Git, Rust/Cargo, curl, Python 3, and standard Unix utilities.
- A clean Groundhog checkout at the pinned revision in
  [`groundhog-revision.txt`](./groundhog-revision.txt). The script verifies the
  checkout revision and builds its `groundhog` binary with `--locked`.
- This exo checkout. The script builds the Rust demo driver.

Run it from the exo repository root:

```sh
git -C /path/to/groundhog checkout "$(cat demo/groundhog-backend/groundhog-revision.txt)"
GROUNDHOG_ROOT=/path/to/groundhog ./demo/groundhog-backend/demo.sh
```

By default, the script creates a new short path under `/tmp` and leaves the data
there for inspection. To choose the path, set `DEMO_ROOT` to a path that does
not already exist. The script never recursively deletes `DEMO_ROOT`.

Set `STARTUP_TIMEOUT_SECONDS` to change the 10-second server startup deadline.
Startup failures report the child process status and the end of its server log.
