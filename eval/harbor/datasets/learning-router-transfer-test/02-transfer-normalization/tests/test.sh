#!/usr/bin/env bash

expected=$(mktemp)
printf 'eve=22\nfrank=20\ngrace=20\nheidi=20\n' > "$expected"
if [[ -f /app/ranked.txt ]] && cmp -s "$expected" /app/ranked.txt; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
