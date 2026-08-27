import json
import tempfile
import unittest
from pathlib import Path

from exo_harbor.learning import (
    build_job_learning_summary,
    build_learning_report,
    export_job_learning_summary,
    read_final_memory_count,
)
from exo_harbor.trajectory import ConversationEvents

TRIAL_ID = "trial-1"


def messages_event(event_id: str, messages: list[dict]) -> dict:
    return {
        "id": event_id,
        "session_id": "session-1",
        "turn_id": "turn-1",
        "created_at": "2026-08-18T12:00:00Z",
        "data": {"type": "messages", "messages": messages},
    }


def tool_result_event(
    event_id: str,
    call_id: str,
    value: dict,
    *,
    tool_name: str = "test",
    source: str = "built_in",
) -> dict:
    return {
        "id": event_id,
        "session_id": "session-1",
        "turn_id": "turn-1",
        "created_at": "2026-08-18T12:00:01Z",
        "data": {
            "type": "tool_result",
            "tool_call_id": call_id,
            "result": {
                "ok": True,
                "preview": json.dumps(value),
                "source": source,
                "toolName": tool_name,
                "truncated": False,
                "value": value,
            },
        },
    }


def tool_call(call_id: str, name: str, arguments: dict) -> dict:
    return {
        "type": "tool_call",
        "tool_call_id": call_id,
        "tool_name": name,
        "arguments": {"type": "valid", "value": arguments},
    }


def learning_activated_event(event_id: str, artifact_id: str, route: str) -> dict:
    return {
        "id": event_id,
        "session_id": "session-1",
        "turn_id": "turn-1",
        "created_at": "2026-08-18T12:00:00Z",
        "data": {
            "type": "learning_activated",
            "artifacts": [
                {"id": artifact_id, "route": route, "title": "FLINT procedure"}
            ],
        },
    }


