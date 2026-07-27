Set up the agent-cli adapter so I can talk to you from my shell with `exo-cli`.

Create a built-in agent-cli adapter if one does not already exist for this conversation. Use these settings:

- name: `agent-cli`
- source: `built_in`
- config type: `agent-cli`
- socketPath: `null` (defaults to `~/.exo/agent-cli.sock` on the host)
- mountRoot: the host directory that is bind-mounted into your sandbox at `/agent-cli`. You cannot reliably determine host paths from inside your sandbox, so do not infer this from mount tables. If this message does not state the host workspace root explicitly, ask me for it before creating the adapter.
- mountPath: `null` (defaults to `/agent-cli`)

After creating or confirming the adapter, briefly tell me the adapter id, the socket path, the workspace root it covers, and an example `exo-cli` command I can run to test it.
