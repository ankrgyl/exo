"""Measure durable learning actions taken during Harbor reflection."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any, Literal

from exo_harbor.exo import ExoClient
from exo_harbor.trajectory import (
    AssistantMessage,
    ConversationEvent,
    ConversationEvents,
    MessagesData,
    ToolCallContent,
    ToolResultData,
    UserMessage,
)

LearningRoute = Literal["memory", "skill", "tool", "policy"]
LEARNING_ROUTES = ("memory", "skill", "tool", "policy")
MEMORY_ARTIFACT_PATH = "memory/exo-memory.json"

TOOL_ROUTES: dict[str, LearningRoute] = {
    "remember": "memory",
    "forget": "memory",
    "install_skill": "skill",
    "uninstall_skill": "skill",
    "install_agent_tool": "tool",
    "uninstall_agent_tool": "tool",
    "manage_tool": "tool",
    "rebuild_and_restart_exo": "policy",
}


async def export_learning_report(
    client: ExoClient,
    conversation: str,
    trial_id: str,
    strategy: str,
    reflection_summary: str | None,
    destination: Path,
) -> None:
    """Write observable reflection mutations for one completed trial."""
    page = ConversationEvents.model_validate_json(
        await client.read_conversation_events(
            conversation,
            types=["messages", "tool_result"],
            limit=10_000,
        )
    )
    report = build_learning_report(
        events=page.events,
        trial_id=trial_id,
        conversation=conversation,
        strategy=strategy,
        reflection_summary=reflection_summary,
    )
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_suffix(destination.suffix + ".tmp")
    temporary.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    temporary.replace(destination)


def build_learning_report(
    *,
    events: list[ConversationEvent],
    trial_id: str,
    conversation: str,
    strategy: str,
    reflection_summary: str | None,
) -> dict[str, Any]:
    """Summarize persistence calls after the trial's feedback marker."""
    marker = f"Feedback for trial `{trial_id}`"
    start_index = next(
        (
            index
            for index, event in enumerate(events)
            if isinstance(event.data, MessagesData)
            and any(
                isinstance(message, UserMessage) and marker in message.content
                for message in event.data.messages
            )
        ),
        None,
    )
    if start_index is None:
        raise ValueError(f"Exo reflection for trial {trial_id} was not found")

    task_usage = _task_usage(events[:start_index])
    actions: list[dict[str, Any]] = []
    actions_by_call: dict[str, dict[str, Any]] = {}
    for event in events[start_index:]:
        if isinstance(event.data, MessagesData):
            for message in event.data.messages:
                if not isinstance(message, AssistantMessage):
                    continue
                for content in message.content:
                    if not isinstance(content, ToolCallContent):
                        continue
                    route = TOOL_ROUTES.get(content.tool_name)
                    if route is None:
                        continue
                    action = {
                        "route": route,
                        "tool": content.tool_name,
                        "tool_call_id": content.tool_call_id,
                        "detail": _action_detail(
                            content.tool_name, content.arguments.value
                        ),
                        "succeeded": None,
                    }
                    actions.append(action)
                    actions_by_call[content.tool_call_id] = action
            continue

        if not isinstance(event.data, ToolResultData):
            continue
        action = actions_by_call.get(event.data.tool_call_id)
        if action is None:
            continue
        result = event.data.result
        action["succeeded"] = _result_succeeded(result.ok, result.value)

    route_counts = {
        route: {
            "attempted": sum(action["route"] == route for action in actions),
            "succeeded": sum(
                action["route"] == route and action["succeeded"] is True
                for action in actions
            ),
            "failed": sum(
                action["route"] == route and action["succeeded"] is False
                for action in actions
            ),
            "unresolved": sum(
                action["route"] == route and action["succeeded"] is None
                for action in actions
            ),
        }
        for route in LEARNING_ROUTES
    }
    return {
        "schema_version": 2,
        "trial_id": trial_id,
        "conversation_id": conversation,
        "reflection_strategy": strategy,
        "reflection_summary": reflection_summary,
        "task_usage": task_usage,
        "route_counts": route_counts,
        "actions": actions,
    }


