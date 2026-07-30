---
title: Task Scheduler
description: Run recurring work in a sandbox on a schedule, with each run waking the conversation.
---

# Task Scheduler

The **task scheduler** runs recurring work in a sandbox on a schedule —
independent of whether anyone is chatting. It's how a long-running agent
does things like "check the BBC headlines every hour" or "run the test
suite nightly."

Like [tools](./tools), the scheduler is executor-level, not an
exoharness primitive. The agent manages tasks through scheduler tools; a
separate **scheduler runner** process owns timing and execution and is
started as one of the [canonical agent's](./canonical-agent) services.

## Managing tasks

The agent has four tools:

- `schedule_sandbox_task` — create a recurring task
- `list_scheduled_tasks` — see active tasks
- `cancel_scheduled_task` — disable a task but keep its history
- `delete_scheduled_task` — remove a task entirely

## What a task is

Each task records:

- **schedule** — `@every 10m`, `@every 1h`, a simple cron interval like
  `*/30 * * * *`, or `@at 2026-07-26T17:00:00Z` for a one-shot (see below)
- **command** — the argv to run (e.g. `["bash", "-lc", "curl -fsSL …"]`)
- **setupCommand** — optional argv run before each run (install deps, etc.)
- **sandboxMode** — where it runs (see below)
- **missed** — what to do about slots that elapsed while nothing was running
  (see below)
- **reportPrompt** — how to summarize each completed run back to the user
- **maxOutputBytes** — how much output to retain before truncating

### Sandbox mode

| Mode | Runs in |
|:-----|:--------|
| `agent` | The shared, persistent agent [sandbox](./sandboxes) (default) |
| `conversation` | This conversation's sandbox |
| `task_fresh` | A separate sandbox created for the task and reused across its runs |

## When a task fires

A recurring task has a **grid**: fires land on `anchor + n × interval`, where
the anchor is when the task was created. The grid is fixed, so a run that
takes longer than its own interval does not push the next fire out — the
schedule never drifts away from the times the user asked for. A task whose
command outlives its interval simply runs back to back.

### Missed fires

If the host is down — or the scheduler runner is stopped — slots elapse with
nothing running. The task's `missed` policy decides what it is owed when the
runner comes back:

| Policy | On coming back |
|:-------|:---------------|
| `skip` | Fire nothing; resume at the next future slot. For work whose value expired with its slot |
| `once` | Fire one catch-up run, then resume on the grid (default) |
| `all` | Fire every missed slot in order, capped at 100 |

The policy only applies to a real backlog. A task that is merely a little
late — the normal case, since the runner polls — always fires exactly once,
whatever its policy. Each evaluation is recorded on the task, so a listing
shows that runs were skipped rather than leaving the agent to infer it from
gaps.

### One-shots

`@at <rfc3339>` schedules a single fire at an absolute time, e.g.
`@at 2026-07-26T17:00:00Z`. A timestamp already in the past is accepted and
fires as soon as the runner sees it — the task was still owed, just late.
Once it has fired, the task is stamped `completed_at_ms` and is never due
again. It stays visible in listings, so the agent can see that the thing it
promised to do did happen.

## Durability

Waking the conversation is the only thing that tells it a run happened, and
nothing retries a machine-sent event. So the scheduler records the fire —
prompt included — before it attempts the wakeup, and clears it only once the
wakeup lands. On startup the runner redelivers anything still outstanding, so
a process that dies between finishing the command and waking the conversation
does not swallow the result. Delivery is at-least-once, keyed by
`(task, slot)`, which bounds a crash mid-delivery to one repeated wakeup
rather than a loop.

Task records are versioned and migrated forward on read, so state written by
an older build is upgraded rather than silently reinterpreted.

## How a run reports back

When a run finishes, the scheduler stores its output as an
[artifact](./data-model) and **wakes the conversation** — it starts a
new turn carrying a compact result guided by the task's `reportPrompt`. So
scheduled work shows up in the conversation as if the agent had just done
it, and the durable record (last run, next run, latest result) lives in the
task record.

::: info
  Registering a task writes it to the scheduler's store immediately, but a
  task only *runs* if the scheduler runner process is active. The canonical
  agent's setup starts it; a bare CLI agent has no runner.
:::
