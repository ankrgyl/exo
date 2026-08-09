#!/usr/bin/env bash

if [[ $(cat /app/timeout-started.txt 2>/dev/null) == "STARTED" ]] &&
  [[ ! -e /app/timeout-finished.txt ]]; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
