#!/usr/bin/env bash

# The point of the trial is that a rebuild actually ran to completion and Exo
# saw the outcome land in its own event log. Either terminal status counts:
# restarting guardian services inside an eval root may legitimately fail, but
# the outcome still has to be recorded against this conversation, which only
# happens when the guardian is scoped to the active Exo root.
if [[ -f /app/restart.txt ]] &&
  grep -Eqi '^RESTART:[0-9a-f-]{36} STATUS:(succeeded|failed)$' /app/restart.txt; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
