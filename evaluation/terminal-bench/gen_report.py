#!/usr/bin/env python3
"""Generate a report + graphs for the latest Harbor (Terminal-Bench) run.

Usage: python3 gen_report.py [job_dir]
If no job_dir given, uses the most recent jobs/* directory.
Outputs into reports/<job_name>/: results.csv, *.png, REPORT.md
"""
import csv
import datetime as dt
import glob
import json
import os
import re
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

ROOT = os.path.dirname(os.path.abspath(__file__))


def latest_job() -> str:
    jobs = sorted(glob.glob(os.path.join(ROOT, "jobs", "*/")))
    if not jobs:
        sys.exit("no jobs/ dirs found")
    return jobs[-1]


def parse_iso(s):
    if not s:
        return None
    try:
        return dt.datetime.fromisoformat(s.replace("Z", "+00:00"))
    except Exception:
        return None


def collect(job_dir):
    rows = []
    for d in sorted(glob.glob(os.path.join(job_dir, "*/"))):
        rp = os.path.join(d, "result.json")
        if not os.path.isfile(rp):
            continue
        with open(rp) as f:
            r = json.load(f)
        name = r.get("task_name") or os.path.basename(d.rstrip("/"))
        reward = (r.get("verifier_result") or {}).get("rewards", {}).get("reward")
        ei = r.get("exception_info") or {}
        exc = bool(r.get("exception_info"))
        emsg = ei.get("exception_message") or ""
        # classify failures: infra (env never started) vs timeout vs real
        if not exc:
            exc_kind = "none"
        elif ("Docker compose command failed" in emsg or "compose" in emsg.lower()
              or "mkdir -p /logs/agent" in emsg):
            exc_kind = "infra"  # task container env never started — local infra, not the agent
        elif "timed out" in emsg:
            exc_kind = "timeout"
        else:
            exc_kind = "other"
        start = parse_iso(r.get("started_at"))
        fin = parse_iso(r.get("finished_at"))
        dur = (fin - start).total_seconds() if start and fin else None
        # shell calls from the agent transcript
        shell = 0
        tx = os.path.join(d, "agent", "exo.txt")
        if os.path.isfile(tx):
            shell = len(re.findall(r"tool_call shell", open(tx, errors="ignore").read()))
        # token usage + cost harvested by ExoAgent (exo_usage.json), if present
        tokens_in = tokens_out = cost = None
        up = os.path.join(d, "agent", "exo_usage.json")
        if os.path.isfile(up):
            try:
                u = json.load(open(up))
                tokens_in = u.get("prompt_tokens")
                tokens_out = u.get("completion_tokens")
                cost = u.get("cost_usd")
            except Exception:
                pass
        rows.append(
            {
                "task": name,
                "reward": reward if reward is not None else 0.0,
                "passed": (reward or 0) >= 1.0,
                "exception": exc,
                "exc_kind": exc_kind,
                "duration_s": round(dur, 1) if dur else "",
                "shell_calls": shell,
                "tokens_in": tokens_in if tokens_in is not None else "",
                "tokens_out": tokens_out if tokens_out is not None else "",
                "cost_usd": round(cost, 4) if isinstance(cost, (int, float)) else "",
            }
        )
    return rows


