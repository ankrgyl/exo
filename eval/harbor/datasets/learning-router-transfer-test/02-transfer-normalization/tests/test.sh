#!/usr/bin/env bash

expected=$'eve=22\nfrank=20\ngrace=20\nheidi=20'
if [[ -f /app/ranked.txt ]] && [[ $(cat /app/ranked.txt) == "$expected" ]]; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
