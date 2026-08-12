#!/usr/bin/env bash

if [[ -f /app/restarted.txt ]] &&
  [[ $(cat /app/restarted.txt) == "RESTARTED:POLICY ACTIVE" ]]; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