def write_csv(rows, out):
    with open(out, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        w.writeheader()
        w.writerows(rows)


def graph_per_task(rows, out):
    rows2 = sorted(rows, key=lambda r: (-r["reward"], r["task"]))
    names = [r["task"] for r in rows2]
    colors = ["#2e7d32" if r["passed"] else ("#c62828" if not r["exception"] else "#9e9e9e") for r in rows2]
    fig, ax = plt.subplots(figsize=(10, max(4, len(names) * 0.32)))
    ax.barh(range(len(names)), [r["reward"] for r in rows2], color=colors)
    ax.set_yticks(range(len(names)))
    ax.set_yticklabels(names, fontsize=8)
    ax.invert_yaxis()
    ax.set_xlabel("reward")
    ax.set_xlim(0, 1.05)
    ax.set_title("exo + gpt-5.5 · Terminal-Bench 2.0 · per-task reward")
    fig.tight_layout()
    fig.savefig(out, dpi=130)
    plt.close(fig)


def graph_summary(rows, out):
    passed = sum(1 for r in rows if r["passed"])
    failed = sum(1 for r in rows if not r["passed"] and not r["exception"])
    errored = sum(1 for r in rows if r["exception"])
    parts = [("passed", passed, "#2e7d32"), ("failed", failed, "#c62828"), ("errored", errored, "#9e9e9e")]
    parts = [p for p in parts if p[1] > 0]
    fig, ax = plt.subplots(figsize=(5.5, 5.5))
    ax.pie([p[1] for p in parts], labels=[f"{p[0]} ({p[1]})" for p in parts],
           colors=[p[2] for p in parts], autopct="%1.0f%%", startangle=90)
    ax.set_title(f"Outcomes (n={len(rows)}) · mean reward {sum(r['reward'] for r in rows)/len(rows):.3f}")
    fig.tight_layout()
    fig.savefig(out, dpi=130)
    plt.close(fig)


def graph_effort(rows, out):
    fig, ax = plt.subplots(figsize=(8, 5))
    for r in rows:
        ax.scatter(r["shell_calls"], r["reward"], s=60,
                   color="#2e7d32" if r["passed"] else "#c62828", alpha=0.7)
    ax.set_xlabel("shell calls (agent effort)")
    ax.set_ylabel("reward")
    ax.set_ylim(-0.05, 1.05)
    ax.set_title("Reward vs. agent effort (shell calls)")
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(out, dpi=130)
    plt.close(fig)


def main():
    job = sys.argv[1] if len(sys.argv) > 1 else latest_job()
    job_name = os.path.basename(job.rstrip("/"))
    outdir = os.path.join(ROOT, "reports", job_name)
    os.makedirs(outdir, exist_ok=True)
    rows = collect(job)
    if not rows:
        sys.exit(f"no task results in {job}")
    write_csv(rows, os.path.join(outdir, "results.csv"))
    graph_per_task(rows, os.path.join(outdir, "per_task_reward.png"))
    graph_summary(rows, os.path.join(outdir, "outcomes.png"))
    graph_effort(rows, os.path.join(outdir, "reward_vs_effort.png"))

    n = len(rows)
    mean = sum(r["reward"] for r in rows) / n
    passed = sum(1 for r in rows if r["passed"])
    errored = sum(1 for r in rows if r["exception"])
    infra = sum(1 for r in rows if r["exc_kind"] == "infra")
    timeout = sum(1 for r in rows if r["exc_kind"] == "timeout")
    other_exc = sum(1 for r in rows if r["exc_kind"] == "other")
    ran = n - infra  # tasks whose environment actually started
    durs = [r["duration_s"] for r in rows if isinstance(r["duration_s"], (int, float))]
    costs = [r["cost_usd"] for r in rows if isinstance(r["cost_usd"], (int, float))]
    total_cost = sum(costs) if costs else None

    md = []
    md.append("# Exo + gpt-5.5 on Terminal-Bench 2.0 — Results\n")
    md.append(f"_Run `{job_name}`, generated {dt.datetime.now().strftime('%Y-%m-%d %H:%M')}._\n")
    md.append("## Headline\n")
    md.append(f"- **Raw score: {passed}/{n} = {100*passed/n:.0f}%** (mean reward {mean:.3f})"
              if n else "- No tasks ran.")
    if infra and ran:
        md.append(f"- **On tasks that actually executed: {passed}/{ran} = {100*passed/ran:.0f}%** "
                  f"(excludes {infra} tasks whose container env failed to start — local infra, not the agent)")
    md.append(f"- Exceptions: **{errored}** — {infra} infra (env never started), {timeout} timeouts, {other_exc} other")
    if durs:
        md.append(f"- Median task time: {sorted(durs)[len(durs)//2]:.0f}s; total agent wall-clock ~{sum(durs)/60:.0f} min")
    if total_cost is not None:
        md.append(f"- **Total cost: ${total_cost:.2f}** (gpt-5.5, {len(costs)}/{n} tasks reported; ${total_cost/max(1,len(costs)):.3f}/task avg)")
    else:
        md.append("- Cost: not captured for this run (see OpenAI usage dashboard for total)")
    md.append("\n> Agent: Exo (minimal shell harness, local-process sandbox) running gpt-5.5 in-container via Harbor.")
    md.append("> 'infra' exceptions are local Docker image build/pull failures (e.g. Docker Hub pull limits on")
    md.append("> unauthenticated pulls, or containerd layer corruption on large images) — not the benchmark or the")
    md.append("> agent. `docker login` + retrying those tasks recovers them.\n")
    md.append("## Graphs\n")
    md.append("![outcomes](outcomes.png)\n")
    md.append("![per-task reward](per_task_reward.png)\n")
    md.append("![reward vs effort](reward_vs_effort.png)\n")
    md.append("## Per-task results\n")
    md.append("| task | reward | shell calls | duration (s) | cost ($) | note |")
    md.append("|---|---|---|---|---|---|")
    for r in sorted(rows, key=lambda r: (-r["reward"], r["task"])):
        note = "error" if r["exception"] else ("pass" if r["passed"] else "fail")
        md.append(f"| {r['task']} | {r['reward']:.1f} | {r['shell_calls']} | {r['duration_s']} | {r['cost_usd']} | {note} |")
    md.append("")
    with open(os.path.join(outdir, "REPORT.md"), "w") as f:
        f.write("\n".join(md))
    print(f"report written: {outdir}/REPORT.md  (mean {mean:.3f}, {passed}/{n} passed)")


if __name__ == "__main__":
    main()
