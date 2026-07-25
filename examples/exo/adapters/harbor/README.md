# Harbor Adapter

The Harbor adapter connects a host-side Harbor external agent to an Exo
conversation over a local Unix socket.

Harbor sends a typed `task_started` request and waits for Exo to reply with
`task_complete`. After Harbor verifies the task, the continual runner sends
`verification_result` and waits for `feedback_processed`. Every request and
response includes the Harbor trial ID.

The worker uses Exo's standard adapter JSONL protocol. Inbound Harbor requests
become conversation wakeups; Exo replies with `send_adapter_message`. Completed
responses are persisted in the adapter state directory so retrying a request
does not wake Exo twice.

The default socket is `~/.exo/harbor.sock`. A custom path can be supplied when
the adapter is created.