def _task_usage(events: list[ConversationEvent]) -> dict[str, list[dict[str, str]]]:
    """Return task use, distinguishing prior learning from same-task creation."""
    calls: dict[str, tuple[str, dict[str, Any]]] = {}
    skills: list[dict[str, str]] = []
    reused_skills: list[dict[str, str]] = []
    agent_tools: list[dict[str, str]] = []
    reused_agent_tools: list[dict[str, str]] = []
    skills_installed_during_task: set[str] = set()
    tools_installed_during_task: set[str] = set()
    for event in events:
        if isinstance(event.data, MessagesData):
            for message in event.data.messages:
                if not isinstance(message, AssistantMessage):
                    continue
                for content in message.content:
                    if isinstance(content, ToolCallContent):
                        calls[content.tool_call_id] = (
                            content.tool_name,
                            content.arguments.value,
                        )
            continue

        if not isinstance(event.data, ToolResultData):
            continue
        result = event.data.result
        if not _result_succeeded(result.ok, result.value):
            continue
        requested_name, arguments = calls.get(
            event.data.tool_call_id, (result.tool_name, {})
        )
        if requested_name == "install_skill":
            skill = _action_detail(requested_name, arguments)
            if skill is not None:
                skills_installed_during_task.add(skill)
            continue
        if requested_name == "install_agent_tool" and isinstance(result.value, dict):
            tool = _short_string(result.value.get("toolName"))
            if tool is not None:
                tools_installed_during_task.add(tool)
            continue
        if (
            requested_name == "manage_tool"
            and arguments.get("action") == "install"
            and isinstance(result.value, dict)
        ):
            installed = result.value.get("installed")
            tool = (
                _short_string(installed.get("name"))
                if isinstance(installed, dict)
                else None
            )
            if tool is not None:
                tools_installed_during_task.add(tool)
            continue
        if requested_name == "use_skill":
            skill = _short_string(arguments.get("name"))
            if skill is not None:
                use = {"name": skill, "tool_call_id": event.data.tool_call_id}
                skills.append(use)
                if skill not in skills_installed_during_task:
                    reused_skills.append(use)
        if result.source == "agent":
            use = {
                "name": result.tool_name,
                "tool_call_id": event.data.tool_call_id,
            }
            agent_tools.append(use)
            if result.tool_name not in tools_installed_during_task:
                reused_agent_tools.append(use)
    return {
        "skills_loaded": skills,
        "skills_reused_from_prior_tasks": reused_skills,
        "agent_tools_called": agent_tools,
        "agent_tools_reused_from_prior_tasks": reused_agent_tools,
    }


def _action_detail(tool_name: str, arguments: dict[str, Any]) -> str | None:
    """Return an identifier without copying source code into the report."""
    if tool_name == "remember":
        return _short_string(arguments.get("text"))
    if tool_name == "forget":
        return _short_string(arguments.get("id"))
    if tool_name == "install_skill":
        skill_md = arguments.get("skillMd")
        if isinstance(skill_md, str):
            match = re.search(
                r"""(?m)^name:\s*(?:"([a-z0-9-]+)"|'([a-z0-9-]+)'|([a-z0-9-]+))\s*$""",
                skill_md,
            )
            if match is not None:
                return next((group for group in match.groups() if group), None)
            return None
        return None
    if tool_name in {"uninstall_skill", "install_agent_tool", "uninstall_agent_tool"}:
        return _short_string(arguments.get("name"))
    if tool_name == "manage_tool":
        action = _short_string(arguments.get("action"))
        tool_id = _short_string(arguments.get("toolId"))
        source = arguments.get("source")
        source_path = (
            _short_string(source.get("path")) if isinstance(source, dict) else None
        )
        target = tool_id or source_path
        return ":".join(part for part in (action, target) if part) or None
    if tool_name == "rebuild_and_restart_exo":
        return _short_string(arguments.get("reason"))
    return None


def _short_string(value: Any, limit: int = 600) -> str | None:
    if not isinstance(value, str):
        return None
    value = value.strip()
    if not value:
        return None
    return value if len(value) <= limit else value[:limit] + "…"


def _result_succeeded(ok: bool, value: Any) -> bool:
    if isinstance(value, dict) and isinstance(value.get("ok"), bool):
        return ok and value["ok"]
    return ok


def export_job_learning_summary(
    *,
    job_dir: Path,
    exo_root: Path,
    strategy: str,
    model: str,
    trial_metadata: dict[str, dict[str, Any]],
) -> None:
    """Aggregate trial learning reports into one comparison-ready job file."""
    reports_by_trial = {}
    for path in sorted(job_dir.glob("*/agent/learning.json")):
        report = json.loads(path.read_text(encoding="utf-8"))
        reports_by_trial[path.parent.parent.name] = report
    reports = [
        {
            "trial_name": trial_name,
            "task_name": metadata["task_name"],
            "report": reports_by_trial.pop(trial_name, {}),
            "rewards": metadata["rewards"],
        }
        for trial_name, metadata in trial_metadata.items()
    ]
    reports.extend(
        {
            "trial_name": trial_name,
            "task_name": trial_name,
            "report": report,
            "rewards": {},
        }
        for trial_name, report in sorted(reports_by_trial.items())
    )
    summary = build_job_learning_summary(
        reports=reports,
        strategy=strategy,
        model=model,
        final_memory_count=read_final_memory_count(exo_root),
    )
    destination = job_dir / "learning-summary.json"
    temporary = destination.with_suffix(destination.suffix + ".tmp")
    temporary.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    temporary.replace(destination)


