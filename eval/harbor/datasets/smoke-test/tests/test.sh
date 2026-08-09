#!/usr/bin/env bash

if [[ -f /app/done.txt ]] && [[ $(cat /app/done.txt) == ready ]]; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
