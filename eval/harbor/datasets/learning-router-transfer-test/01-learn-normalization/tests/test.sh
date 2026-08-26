#!/usr/bin/env bash

expected=$(mktemp)
printf 'alice=15\ncarol=15\nbob=11\ndave=3\n' > "$expected"
if [[ -f /app/ranked.txt ]] && cmp -s "$expected" /app/ranked.txt; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
