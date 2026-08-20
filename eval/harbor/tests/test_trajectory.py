"""Tests for the Harbor trajectory export functionality.

Note: tests entierly AI generated but reviewed by human.
"""


import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock

from exo_harbor.trajectory import (
    ConversationEvents,
    build_trajectory,
    export_trial_trajectory,
)


TRIAL_ID = "514afdba-87e0-43cc-a3ac-86422ade3cf8"
TURN_ID = "019fde0e-d687-7be3-8771-d9f5a4309e0d"
CONTINUATION_TURN_ID = "019fde0e-e000-7000-8000-000000000001"


def event_page(events: list[dict]) -> str:
    return json.dumps({"events": events, "cursor": events[-1]["id"] if events else None})


def messages_event(
    event_id: str,
    messages: list[dict],
    *,
    turn_id: str = TURN_ID,
    usage: dict | None = None,
) -> dict:
    data = {"type": "messages", "messages": messages, "response_id": None}
    if usage is not None:
        data["usage"] = usage
    return {
        "id": event_id,
        "thread_id": "conversation-id",
        "session_id": "session-id",
        "turn_id": turn_id,
        "created_at": "2026-08-07T21:09:02.215Z",
        "data": data,
    }


class TrajectoryTest(unittest.IsolatedAsyncioTestCase):
    async def test_export_includes_trial_continuation_turns_and_writes_atif(self) -> None:
        start = messages_event(
            "event-2",
            [{"role": "user", "content": "Do the task"}],
        )
        assistant = messages_event(
            "event-3",
            [
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "reasoning",
                            "text": "Inspect the output.",
                            "encrypted_content": "ciphertext",
                        },
                        {
                            "type": "tool_call",
                            "tool_call_id": "call-1",
                            "tool_name": "shell",
                            "arguments": {
                                "type": "valid",
                                "value": {"command": "run tests"},
                            },
                        },
                    ],
                    "id": "response-item",
                }
            ],
            usage={
                "model": "gpt-5.5-2026-04-23",
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_cached_tokens": 80,
                "completion_reasoning_tokens": 10,
                "cost_usd": 0.02,
            },
        )
        tool_result = {
            "id": "event-4",
            "thread_id": "conversation-id",
            "session_id": "session-id",
            "turn_id": TURN_ID,
            "created_at": "2026-08-07T21:09:03.215Z",
            "data": {
                "type": "tool_result",
                "tool_call_id": "call-1",
                "result": {
                    "ok": True,
                    "preview": '{"stdout":"Smy Smy"}',
                    "source": "built_in",
                    "toolName": "shell",
                    "truncated": False,
                    "value": {"exit_code": 0, "stdout": "Smy Smy", "stderr": ""},
                },
            },
        }
        continuation = messages_event(
            "event-5",
            [
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "text",
                            "text": "Continued after restart.",
                        }
                    ],
                    "id": "continuation-response",
                }
            ],
            turn_id=CONTINUATION_TURN_ID,
        )
        client = AsyncMock()
        client.read_conversation_events.return_value = event_page(
            [start, assistant, tool_result, continuation]
        )

        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "agent" / "trajectory.json"
            await export_trial_trajectory(
                client,
                "harbor-shared",
                TRIAL_ID,
                "Do the task",
                "gpt-5.5",
                destination,
            )
            output = json.loads(destination.read_text())

        self.assertEqual(output["schema_version"], "ATIF-v1.7")
        self.assertEqual(output["trajectory_id"], TRIAL_ID)
        self.assertEqual(output["session_id"], "harbor-shared")
        self.assertEqual(output["steps"][0]["message"], "Do the task")
        self.assertEqual(len(output["steps"]), 3)
        agent_step = output["steps"][1]
        self.assertEqual(agent_step["reasoning_content"], "Inspect the output.")
        self.assertEqual(agent_step["tool_calls"][0]["function_name"], "shell")
        self.assertIn("Smy Smy", agent_step["observation"]["results"][0]["content"])
        self.assertEqual(output["final_metrics"]["total_prompt_tokens"], 100)
        self.assertEqual(output["steps"][2]["message"], "Continued after restart.")
        self.assertEqual(
            output["extra"]["exo_turn_ids"],
            [TURN_ID, CONTINUATION_TURN_ID],
        )
        self.assertNotIn("ciphertext", json.dumps(output))
        client.read_conversation_events.assert_awaited_once_with(
            "harbor-shared",
            types=["messages", "tool_result"],
            limit=10_000,
        )

    def test_first_step_is_harbors_instruction_not_the_sent_prompt(self) -> None:
        # Harbor's trajectory should read as the task it set, whatever wrapping
        # text the prompt actually carried.
        events = ConversationEvents.model_validate_json(
            event_page(
                [
                    messages_event(
                        "event-1",
                        [{"role": "user", "content": "prompt with extra framing"}],
                    )
                ]
            )
        ).events

        trajectory = build_trajectory(
            events=events,
            trial_id=TRIAL_ID,
            turn_ids=[TURN_ID],
            instruction="Do the task",
            model_name="gpt-5.5",
            conversation="harbor-shared",
            started_at="2026-08-07T21:09:02.215Z",
        )

        self.assertEqual(len(trajectory.steps), 1)
        self.assertEqual(trajectory.steps[0].message, "Do the task")


if __name__ == "__main__":
    unittest.main()
