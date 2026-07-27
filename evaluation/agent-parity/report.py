#!/usr/bin/env python3
"""Build the agent-integration parity table from this folder's Harbor jobs.

Scans ./jobs/*/result.json, maps each eval to one of the four cells by its eval
name, keeps the most recent job per cell, and prints a Markdown comparison plus
a per-cell detail block. Run automatically by run.sh; also runnable standalone.

Usage:  python3 report.py [jobs_dir]   (default: ./jobs next to this file)
"""
import glob
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
JOBS = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "jobs")

# eval-name prefix -> (cell key, agent label, lane). Order matters: the exo-*
# prefixes must be tested before the bare agent prefixes they contain.
MATCHERS = [
    ("exo-codex", ("codex", "over_exo")),
    ("exo-claude-code", ("claude", "over_exo")),
    ("codex", ("codex", "native")),
    ("claude-code", ("claude", "native")),
]
AGENTS = [("codex", "Codex"), ("claude", "Claude Code")]
LANES = [("native", "Native"), ("over_exo", "Over Exo")]


def classify(eval_name: str):
    for prefix, cell in MATCHERS:
        if eval_name.startswith(prefix):
            return cell
    return None


def collect():
    """latest job per (agent, lane). Returns {(agent,lane): record}."""
    best: dict = {}
    for rj in glob.glob(os.path.join(JOBS, "*", "result.json")):
        try:
            data = json.load(open(rj))
        except Exception:
            continue
        stats = data.get("stats", {})
        mtime = os.path.getmtime(rj)
        for eval_name, ev in (stats.get("evals") or {}).items():
            cell = classify(eval_name)
            if cell is None:
                continue
            metrics = ev.get("metrics") or [{}]
            mean = metrics[0].get("mean") if metrics else None
            rec = {
                "eval": eval_name,
                "mean": mean,
                "n_trials": ev.get("n_trials"),
                "n_errors": ev.get("n_errors"),
                "cost_usd": stats.get("cost_usd"),
                "job": os.path.basename(os.path.dirname(rj)),
                "mtime": mtime,
            }
            if cell not in best or mtime > best[cell]["mtime"]:
                best[cell] = rec
    return best


def fmt_mean(rec):
    if rec is None:
        return "—"
    m = rec["mean"]
    cell = "n/a" if m is None else f"{m:.2f}"
    err = rec.get("n_errors") or 0
    return f"{cell} ({'err' if err else ''}{err if err else ''})".strip() if err else cell


def main():
    best = collect()
    print("\n# Agent-integration parity — Terminal-Bench 2.0\n")
    print("Mean reward (1.0 = solved). Same tasks, same model per agent.\n")
    # table
    print("| | " + " | ".join(label for _, label in LANES) + " |")
    print("|" + "---|" * (len(LANES) + 1))
    for akey, alabel in AGENTS:
        cells = [fmt_mean(best.get((akey, lkey))) for lkey, _ in LANES]
        print(f"| **{alabel}** | " + " | ".join(cells) + " |")
    # details
    print("\n## Detail\n")
    for akey, alabel in AGENTS:
        for lkey, llabel in LANES:
            rec = best.get((akey, lkey))
            if rec is None:
                print(f"- {alabel} / {llabel}: (no run found)")
                continue
            n = rec["n_trials"] or 0
            e = rec["n_errors"] or 0
            cost = rec["cost_usd"]
            cost_s = f"${cost:.2f}" if isinstance(cost, (int, float)) else "n/a"
            print(
                f"- {alabel} / {llabel}: mean={rec['mean']}  "
                f"trials={n} errors={e}  cost={cost_s}  "
                f"[{rec['eval']} · {rec['job']}]"
            )
    print()


if __name__ == "__main__":
    main()
