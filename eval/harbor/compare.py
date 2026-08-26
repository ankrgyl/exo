#!/usr/bin/env python3
"""Run two contamination-safe Harbor reflection strategies as matched arms."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import shutil
import subprocess
import sys
from fnmatch import fnmatch
from pathlib import Path
from typing import Any


ROUTES = ("memory", "skill", "tool", "policy")
REUSE_METRICS = (
    "prior_skill_reuse_count",
    "prior_agent_tool_reuse_count",
    "learning_activation_count",
)
LIFECYCLE_METRICS = (
    "proposal_count",
    "promotion_count",
    "rejection_count",
    "discard_count",
    "unresolved_count",
)
REFLECTION_STRATEGIES = ("memory", "router", "lifecycle")
EXPECTED_TASK_SEQUENCES = {
    "learning-router-transfer-test": [
        "exo/learning-router-normalization-first",
        "exo/learning-router-normalization-transfer",
        "exo/learning-router-unrelated-control",
    ],
}
SNAPSHOT_IGNORES = {
    ".exo",
    ".git",
    ".local",
    ".venv",
    "__pycache__",
    "node_modules",
    "target",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run identical Harbor tasks with two reflection strategies from "
            "fresh Exo states."
        ),
    )
    parser.add_argument("--dataset", default="learning-router-transfer-test")
    parser.add_argument("--dataset-path", type=Path)
    parser.add_argument(
        "--provider", choices=("openai", "openrouter"), default="openai"
    )
    parser.add_argument("--model", default="gpt-5.5")
    parser.add_argument("--n-tasks", type=int)
    parser.add_argument("--n-attempts", type=int, default=1)
    parser.add_argument(
        "--include-task-name",
        dest="include_task_names",
        action="append",
        default=[],
    )
    parser.add_argument(
        "--baseline-strategy",
        choices=REFLECTION_STRATEGIES,
        default="router",
    )
    parser.add_argument(
        "--candidate-strategy",
        choices=REFLECTION_STRATEGIES,
        default="lifecycle",
    )
    parser.add_argument(
        "--arm-order",
        choices=("baseline-first", "candidate-first"),
        default="baseline-first",
    )
    parser.add_argument(
        "--exo-bin",
        type=Path,
        help="prebuilt Exo binary; defaults to target/debug/exo",
    )
    return parser.parse_args()


def _copy_ignore(_directory: str, names: list[str]) -> set[str]:
    return set(names) & SNAPSHOT_IGNORES


def copy_source_snapshot(source: Path, destination: Path) -> str:
    """Copy one source snapshot without generated state and return its digest."""
    shutil.copytree(source, destination, ignore=_copy_ignore, symlinks=True)
    return workspace_digest(destination)


def link_runtime_dependencies(source: Path, destination: Path) -> None:
    """Expose installed dependencies without copying them into each arm."""
    dependencies = source / "node_modules"
    if not dependencies.is_dir():
        raise ValueError(
            f"runtime dependencies not found: {dependencies}; run pnpm install first"
        )
    (destination / "node_modules").symlink_to(
        dependencies.resolve(), target_is_directory=True
    )


def workspace_digest(workspace: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(workspace.rglob("*")):
        relative = path.relative_to(workspace).as_posix()
        if path.is_symlink():
            digest.update(b"link\0")
            digest.update(relative.encode())
            digest.update(b"\0")
            digest.update(os.readlink(path).encode())
        elif path.is_file():
            digest.update(b"file\0")
            digest.update(relative.encode())
            digest.update(b"\0")
            with path.open("rb") as file:
                for block in iter(lambda: file.read(1024 * 1024), b""):
                    digest.update(block)
    return digest.hexdigest()


def expected_task_sequence(args: argparse.Namespace) -> list[str] | None:
    sequence = EXPECTED_TASK_SEQUENCES.get(args.dataset)
    if sequence is None:
        return None
    selected = list(sequence)
    if args.include_task_names:
        selected = [
            task
            for task in selected
            if any(fnmatch(task, pattern) for pattern in args.include_task_names)
        ]
    if args.n_tasks is not None:
        selected = selected[: args.n_tasks]
    return selected * args.n_attempts


def build_comparison(
    *,
    baseline: dict[str, Any],
    candidate: dict[str, Any],
    baseline_strategy: str,
    candidate_strategy: str,
    baseline_summary_path: Path,
    candidate_summary_path: Path,
    provider: str,
    source_digest: str,
    arm_order: list[str],
    expected_task_sequence: list[str] | None = None,
) -> dict[str, Any]:
    """Validate matched arms and return candidate-minus-baseline measurements."""
    if baseline_strategy == candidate_strategy:
        raise ValueError("baseline and candidate strategies must differ")
    _validate_arm(
        baseline, label="baseline", expected_strategy=baseline_strategy
    )
    _validate_arm(
        candidate, label="candidate", expected_strategy=candidate_strategy
    )
    if baseline.get("model") != candidate.get("model"):
        raise ValueError("comparison arms used different models")
    if arm_order not in (
        ["baseline", "candidate"],
        ["candidate", "baseline"],
    ):
        raise ValueError("arm order must contain baseline and candidate exactly once")
    baseline_tasks = [
        trial.get("task_name") for trial in baseline.get("trials", [])
    ]
    candidate_tasks = [
        trial.get("task_name") for trial in candidate.get("trials", [])
    ]
    if baseline_tasks != candidate_tasks:
        raise ValueError("comparison arms used different task sequences")
    if (
        expected_task_sequence is not None
        and baseline_tasks != expected_task_sequence
    ):
        raise ValueError(
            "comparison task sequence did not match the expected learning order"
        )

    reward_names = sorted(
        set(baseline.get("rewards", {})) | set(candidate.get("rewards", {}))
    )
    reward_deltas = {}
    for name in reward_names:
        baseline_reward = baseline.get("rewards", {}).get(name, {}).get("mean")
        candidate_reward = candidate.get("rewards", {}).get(name, {}).get("mean")
        reward_deltas[name] = (
            candidate_reward - baseline_reward
            if isinstance(baseline_reward, (int, float))
            and isinstance(candidate_reward, (int, float))
            else None
        )
    baseline_task_rewards = _task_reward_means(baseline)
    candidate_task_rewards = _task_reward_means(candidate)
    task_reward_deltas = {
        task: {
            name: (
                candidate_task_rewards.get(task, {}).get(name)
                - baseline_task_rewards.get(task, {}).get(name)
                if isinstance(
                    candidate_task_rewards.get(task, {}).get(name), (int, float)
                )
                and isinstance(
                    baseline_task_rewards.get(task, {}).get(name), (int, float)
                )
                else None
            )
            for name in sorted(
                set(baseline_task_rewards.get(task, {}))
                | set(candidate_task_rewards.get(task, {}))
            )
        }
        for task in sorted(set(baseline_task_rewards) | set(candidate_task_rewards))
    }
    baseline_task_activations = _task_activation_counts(baseline)
    candidate_task_activations = _task_activation_counts(candidate)
    task_activation_deltas = {
        task: candidate_task_activations.get(task, 0)
        - baseline_task_activations.get(task, 0)
        for task in sorted(
            set(baseline_task_activations) | set(candidate_task_activations)
        )
    }

    route_action_deltas = {
        measure: {
            route: _route_action_count(candidate, route, measure)
            - _route_action_count(baseline, route, measure)
            for route in ROUTES
        }
        for measure in ("succeeded", "failed", "unresolved")
    }
    reuse_deltas = {
        metric: _reuse_count(candidate, metric) - _reuse_count(baseline, metric)
        for metric in REUSE_METRICS
    }
    lifecycle_deltas = {
        metric: _lifecycle_count(candidate, metric)
        - _lifecycle_count(baseline, metric)
        for metric in LIFECYCLE_METRICS
    }
    baseline_memory_growth = int(baseline.get("memory", {}).get("growth", 0))
    candidate_memory_growth = int(candidate.get("memory", {}).get("growth", 0))

    return {
        "schema_version": 3,
        "provider": provider,
        "model": baseline.get("model"),
        "source_digest": source_digest,
        "task_sequence": baseline_tasks,
        "arm_order": arm_order,
        "arms": {
            "baseline": {
                "strategy": baseline_strategy,
                "summary_path": str(baseline_summary_path),
                "rewards": baseline.get("rewards", {}),
                "task_rewards": baseline_task_rewards,
                "task_activations": baseline_task_activations,
                "memory": baseline.get("memory", {}),
                "lifecycle": baseline.get("lifecycle", {}),
                "route_counts": baseline.get("route_counts", {}),
                "task_reuse": baseline.get("task_reuse", {}),
            },
            "candidate": {
                "strategy": candidate_strategy,
                "summary_path": str(candidate_summary_path),
                "rewards": candidate.get("rewards", {}),
                "task_rewards": candidate_task_rewards,
                "task_activations": candidate_task_activations,
                "memory": candidate.get("memory", {}),
                "lifecycle": candidate.get("lifecycle", {}),
                "route_counts": candidate.get("route_counts", {}),
                "task_reuse": candidate.get("task_reuse", {}),
            },
        },
        "candidate_minus_baseline": {
            "reward_mean": reward_deltas,
            "task_reward_mean": task_reward_deltas,
            "task_activations": task_activation_deltas,
            "memory_growth": candidate_memory_growth - baseline_memory_growth,
            "lifecycle": lifecycle_deltas,
            "route_actions": route_action_deltas,
            "prior_task_reuse": reuse_deltas,
        },
    }


def _validate_arm(
    summary: dict[str, Any], *, label: str, expected_strategy: str
) -> None:
    if summary.get("schema_version") != 2:
        raise ValueError(f"{label} arm has an unsupported learning-summary schema")
    if summary.get("reflection_strategy") != expected_strategy:
        raise ValueError(
            f"{label} arm did not use the {expected_strategy} reflection strategy"
        )
    trials = summary.get("trials")
    trial_count = summary.get("trial_count")
    report_count = summary.get("reflection_report_count")
    if (
        not isinstance(trials, list)
        or not isinstance(trial_count, int)
        or trial_count <= 0
        or len(trials) != trial_count
    ):
        raise ValueError(f"{label} arm has inconsistent trial metadata")
    if not isinstance(report_count, int) or report_count != trial_count:
        raise ValueError(
            f"{label} arm has incomplete reflection reports: "
            f"{report_count!r} of {trial_count}"
        )


def _route_action_count(summary: dict[str, Any], route: str, measure: str) -> int:
    return int(summary.get("route_counts", {}).get(route, {}).get(measure, 0))


def _reuse_count(summary: dict[str, Any], metric: str) -> int:
    return int(summary.get("task_reuse", {}).get(metric, 0))


def _lifecycle_count(summary: dict[str, Any], metric: str) -> int:
    return int(summary.get("lifecycle", {}).get(metric, 0))


def _task_reward_means(summary: dict[str, Any]) -> dict[str, dict[str, float]]:
    values: dict[str, dict[str, list[float]]] = {}
    for trial in summary.get("trials", []):
        task = trial.get("task_name")
        if not isinstance(task, str):
            continue
        for name, value in trial.get("rewards", {}).items():
            if isinstance(name, str) and isinstance(value, (int, float)):
                values.setdefault(task, {}).setdefault(name, []).append(float(value))
    return {
        task: {
            name: sum(measurements) / len(measurements)
            for name, measurements in sorted(rewards.items())
        }
        for task, rewards in sorted(values.items())
    }


def _task_activation_counts(summary: dict[str, Any]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for trial in summary.get("trials", []):
        task = trial.get("task_name")
        if not isinstance(task, str):
            continue
        activated = (
            trial.get("task_usage", {}).get("learning_artifacts_activated", [])
        )
        counts[task] = counts.get(task, 0) + (
            len(activated) if isinstance(activated, list) else 0
        )
    return counts


def _run_eval(
    *,
    eval_script: Path,
    workspace: Path,
    exo_bin: Path,
    output_root: Path,
    log_path: Path,
    strategy: str,
    args: argparse.Namespace,
) -> Path:
    command = [
        sys.executable,
        str(eval_script),
        f"--dataset={args.dataset}",
        f"--provider={args.provider}",
        f"--model={args.model}",
        f"--n-attempts={args.n_attempts}",
        f"--reflection-strategy={strategy}",
        f"--workspace-root={workspace}",
        f"--exo-bin={exo_bin}",
        f"--output-root={output_root}",
        "--skip-build",
    ]
    if args.dataset_path is not None:
        command.append(f"--dataset-path={args.dataset_path.expanduser().resolve()}")
    if args.n_tasks is not None:
        command.append(f"--n-tasks={args.n_tasks}")
    for name in args.include_task_names:
        command.append(f"--include-task-name={name}")

    output_root.mkdir(parents=True)
    with log_path.open("w", encoding="utf-8") as log:
        process = subprocess.Popen(
            command,
            cwd=workspace,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        assert process.stdout is not None
        for line in process.stdout:
            print(f"[{strategy}] {line}", end="", flush=True)
            log.write(line)
            log.flush()
        return_code = process.wait()
    if return_code != 0:
        raise subprocess.CalledProcessError(return_code, command)

    summaries = list(output_root.glob("*/jobs/*/learning-summary.json"))
    if len(summaries) != 1:
        raise ValueError(
            f"expected one {strategy} learning summary, found {len(summaries)}"
        )
    return summaries[0].resolve()


def main() -> int:
    try:
        args = parse_args()
        if args.model == "openrouter/free":
            raise ValueError(
                "openrouter/free is not a fixed model; pass an exact "
                "OpenRouter model id"
            )
        if args.n_attempts <= 0 or (args.n_tasks is not None and args.n_tasks <= 0):
            raise ValueError("n_tasks and n_attempts must be positive")
        if args.baseline_strategy == args.candidate_strategy:
            raise ValueError("baseline and candidate strategies must differ")

        repo = Path(__file__).resolve().parents[2]
        exo_bin = (
            args.exo_bin.expanduser().resolve()
            if args.exo_bin is not None
            else repo / "target/debug/exo"
        )
        if not exo_bin.is_file():
            raise ValueError(f"prebuilt Exo binary not found: {exo_bin}")

        timestamp = dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")
        experiment_dir = repo / ".local/harbor-comparisons" / timestamp
        workspaces = experiment_dir / "workspaces"
        baseline_workspace = workspaces / "baseline"
        candidate_workspace = workspaces / "candidate"
        experiment_dir.mkdir(parents=True)

        print("Creating identical isolated source workspaces...", flush=True)
        source_digest = copy_source_snapshot(repo, baseline_workspace)
        shutil.copytree(baseline_workspace, candidate_workspace, symlinks=True)
        if workspace_digest(candidate_workspace) != source_digest:
            raise ValueError("isolated workspaces differ before evaluation")
        link_runtime_dependencies(repo, baseline_workspace)
        link_runtime_dependencies(repo, candidate_workspace)

        short_id = dt.datetime.now(dt.UTC).strftime("%H%M%S") + f"-{os.getpid()}"
        short_runs = repo / ".local/hr" / short_id
        order = (
            ["baseline", "candidate"]
            if args.arm_order == "baseline-first"
            else ["candidate", "baseline"]
        )
        strategy_by_arm = {
            "baseline": args.baseline_strategy,
            "candidate": args.candidate_strategy,
        }
        workspace_by_arm = {
            "baseline": baseline_workspace,
            "candidate": candidate_workspace,
        }
        # Darwin's sockaddr_un path limit is 104 bytes including the NUL.
        # One-letter arm directories leave enough room for Exo's trial socket.
        output_root_by_arm = {
            "baseline": short_runs / "b",
            "candidate": short_runs / "c",
        }
        summaries: dict[str, Path] = {}
        for arm in order:
            strategy = strategy_by_arm[arm]
            print(f"\n=== {arm} arm ({strategy}) ===", flush=True)
            summaries[arm] = _run_eval(
                eval_script=Path(__file__).with_name("eval.py"),
                workspace=workspace_by_arm[arm],
                exo_bin=exo_bin,
                output_root=output_root_by_arm[arm],
                log_path=experiment_dir / f"{arm}-{strategy}.log",
                strategy=strategy,
                args=args,
            )

        baseline_summary = json.loads(
            summaries["baseline"].read_text(encoding="utf-8")
        )
        candidate_summary = json.loads(
            summaries["candidate"].read_text(encoding="utf-8")
        )
        comparison = build_comparison(
            baseline=baseline_summary,
            candidate=candidate_summary,
            baseline_strategy=args.baseline_strategy,
            candidate_strategy=args.candidate_strategy,
            baseline_summary_path=summaries["baseline"],
            candidate_summary_path=summaries["candidate"],
            provider=args.provider,
            source_digest=source_digest,
            arm_order=order,
            expected_task_sequence=expected_task_sequence(args),
        )
        destination = experiment_dir / "comparison.json"
        destination.write_text(
            json.dumps(comparison, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(f"\nComparison: {destination}")
        print(
            json.dumps(
                comparison["candidate_minus_baseline"], indent=2, sort_keys=True
            )
        )
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("\nComparison stopped.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
