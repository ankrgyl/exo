#!/usr/bin/env bash

if [[ -f /app/evolution.txt ]] &&
  [[ $(cat /app/evolution.txt) == "EVOLVED:FIRST TRIAL" ]]; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
