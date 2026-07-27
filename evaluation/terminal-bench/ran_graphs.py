#!/usr/bin/env python3
"""Graphs for the tasks exo actually RAN (excludes infra failures).
Reads reports/<run>/analysis_data.csv; writes PNGs into reports/<run>/ran_only/.

Usage: ran_graphs.py [<run>]   (default: the most recent reports/ dir)
"""
import csv, os, sys
import matplotlib; matplotlib.use("Agg")
import matplotlib.pyplot as plt

REPORTS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "reports")
RUN = sys.argv[1] if len(sys.argv) > 1 else sorted(os.listdir(REPORTS))[-1]
BASE = os.path.join(REPORTS, RUN)
OUT = os.path.join(BASE, "ran_only"); os.makedirs(OUT, exist_ok=True)

rows = list(csv.DictReader(open(os.path.join(BASE, "analysis_data.csv"))))
for r in rows:
    for k in ("reward", "cost", "tin", "tout", "reason", "cached", "shell", "tx_bytes"):
        r[k] = float(r[k] or 0)

ran = [r for r in rows if r["outcome"] != "infra"]            # tasks that ran (exclude infra failures)
data = [r for r in ran if r["outcome"] in ("pass", "wrong")]  # ran tasks with captured usage
GREEN, RED, GREY = "#2e7d32", "#c62828", "#9e9e9e"
col = lambda r: GREEN if r["outcome"] == "pass" else (RED if r["outcome"] == "wrong" else GREY)

# 1) Outcome summary of the 59 ran tasks
fig, ax = plt.subplots(figsize=(6, 4))
from collections import Counter
c = Counter(r["outcome"] for r in ran)
labels = [("pass", GREEN), ("wrong", RED), ("timeout", GREY)]
bars = [(k.upper(), c.get(k, 0), col2) for k, col2 in labels]
ax.bar([b[0] for b in bars], [b[1] for b in bars], color=[b[2] for b in bars])
for i, b in enumerate(bars): ax.text(i, b[1] + 0.3, str(b[1]), ha="center", fontweight="bold")
ax.set_title(f"Tasks exo actually ran (n={len(ran)}) — {c.get('pass',0)}/{len(ran)} = {100*c.get('pass',0)/len(ran):.0f}% passed")
ax.set_ylabel("tasks")
fig.tight_layout(); fig.savefig(os.path.join(OUT, "ran_outcomes.png"), dpi=130); plt.close(fig)

# 2) Per-task shell calls (turns), sorted, colored by outcome
ds = sorted(data, key=lambda r: r["shell"])
fig, ax = plt.subplots(figsize=(9, max(5, len(ds) * 0.26)))
ax.barh(range(len(ds)), [r["shell"] for r in ds], color=[col(r) for r in ds])
ax.set_yticks(range(len(ds))); ax.set_yticklabels([r["task"] for r in ds], fontsize=7)
ax.set_xlabel("shell tool calls (≈ agent turns)")
ax.set_title("Agent effort per task — shell calls (green=pass, red=fail)")
fig.tight_layout(); fig.savefig(os.path.join(OUT, "ran_calls_per_task.png"), dpi=130); plt.close(fig)

# 3) Per-task cost, sorted, colored by outcome
ds = sorted(data, key=lambda r: r["cost"])
fig, ax = plt.subplots(figsize=(9, max(5, len(ds) * 0.26)))
ax.barh(range(len(ds)), [r["cost"] for r in ds], color=[col(r) for r in ds])
ax.set_yticks(range(len(ds))); ax.set_yticklabels([r["task"] for r in ds], fontsize=7)
ax.set_xlabel("cost ($, gpt-5.5)")
ax.set_title("Cost per task (green=pass, red=fail)")
fig.tight_layout(); fig.savefig(os.path.join(OUT, "ran_cost_per_task.png"), dpi=130); plt.close(fig)

# 4) calls vs cost scatter, colored by outcome, sized by reasoning tokens
fig, ax = plt.subplots(figsize=(8.5, 6))
for r in data:
    ax.scatter(r["shell"], r["cost"], s=30 + r["reason"] / 200, color=col(r),
               alpha=0.7, edgecolors="white", linewidths=0.5)
ax.set_xlabel("shell tool calls (≈ turns)"); ax.set_ylabel("cost ($)")
ax.set_title("Effort vs cost (point size ∝ reasoning tokens; green=pass, red=fail)")
ax.grid(True, alpha=0.3)
import matplotlib.patches as mp
ax.legend(handles=[mp.Patch(color=GREEN, label="pass"), mp.Patch(color=RED, label="fail")], loc="upper left")
fig.tight_layout(); fig.savefig(os.path.join(OUT, "ran_calls_vs_cost.png"), dpi=130); plt.close(fig)

# 5) cost distribution: pass vs fail (strip)
fig, ax = plt.subplots(figsize=(6.5, 4.5))
for i, (oc, c2) in enumerate([("pass", GREEN), ("wrong", RED)]):
    ys = [r["cost"] for r in data if r["outcome"] == oc]
    xs = [i + (j % 7 - 3) * 0.03 for j in range(len(ys))]
    ax.scatter(xs, ys, color=c2, alpha=0.7)
    if ys: ax.hlines(sum(ys)/len(ys), i-0.2, i+0.2, color="black", lw=2)
ax.set_xticks([0, 1]); ax.set_xticklabels(["pass (n=%d)" % sum(1 for r in data if r["outcome"]=="pass"),
                                            "fail (n=%d)" % sum(1 for r in data if r["outcome"]=="wrong")])
ax.set_ylabel("cost ($)"); ax.set_title("Cost: passes vs fails (bar = mean) — fails cost more")
fig.tight_layout(); fig.savefig(os.path.join(OUT, "ran_cost_pass_vs_fail.png"), dpi=130); plt.close(fig)

print("wrote graphs to", OUT)
for f in sorted(os.listdir(OUT)): print("  ", f)
print(f"\nran={len(ran)} (pass {c.get('pass',0)}, wrong {c.get('wrong',0)}, timeout {c.get('timeout',0)}); quant charts cover {len(data)} with captured data")
