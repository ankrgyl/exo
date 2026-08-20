#!/usr/bin/env bash

expected=$'alice=15\ncarol=15\nbob=11\ndave=3'
if [[ -f /app/ranked.txt ]] && [[ $(cat /app/ranked.txt) == "$expected" ]]; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
