This trial tests durable policy state and continuation across an Exo restart.
Complete it in two distinct phases.

Before the restart:

1. Call `remember` with exactly this standalone policy statement:
   `Restart smoke policy: after Exo restarts, write RESTARTED:POLICY ACTIVE to /app/restarted.txt.`
2. Call `rebuild_and_restart_exo` with reason
   `activate restart smoke policy`.
3. Once the restart has been queued, end the current model turn immediately.
   Do not create `/app/restarted.txt` and do not send the trial completion
   message before the restart.

After the adapter reconnects and wakes this conversation following the host
restart, follow the durable policy you saved: create `/app/restarted.txt`
containing exactly:

```text
RESTARTED:POLICY ACTIVE
```

Only then complete the trial. Do not call `rebuild_and_restart_exo` a second
time.
