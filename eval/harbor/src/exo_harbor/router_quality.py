"""Score Harbor learning summaries against route-labeled tasks."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any


KEEP_ROUTES = ("memory", "skill", "tool", "policy")


def load_gold_labels(path: Path) -> dict[str, Any]:
    """Return the labeled transfer protocol for one dataset."""
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict) or not isinstance(payload.get("tasks"), dict):
        raise ValueError(f"gold labels are missing tasks: {path}")
    return payload


def score_router_quality(
    summary: dict[str, Any], gold: dict[str, Any]
) -> dict[str, Any]:
    """Compare one arm's persisted routes and reuse against gold labels."""
    tasks = gold.get("tasks", {})
    scored: list[dict[str, Any]] = []
    correct_routes = 0
    route_cases = 0
    useless = 0
    validated_reuse = 0
    missed_transfer = 0
    false_activation = 0
    held_out = 0
    held_out_cases = 0

    trials_by_task: dict[str, list[dict[str, Any]]] = {}
    for trial in summary.get("trials", []):
        task = trial.get("task_name")
        if isinstance(task, str):
            trials_by_task.setdefault(task, []).append(trial)

    for task_name, label in tasks.items():
        trial = (trials_by_task.get(task_name) or [None])[0]
        result = _score_task(trial, label if isinstance(label, dict) else {})
        scored.append({"task_name": task_name, **result})
        if result["route_scored"]:
            route_cases += 1
            if result["route_correct"]:
                correct_routes += 1
            if result["useless_artifact"]:
                useless += 1
        if result["phase"] == "transfer":
            held_out_cases += 1
            if result["validated_reuse"]:
                validated_reuse += 1
                held_out += 1
            if result["missed_transfer"]:
                missed_transfer += 1
        if result["phase"] == "control" and result["false_activation"]:
            false_activation += 1
        if result["phase"] == "control":
            held_out_cases += 1
            if not result["false_activation"]:
                held_out += 1

    return {
        "dataset": gold.get("dataset"),
        "route_accuracy": (correct_routes / route_cases) if route_cases else None,
        "correct_routes": correct_routes,
        "route_cases": route_cases,
        "useless_artifacts": useless,
        "validated_reuse": validated_reuse,
        "missed_transfer": missed_transfer,
        "false_activation": false_activation,
        "held_out_reward": (held_out / held_out_cases) if held_out_cases else None,
        "tasks": scored,
    }


def better_than_prompt_only(
    *, baseline: dict[str, Any], candidate: dict[str, Any]
) -> dict[str, Any]:
    """Return the published proof checks for a labeled comparison."""
    criteria = {
        "higher_route_accuracy": _greater(
            candidate.get("route_accuracy"), baseline.get("route_accuracy")
        ),
        "fewer_useless_artifacts": _less_or_equal(
            candidate.get("useless_artifacts"), baseline.get("useless_artifacts")
        ),
        "more_validated_reuse": _greater(
            candidate.get("validated_reuse"), baseline.get("validated_reuse")
        ),
        "equal_or_better_held_out_reward": _greater_or_equal(
            candidate.get("held_out_reward"), baseline.get("held_out_reward")
        ),
    }
    return {
        "success_criteria": criteria,
        "proven": all(criteria.values()),
        "candidate_minus_baseline": {
            "route_accuracy": _delta(
                candidate.get("route_accuracy"), baseline.get("route_accuracy")
            ),
            "useless_artifacts": _delta(
                candidate.get("useless_artifacts"),
                baseline.get("useless_artifacts"),
            ),
            "validated_reuse": _delta(
                candidate.get("validated_reuse"), baseline.get("validated_reuse")
            ),
            "held_out_reward": _delta(
                candidate.get("held_out_reward"), baseline.get("held_out_reward")
            ),
        },
    }


def _score_task(
    trial: dict[str, Any] | None, label: dict[str, Any]
) -> dict[str, Any]:
    phase = str(label.get("phase", "unknown"))
    accepted = {
        route
        for route in label.get("accepted_keep_routes", [])
        if isinstance(route, str)
    }
    incorrect = {
        route
        for route in label.get("incorrect_routes", [])
        if isinstance(route, str)
    }
    persisted = persisted_keep_routes(trial) if trial is not None else set()
    activated = activation_count(trial) if trial is not None else 0
    reused = prior_reuse_count(trial) if trial is not None else 0
    expected_activation = bool(label.get("expected_activation"))
    expected_reuse = bool(label.get("expected_prior_reuse"))
    route_scored = bool(accepted)
    route_correct = bool(persisted & accepted) and not (persisted & incorrect)
    useless_artifact = bool(persisted & incorrect)
    return {
        "phase": phase,
        "persisted_routes": sorted(persisted),
        "activation_count": activated,
        "prior_reuse_count": reused,
        "route_scored": route_scored,
        "route_correct": route_correct,
        "useless_artifact": useless_artifact,
        "validated_reuse": expected_reuse and reused > 0 and activated > 0,
        "missed_transfer": expected_activation and activated == 0 and reused == 0,
        "false_activation": (not expected_activation) and activated > 0,
    }


def persisted_keep_routes(trial: dict[str, Any]) -> set[str]:
    """Return durable keep routes from lifecycle promotions or prompt-only writes."""
    promotions = trial.get("lifecycle", {}).get("promotions")
    if isinstance(promotions, list) and promotions:
        return {
            str(item.get("route"))
            for item in promotions
            if isinstance(item, dict)
            and item.get("status") == "promoted"
            and item.get("route") in KEEP_ROUTES
        }
    routes: set[str] = set()
    for route, counts in trial.get("route_counts", {}).items():
        if (
            route in KEEP_ROUTES
            and isinstance(counts, dict)
            and int(counts.get("succeeded", 0)) > 0
        ):
            routes.add(str(route))
    return routes


def activation_count(trial: dict[str, Any]) -> int:
    activated = trial.get("task_usage", {}).get("learning_artifacts_activated", [])
    return len(activated) if isinstance(activated, list) else 0


def prior_reuse_count(trial: dict[str, Any]) -> int:
    usage = trial.get("task_usage", {})
    skills = usage.get("skills_reused_from_prior_tasks", [])
    tools = usage.get("agent_tools_reused_from_prior_tasks", [])
    return (len(skills) if isinstance(skills, list) else 0) + (
        len(tools) if isinstance(tools, list) else 0
    )


def _greater(candidate: Any, baseline: Any) -> bool:
    return isinstance(candidate, (int, float)) and isinstance(
        baseline, (int, float)
    ) and candidate > baseline


def _less(candidate: Any, baseline: Any) -> bool:
    return isinstance(candidate, (int, float)) and isinstance(
        baseline, (int, float)
    ) and candidate < baseline


def _less_or_equal(candidate: Any, baseline: Any) -> bool:
    return isinstance(candidate, (int, float)) and isinstance(
        baseline, (int, float)
    ) and candidate <= baseline


def _greater_or_equal(candidate: Any, baseline: Any) -> bool:
    return isinstance(candidate, (int, float)) and isinstance(
        baseline, (int, float)
    ) and candidate >= baseline


def _delta(candidate: Any, baseline: Any) -> float | None:
    if isinstance(candidate, (int, float)) and isinstance(baseline, (int, float)):
        return candidate - baseline
    return None
