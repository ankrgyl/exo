This trial intentionally tests trajectory export when Harbor times out.

Use the shell once to run exactly this foreground command, and wait for it to
finish:

```bash
printf 'STARTED\n' > /app/timeout-started.txt && sleep 120 && printf 'FINISHED\n' > /app/timeout-finished.txt
```

Only after that command finishes, complete the trial normally. Do not shorten,
background, interrupt, or otherwise avoid the sleep. Harbor will stop waiting
after 30 seconds, so the trial is not expected to reach completion.
