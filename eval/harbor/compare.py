#!/usr/bin/env python3
"""Run contamination-safe memory-vs-router Harbor evaluation arms."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


ROUTES = ("memory", "skill", "tool", "policy")
REUSE_METRICS = (
    "prior_skill_reuse_count",
    "prior_agent_tool_reuse_count",
)
EXPECTED_TASK_SEQUENCES = {
    "learning-router-transfer-test": [
        "exo/learning-router-normalization-first",
        "exo/learning-router-normalization-transfer",
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
        description="Run identical Harbor tasks with PR #202 memory reflection and the learning router.",
    )
    parser.add_argument("--dataset", default="learning-router-transfer-test")
    parser.add_argument("--dataset-path", type=Path)
    parser.add_argument("--provider", choices=("openai", "openrouter"), default="openai")
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
        "--arm-order",
        choices=("memory-first", "router-first"),
        default="memory-first",
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


def build_comparison(
    *,
    memory: dict[str, Any],
    router: dict[str, Any],
    memory_summary_path: Path,
    router_summary_path: Path,
    provider: str,
    source_digest: str,
    arm_order: list[str],
    expected_task_sequence: list[str] | None = None,
) -> dict[str, Any]:
    """Validate matched arms and return router-minus-memory measurements."""
    _validate_arm(memory, label="memory", expected_strategy="memory")
    _validate_arm(router, label="router", expected_strategy="router")
    if memory.get("model") != router.get("model"):
        raise ValueError("comparison arms used different models")
    if arm_order not in (["memory", "router"], ["router", "memory"]):
        raise ValueError("arm order must contain memory and router exactly once")
    memory_tasks = [trial.get("task_name") for trial in memory.get("trials", [])]
    router_tasks = [trial.get("task_name") for trial in router.get("trials", [])]
    if memory_tasks != router_tasks:
        raise ValueError("comparison arms used different task sequences")
    if expected_task_sequence is not None and memory_tasks != expected_task_sequence:
        raise ValueError(
            "comparison task sequence did not match the expected learning order"
        )

    reward_names = sorted(
        set(memory.get("rewards", {})) | set(router.get("rewards", {}))
    )
    reward_deltas = {}
    for name in reward_names:
        memory_reward = memory.get("rewards", {}).get(name, {}).get("mean")
        router_reward = router.get("rewards", {}).get(name, {}).get("mean")
        reward_deltas[name] = (
            router_reward - memory_reward
            if isinstance(memory_reward, (int, float))
            and isinstance(router_reward, (int, float))
            else None
        )

    route_action_deltas = {
        measure: {
            route: _route_action_count(router, route, measure)
            - _route_action_count(memory, route, measure)
            for route in ROUTES
        }
        for measure in ("succeeded", "failed", "unresolved")
    }
    reuse_deltas = {
        metric: _reuse_count(router, metric) - _reuse_count(memory, metric)
        for metric in REUSE_METRICS
    }
    memory_growth = int(memory.get("memory", {}).get("growth", 0))
    router_growth = int(router.get("memory", {}).get("growth", 0))

    return {
        "schema_version": 2,
        "provider": provider,
        "model": memory.get("model"),
        "source_digest": source_digest,
        "task_sequence": memory_tasks,
        "arm_order": arm_order,
        "arms": {
            "memory": {
                "summary_path": str(memory_summary_path),
                "rewards": memory.get("rewards", {}),
                "memory": memory.get("memory", {}),
                "route_counts": memory.get("route_counts", {}),
                "task_reuse": memory.get("task_reuse", {}),
            },
            "router": {
                "summary_path": str(router_summary_path),
                "rewards": router.get("rewards", {}),
                "memory": router.get("memory", {}),
                "route_counts": router.get("route_counts", {}),
                "task_reuse": router.get("task_reuse", {}),
            },
        },
        "router_minus_memory": {
            "reward_mean": reward_deltas,
            "memory_growth": router_growth - memory_growth,
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
                "openrouter/free is not a fixed model; pass an exact OpenRouter model id"
            )
        if args.n_attempts <= 0 or (args.n_tasks is not None and args.n_tasks <= 0):
            raise ValueError("n_tasks and n_attempts must be positive")

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
        memory_workspace = workspaces / "memory"
        router_workspace = workspaces / "router"
        experiment_dir.mkdir(parents=True)

        print("Creating identical isolated source workspaces...", flush=True)
        source_digest = copy_source_snapshot(repo, memory_workspace)
        shutil.copytree(memory_workspace, router_workspace, symlinks=True)
        if workspace_digest(router_workspace) != source_digest:
            raise ValueError("isolated workspaces differ before evaluation")
        link_runtime_dependencies(repo, memory_workspace)
        link_runtime_dependencies(repo, router_workspace)

        short_id = dt.datetime.now(dt.UTC).strftime("%H%M%S") + f"-{os.getpid()}"
        short_runs = repo / ".local/hr" / short_id
        order = (
            ["memory", "router"]
            if args.arm_order == "memory-first"
            else ["router", "memory"]
        )
        workspace_by_strategy = {
            "memory": memory_workspace,
            "router": router_workspace,
        }
        summaries: dict[str, Path] = {}
        for strategy in order:
            print(f"\n=== {strategy} arm ===", flush=True)
            summaries[strategy] = _run_eval(
                eval_script=Path(__file__).with_name("eval.py"),
                workspace=workspace_by_strategy[strategy],
                exo_bin=exo_bin,
                output_root=short_runs / strategy,
                log_path=experiment_dir / f"{strategy}.log",
                strategy=strategy,
                args=args,
            )

        memory_summary = json.loads(summaries["memory"].read_text(encoding="utf-8"))
        router_summary = json.loads(summaries["router"].read_text(encoding="utf-8"))
        comparison = build_comparison(
            memory=memory_summary,
            router=router_summary,
            memory_summary_path=summaries["memory"],
            router_summary_path=summaries["router"],
            provider=args.provider,
            source_digest=source_digest,
            arm_order=order,
            expected_task_sequence=(
                EXPECTED_TASK_SEQUENCES[args.dataset] * args.n_attempts
                if args.dataset in EXPECTED_TASK_SEQUENCES
                else None
            ),
        )
        destination = experiment_dir / "comparison.json"
        destination.write_text(
            json.dumps(comparison, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(f"\nComparison: {destination}")
        print(json.dumps(comparison["router_minus_memory"], indent=2, sort_keys=True))
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("\nComparison stopped.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
