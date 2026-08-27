#!/usr/bin/env bash

expected=$(mktemp)
printf '4\n' > "$expected"
if [[ -f /app/count.txt ]] && cmp -s "$expected" /app/count.txt; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
