#!/usr/bin/env python3
"""Per-task native-vs-over-exo comparison for the parity eval.

For each agent (codex, claude) finds the latest native + over-exo jobs, matches
tasks by base name (strips the __<hash> trial suffix), and prints per-task
rewards side by side, flagging discrepancies. Writes a JSON of discrepancies
for follow-up investigation.

Usage: python3 compare.py [jobs_dir]
"""
import glob
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
JOBS = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "jobs")

MATCHERS = [
    ("exo-codex", ("codex", "over_exo")),
    ("exo-claude-code", ("claude", "over_exo")),
    ("codex", ("codex", "native")),
    ("claude-code", ("claude", "native")),
]


def classify(eval_name):
    for prefix, cell in MATCHERS:
        if eval_name.startswith(prefix):
            return cell
    return None


def latest_job_per_cell():
    best = {}
    for rj in glob.glob(os.path.join(JOBS, "*", "result.json")):
        try:
            data = json.load(open(rj))
        except Exception:
            continue
        mtime = os.path.getmtime(rj)
        for eval_name in (data.get("stats", {}).get("evals") or {}):
            cell = classify(eval_name)
            if cell is None:
                continue
            if cell not in best or mtime > best[cell][1]:
                best[cell] = (os.path.dirname(rj), mtime)
    return {k: v[0] for k, v in best.items()}


def task_rewards(jobdir):
    """{base_task_name: reward} from a job's trial dirs."""
    out = {}
    for d in glob.glob(os.path.join(jobdir, "*", "")):
        rf = os.path.join(d, "verifier", "reward.txt")
        base = os.path.basename(os.path.dirname(d))
        if "__" in base:
            base = base.rsplit("__", 1)[0]
        try:
            out[base] = float(open(rf).read().strip())
        except Exception:
            out[base] = None  # errored / no reward (timeout, crash)
    return out


def main():
    jobs = latest_job_per_cell()
    print("# Per-task parity comparison\n")
    print("jobs:", {k: os.path.basename(v) for k, v in jobs.items()}, "\n")
    discrepancies = {}
    for agent in ("codex", "claude"):
        nat = jobs.get((agent, "native"))
        oe = jobs.get((agent, "over_exo"))
        print(f"\n## {agent}: native vs over_exo")
        if not nat or not oe:
            print(f"  missing job(s): native={nat} over_exo={oe}")
            continue
        rn, ro = task_rewards(nat), task_rewards(oe)
        tasks = sorted(set(rn) | set(ro))
        agree = diff = 0
        diffs = []
        for t in tasks:
            a, b = rn.get(t), ro.get(t)
            mark = ""
            if a != b:
                mark = "  <-- DIFF"
                diff += 1
                diffs.append({"task": t, "native": a, "over_exo": b})
            else:
                agree += 1
            print(f"  {t:50s} native={str(a):5s} over_exo={str(b):5s}{mark}")
        # means over tasks present in both, None treated as 0
        common = [t for t in tasks if t in rn and t in ro]
        mn = sum((rn[t] or 0) for t in common) / len(common) if common else 0
        mo = sum((ro[t] or 0) for t in common) / len(common) if common else 0
        print(f"  --- {agent}: {agree} agree, {diff} differ | "
              f"mean native={mn:.3f} over_exo={mo:.3f} (n={len(common)}, None=0) ---")
        discrepancies[agent] = diffs
    with open(os.path.join(HERE, "results", "discrepancies.json"), "w") as f:
        json.dump(discrepancies, f, indent=2)
    print("\nwrote results/discrepancies.json")


if __name__ == "__main__":
    main()
