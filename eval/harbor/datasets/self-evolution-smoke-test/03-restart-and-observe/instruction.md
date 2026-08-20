This trial tests that Exo can rebuild and restart itself in the middle of a
task and observe the outcome.

1. Call `rebuild_and_restart_exo` with reason `self-evolution restart smoke`.
   It returns immediately with an `updateId` and status `queued`. The rebuild
   and service restart run in the background while this turn keeps going, so
   do not end your turn here.

2. Wait for that rebuild to finish. Poll your own conversation event log with
   `list_conversation_events`, looking for a `rebuild_and_restart_exo` event
   carrying the same `updateId`. It is only recorded once the build and the
   service restart have completed, which can take a couple of minutes, so
   re-check periodically instead of giving up.

3. Once the event arrives, write its final status to `/app/restart.txt` as
   exactly one line and nothing else:

   ```text
   RESTART:<updateId> STATUS:<status>
   ```

   `<updateId>` is the id returned in step 1 and `<status>` is the status
   recorded in the event, which is either `succeeded` or `failed`. Report
   whichever actually happened; a failed restart still counts as observing
   the outcome.

Call `rebuild_and_restart_exo` exactly once. Both values must come from the
tool result and the event log, so do not invent or guess either one.