class LearningReportTest(unittest.TestCase):
    def test_reports_lifecycle_promotion_and_prior_activation(self) -> None:
        events = ConversationEvents.model_validate(
            {
                "events": [
                    learning_activated_event("event-0", "learn-old", "skill"),
                    messages_event(
                        "event-1",
                        [
                            {
                                "role": "user",
                                "content": f"Feedback for trial `{TRIAL_ID}` is ready.",
                            }
                        ],
                    ),
                    messages_event(
                        "event-2",
                        [
                            {
                                "role": "assistant",
                                "content": [
                                    tool_call(
                                        "propose-1",
                                        "propose_skill_learning",
                                        {"title": "FLINT procedure"},
                                    )
                                ],
                            }
                        ],
                    ),
                    tool_result_event(
                        "event-3",
                        "propose-1",
                        {
                            "ok": True,
                            "candidateId": "learn-new",
                            "route": "skill",
                            "status": "proposed",
                        },
                        tool_name="propose_skill_learning",
                    ),
                    messages_event(
                        "event-4",
                        [
                            {
                                "role": "assistant",
                                "content": [
                                    tool_call(
                                        "promote-1",
                                        "validate_and_promote_learning",
                                        {"candidateId": "learn-new"},
                                    )
                                ],
                            }
                        ],
                    ),
                    tool_result_event(
                        "event-5",
                        "promote-1",
                        {
                            "ok": True,
                            "candidateId": "learn-new",
                            "route": "skill",
                            "status": "promoted",
                        },
                        tool_name="validate_and_promote_learning",
                    ),
                ]
            }
        ).events

        report = build_learning_report(
            events=events,
            trial_id=TRIAL_ID,
            conversation="conversation-1",
            strategy="lifecycle",
            reflection_summary="promoted validated skill",
        )

        self.assertEqual(report["lifecycle"]["proposal_count"], 1)
        self.assertEqual(report["lifecycle"]["promotion_count"], 1)
        self.assertEqual(report["route_counts"]["skill"]["succeeded"], 1)
        self.assertEqual(
            report["task_usage"]["learning_artifacts_activated"],
            [{"id": "learn-old", "route": "skill", "title": "FLINT procedure"}],
        )

    def test_reports_proposal_that_was_never_promoted_as_unresolved(self) -> None:
        events = ConversationEvents.model_validate(
            {
                "events": [
                    messages_event(
                        "event-1",
                        [
                            {
                                "role": "user",
                                "content": f"Feedback for trial `{TRIAL_ID}` is ready.",
                            }
                        ],
                    ),
                    messages_event(
                        "event-2",
                        [
                            {
                                "role": "assistant",
                                "content": [
                                    tool_call(
                                        "propose-1",
                                        "propose_memory_learning",
                                        {"title": "Unfinished candidate"},
                                    )
                                ],
                            }
                        ],
                    ),
                    tool_result_event(
                        "event-3",
                        "propose-1",
                        {
                            "ok": True,
                            "candidateId": "learn-unresolved",
                            "route": "memory",
                            "status": "proposed",
                        },
                        tool_name="propose_memory_learning",
                    ),
                ]
            }
        ).events

        report = build_learning_report(
            events=events,
            trial_id=TRIAL_ID,
            conversation="conversation-1",
            strategy="lifecycle",
            reflection_summary="forgot to promote",
        )

        self.assertEqual(report["lifecycle"]["proposal_count"], 1)
        self.assertEqual(report["lifecycle"]["promotion_count"], 0)
        self.assertEqual(report["lifecycle"]["unresolved_count"], 1)

    def test_job_summary_aggregates_rewards_routes_reuse_and_memory(self) -> None:
        reports = [
            {
                "trial_name": "task-1",
                "rewards": {"reward": 1.0},
                "report": {
                    "trial_id": "trial-1",
                    "route_counts": {
                        "memory": {
                            "attempted": 2,
                            "succeeded": 1,
                            "failed": 1,
                            "unresolved": 0,
                        },
                        "skill": {
                            "attempted": 1,
                            "succeeded": 1,
                            "failed": 0,
                            "unresolved": 0,
                        },
                    },
                    "task_usage": {
                        "skills_loaded": [],
                        "skills_reused_from_prior_tasks": [],
                        "agent_tools_called": [],
                        "agent_tools_reused_from_prior_tasks": [],
                    },
                },
            },
            {
                "trial_name": "task-2",
                "rewards": {"reward": 0.0},
                "report": {
                    "trial_id": "trial-2",
                    "route_counts": {
                        "memory": {
                            "attempted": 1,
                            "succeeded": 1,
                            "failed": 0,
                            "unresolved": 0,
                        },
                        "tool": {
                            "attempted": 1,
                            "succeeded": 1,
                            "failed": 0,
                            "unresolved": 0,
                        },
                    },
                    "task_usage": {
                        "skills_loaded": [
                            {"name": "debug-copy", "tool_call_id": "skill-1"}
                        ],
                        "skills_reused_from_prior_tasks": [
                            {"name": "debug-copy", "tool_call_id": "skill-1"}
                        ],
                        "agent_tools_called": [
                            {"name": "copy_check", "tool_call_id": "tool-1"}
                        ],
                        "agent_tools_reused_from_prior_tasks": [
                            {"name": "copy_check", "tool_call_id": "tool-1"}
                        ],
                    },
                },
            },
        ]

        summary = build_job_learning_summary(
            reports=reports,
            strategy="router",
            model="fixed-model",
            final_memory_count=2,
        )

        self.assertEqual(summary["model"], "fixed-model")
        self.assertEqual(summary["rewards"]["reward"], {"count": 2, "mean": 0.5})
        self.assertEqual(
            summary["route_counts"]["memory"],
            {"attempted": 3, "succeeded": 2, "failed": 1, "unresolved": 0},
        )
        self.assertEqual(summary["route_counts"]["policy"]["attempted"], 0)
        self.assertEqual(
            summary["memory"],
            {"initial_count": 0, "final_count": 2, "growth": 2},
        )
        self.assertEqual(summary["task_reuse"]["skill_load_count"], 1)
        self.assertEqual(summary["task_reuse"]["prior_skill_reuse_count"], 1)
        self.assertEqual(
            summary["task_reuse"]["unique_agent_tools_called"], ["copy_check"]
        )
        self.assertEqual(
            summary["task_reuse"]["unique_prior_agent_tools_reused"],
            ["copy_check"],
        )

    def test_reads_newest_durable_memory_and_exports_job_summary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            exo_root = root / "exo"
            artifacts = exo_root / "exoharness" / "agents" / "agent-1" / "artifacts"
            for artifact_id, created_at, version, entries in (
                ("old", "2026-08-18T12:00:00Z", 1, [{"id": "one"}]),
                (
                    "new",
                    "2026-08-18T13:00:00Z",
                    2,
                    [{"id": "one"}, {"id": "two"}],
                ),
            ):
                artifact = artifacts / artifact_id
                artifact.mkdir(parents=True)
                (artifact / f"{version}.json").write_text(
                    json.dumps(
                        {
                            "path": "memory/exo-memory.json",
                            "created_at": created_at,
                            "version": version,
                        }
                    ),
                    encoding="utf-8",
                )
                (artifact / f"{version}.bin").write_text(
                    json.dumps({"entries": entries}), encoding="utf-8"
                )
            job_dir = root / "job"
            report_dir = job_dir / "task-1" / "agent"
            report_dir.mkdir(parents=True)
            (report_dir / "learning.json").write_text(
                json.dumps(
                    {
                        "trial_id": "trial-1",
                        "route_counts": {"memory": {"attempted": 1, "succeeded": 1}},
                        "task_usage": {
                            "skills_loaded": [],
                            "skills_reused_from_prior_tasks": [],
                            "agent_tools_called": [],
                            "agent_tools_reused_from_prior_tasks": [],
                        },
                    }
                ),
                encoding="utf-8",
            )

            self.assertEqual(read_final_memory_count(exo_root), 2)
            export_job_learning_summary(
                job_dir=job_dir,
                exo_root=exo_root,
                strategy="router",
                model="fixed-model",
                trial_metadata={
                    "task-1": {
                        "task_name": "first-task",
                        "rewards": {"reward": 1.0},
                    },
                    "timed-out-task": {
                        "task_name": "timeout-task",
                        "rewards": {"reward": 0.0},
                    },
                },
            )
            summary = json.loads(
                (job_dir / "learning-summary.json").read_text(encoding="utf-8")
            )

        self.assertEqual(summary["memory"]["growth"], 2)
        self.assertEqual(summary["rewards"]["reward"]["mean"], 0.5)
        self.assertEqual(summary["trial_count"], 2)
        self.assertEqual(summary["reflection_report_count"], 1)
        self.assertEqual(summary["trials"][0]["task_name"], "first-task")
        self.assertFalse(summary["trials"][1]["reflected"])

    def test_same_task_creation_is_not_counted_as_prior_task_reuse(self) -> None:
        skill_md = (
            '---\nname: "inspect-copy"\ndescription: Check copied files.\n---\nDo it.'
        )
        events = ConversationEvents.model_validate(
            {
                "events": [
                    messages_event(
                        "event-1",
                        [
                            {
                                "role": "assistant",
                                "content": [
                                    tool_call(
                                        "install-tool",
                                        "install_agent_tool",
                                        {"name": "copy-checker"},
                                    ),
                                    tool_call(
                                        "install-skill",
                                        "install_skill",
                                        {"skillMd": skill_md, "files": None},
                                    ),
                                ],
                            }
                        ],
                    ),
                    tool_result_event(
                        "event-2",
                        "install-tool",
                        {"ok": True, "toolName": "copy_checker"},
                        tool_name="install_agent_tool",
                    ),
                    tool_result_event(
                        "event-3",
                        "install-skill",
                        {"ok": True, "name": "inspect-copy"},
                        tool_name="install_skill",
                    ),
                    messages_event(
                        "event-4",
                        [
                            {
                                "role": "assistant",
                                "content": [
                                    tool_call("use-tool", "copy_checker", {}),
                                    tool_call(
                                        "use-skill",
                                        "use_skill",
                                        {"name": "inspect-copy"},
                                    ),
                                ],
                            }
                        ],
                    ),
                    tool_result_event(
                        "event-5",
                        "use-tool",
                        {"ok": True},
                        tool_name="copy_checker",
                        source="agent",
                    ),
                    tool_result_event(
                        "event-6",
                        "use-skill",
                        {"ok": True},
                        tool_name="use_skill",
                    ),
                    messages_event(
                        "event-7",
                        [
                            {
                                "role": "user",
                                "content": f"Feedback for trial `{TRIAL_ID}` is ready.",
                            }
                        ],
                    ),
                ]
            }
        ).events

        report = build_learning_report(
            events=events,
            trial_id=TRIAL_ID,
            conversation="conversation-1",
            strategy="router",
            reflection_summary=None,
        )

        self.assertEqual(len(report["task_usage"]["skills_loaded"]), 1)
        self.assertEqual(len(report["task_usage"]["agent_tools_called"]), 1)
        self.assertEqual(report["task_usage"]["skills_reused_from_prior_tasks"], [])
        self.assertEqual(
            report["task_usage"]["agent_tools_reused_from_prior_tasks"], []
        )

    def test_counts_scoped_lifecycle_skill_load_as_prior_reuse(self) -> None:
        events = ConversationEvents.model_validate(
            {
                "events": [
                    learning_activated_event("event-0", "learn-skill", "skill"),
                    messages_event(
                        "event-1",
                        [
                            {
                                "role": "assistant",
                                "content": [
                                    tool_call(
                                        "use-learning-skill",
                                        "use_learning_skill",
                                        {"candidateId": "learn-skill"},
                                    )
                                ],
                            }
                        ],
                    ),
                    tool_result_event(
                        "event-2",
                        "use-learning-skill",
                        {
                            "ok": True,
                            "candidateId": "learn-skill",
                            "name": "flint-procedure",
                        },
                        tool_name="use_learning_skill",
                        source="library",
                    ),
                    messages_event(
                        "event-3",
                        [
                            {
                                "role": "user",
                                "content": f"Feedback for trial `{TRIAL_ID}` is ready.",
                            }
                        ],
                    ),
                ]
            }
        ).events

        report = build_learning_report(
            events=events,
            trial_id=TRIAL_ID,
            conversation="conversation-1",
            strategy="lifecycle",
            reflection_summary=None,
        )

        self.assertEqual(
            report["task_usage"]["skills_reused_from_prior_tasks"],
            [
                {
                    "name": "flint-procedure",
                    "tool_call_id": "use-learning-skill",
                    "candidate_id": "learn-skill",
                }
            ],
        )

    def test_reports_only_persistence_actions_during_reflection(self) -> None:
        events = ConversationEvents.model_validate(
            {
                "events": [
                    messages_event(
                        "event-1",
                        [
                            {
                                "role": "assistant",
                                "content": [
                                    tool_call(
                                        "before-feedback",
                                        "remember",
                                        {"text": "ignore task-phase memory"},
                                    ),
                                    tool_call(
                                        "skill-use",
                                        "use_skill",
                                        {"name": "inspect-copy"},
                                    ),
                                    tool_call(
                                        "agent-tool-use",
                                        "copy_checker",
                                        {"path": "/app/output"},
                                    ),
                                ],
                            }
                        ],
                    ),
                    tool_result_event(
                        "event-1a",
                        "skill-use",
                        {"ok": True},
                        tool_name="use_skill",
                    ),
                    tool_result_event(
                        "event-1b",
                        "agent-tool-use",
                        {"ok": True},
                        tool_name="copy_checker",
                        source="agent",
                    ),
                    messages_event(
                        "event-2",
                        [
                            {
                                "role": "user",
                                "content": f"Feedback for trial `{TRIAL_ID}` is ready.",
                            }
                        ],
                    ),
                    messages_event(
                        "event-3",
                        [
                            {
                                "role": "assistant",
                                "content": [
                                    tool_call(
                                        "memory-1",
                                        "remember",
                                        {"text": "Check file modes after copying."},
                                    ),
                                    tool_call(
                                        "skill-1",
                                        "install_skill",
                                        {
                                            "skillMd": "---\nname: inspect-copy\ndescription: Check copied files.\n---\nDo it.",
                                            "files": None,
                                        },
                                    ),
                                    tool_call(
                                        "tool-1",
                                        "install_agent_tool",
                                        {
                                            "name": "copy-checker",
                                            "moduleSource": "SECRETLY LARGE SOURCE",
                                            "initialization": {"apiKeyEnv": None},
                                        },
                                    ),
                                    tool_call(
                                        "policy-1",
                                        "rebuild_and_restart_exo",
                                        {"reason": "activate safer copy policy"},
                                    ),
                                    tool_call("shell-1", "shell", {"command": "true"}),
                                ],
                            }
                        ],
                    ),
                    tool_result_event("event-4", "memory-1", {"ok": True}),
                    tool_result_event("event-5", "skill-1", {"ok": False}),
                    tool_result_event("event-6", "tool-1", {"ok": True}),
                ]
            }
        ).events

        report = build_learning_report(
            events=events,
            trial_id=TRIAL_ID,
            conversation="conversation-1",
            strategy="router",
            reflection_summary="created reusable learning",
        )

        self.assertEqual(report["reflection_strategy"], "router")
        self.assertEqual(
            report["task_usage"],
            {
                "skills_loaded": [
                    {"name": "inspect-copy", "tool_call_id": "skill-use"}
                ],
                "skills_reused_from_prior_tasks": [
                    {"name": "inspect-copy", "tool_call_id": "skill-use"}
                ],
                "agent_tools_called": [
                    {"name": "copy_checker", "tool_call_id": "agent-tool-use"}
                ],
                "agent_tools_reused_from_prior_tasks": [
                    {"name": "copy_checker", "tool_call_id": "agent-tool-use"}
                ],
                "learning_artifacts_activated": [],
            },
        )
        self.assertEqual(len(report["actions"]), 4)
        self.assertEqual(
            report["route_counts"],
            {
                "memory": {
                    "attempted": 1,
                    "succeeded": 1,
                    "failed": 0,
                    "unresolved": 0,
                },
                "skill": {
                    "attempted": 1,
                    "succeeded": 0,
                    "failed": 1,
                    "unresolved": 0,
                },
                "tool": {
                    "attempted": 1,
                    "succeeded": 1,
                    "failed": 0,
                    "unresolved": 0,
                },
                "policy": {
                    "attempted": 1,
                    "succeeded": 0,
                    "failed": 0,
                    "unresolved": 1,
                },
            },
        )
        self.assertEqual(report["actions"][1]["detail"], "inspect-copy")
        self.assertEqual(report["actions"][2]["detail"], "copy-checker")
        self.assertNotIn("SECRETLY LARGE SOURCE", json.dumps(report))
        self.assertIsNone(report["actions"][3]["succeeded"])

    def test_requires_feedback_marker(self) -> None:
        with self.assertRaisesRegex(ValueError, "reflection.*was not found"):
            build_learning_report(
                events=[],
                trial_id=TRIAL_ID,
                conversation="conversation-1",
                strategy="router",
                reflection_summary=None,
            )


if __name__ == "__main__":
    unittest.main()