def build_job_learning_summary(
    *,
    reports: list[dict[str, Any]],
    strategy: str,
    model: str,
    final_memory_count: int,
) -> dict[str, Any]:
    """Return aggregate reward, persistence, and reuse measurements."""
    route_counts = {
        route: {
            measure: sum(
                entry["report"].get("route_counts", {}).get(route, {}).get(measure, 0)
                for entry in reports
            )
            for measure in ("attempted", "succeeded", "failed", "unresolved")
        }
        for route in LEARNING_ROUTES
    }
    skills_loaded = [
        skill
        for entry in reports
        for skill in entry["report"].get("task_usage", {}).get("skills_loaded", [])
    ]
    agent_tools_called = [
        tool
        for entry in reports
        for tool in entry["report"].get("task_usage", {}).get("agent_tools_called", [])
    ]
    skills_reused = [
        skill
        for entry in reports
        for skill in entry["report"]
        .get("task_usage", {})
        .get("skills_reused_from_prior_tasks", [])
    ]
    agent_tools_reused = [
        tool
        for entry in reports
        for tool in entry["report"]
        .get("task_usage", {})
        .get("agent_tools_reused_from_prior_tasks", [])
    ]
    reward_values: dict[str, list[float]] = {}
    for entry in reports:
        for name, value in entry["rewards"].items():
            reward_values.setdefault(name, []).append(float(value))
    rewards = {
        name: {
            "count": len(values),
            "mean": sum(values) / len(values),
        }
        for name, values in sorted(reward_values.items())
    }
    return {
        "schema_version": 2,
        "reflection_strategy": strategy,
        "model": model,
        "trial_count": len(reports),
        "reflection_report_count": sum(bool(entry["report"]) for entry in reports),
        "rewards": rewards,
        "memory": {
            "initial_count": 0,
            "final_count": final_memory_count,
            "growth": final_memory_count,
        },
        "route_counts": route_counts,
        "task_reuse": {
            "skill_load_count": len(skills_loaded),
            "agent_tool_call_count": len(agent_tools_called),
            "prior_skill_reuse_count": len(skills_reused),
            "prior_agent_tool_reuse_count": len(agent_tools_reused),
            "unique_skills_loaded": sorted(
                {
                    skill["name"]
                    for skill in skills_loaded
                    if isinstance(skill.get("name"), str)
                }
            ),
            "unique_agent_tools_called": sorted(
                {
                    tool["name"]
                    for tool in agent_tools_called
                    if isinstance(tool.get("name"), str)
                }
            ),
            "unique_prior_skills_reused": sorted(
                {
                    skill["name"]
                    for skill in skills_reused
                    if isinstance(skill.get("name"), str)
                }
            ),
            "unique_prior_agent_tools_reused": sorted(
                {
                    tool["name"]
                    for tool in agent_tools_reused
                    if isinstance(tool.get("name"), str)
                }
            ),
        },
        "trials": [
            {
                "trial_name": entry["trial_name"],
                "task_name": entry.get("task_name", entry["trial_name"]),
                "reflected": bool(entry["report"]),
                "trial_id": entry["report"].get("trial_id"),
                "rewards": entry["rewards"],
                "route_counts": entry["report"].get("route_counts", {}),
                "task_usage": entry["report"].get("task_usage", {}),
            }
            for entry in reports
        ],
    }


def read_final_memory_count(exo_root: Path) -> int:
    """Count entries in the newest durable-memory artifact without copying text."""
    candidates: list[tuple[str, int, Path]] = []
    agents_root = exo_root / "exoharness" / "agents"
    for metadata_path in agents_root.glob("*/artifacts/*/*.json"):
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
        if metadata.get("path") != MEMORY_ARTIFACT_PATH:
            continue
        created_at = metadata.get("created_at")
        if not isinstance(created_at, str):
            raise TypeError(f"memory artifact has no created_at: {metadata_path}")
        version = metadata.get("version")
        if not isinstance(version, int):
            raise TypeError(f"memory artifact has no version: {metadata_path}")
        candidates.append((created_at, version, metadata_path.with_suffix(".bin")))
    if not candidates:
        return 0
    _, _, memory_path = max(candidates)
    payload = json.loads(memory_path.read_text(encoding="utf-8"))
    entries = payload.get("entries")
    if not isinstance(entries, list):
        raise TypeError(f"memory artifact has no entries list: {memory_path}")
    return len(entries)
