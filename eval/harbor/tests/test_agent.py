from __future__ import annotations

import unittest
from pathlib import Path
from types import SimpleNamespace
from uuid import uuid4

from unittest.mock import AsyncMock, patch

from exo_harbor.agent import ExoAgent, get_harbor_docker_container_id


def build_agent(logs_dir: Path = Path("/tmp/logs")) -> ExoAgent:
    """Construct ExoAgent the way Harbor's AgentFactory does."""
    return ExoAgent(
        logs_dir=logs_dir,
        exo_root="/runs/one/exo",
        exo_bin="/repo/target/debug/exo",
        exo_repo_root="/repo",
        exo_model="gpt-5.5",
    )


class ExoAgentTest(unittest.TestCase):

    def test_conversation_slug_follows_the_trial_not_construction(self) -> None:
        # Harbor assigns context_id after building the agent. Reading it too
        # early would give every trial the same conversation, silently removing
        # the isolation between trials.
        agent = build_agent()
        with self.assertRaises(AssertionError):
            agent._conversation

        first = uuid4()
        agent.context_id = first
        self.assertEqual(agent._conversation, f"trial-{first}")

        second = uuid4()
        agent.context_id = second
        self.assertEqual(agent._conversation, f"trial-{second}")

class SetupTest(unittest.IsolatedAsyncioTestCase):
    async def test_attaches_harbors_container_to_this_trials_conversation(self) -> None:
        agent = build_agent()
        agent.context_id = uuid4()
        agent._client = AsyncMock()
        agent._client.attach_container.return_value = "sandbox-1"

        with patch(
            "exo_harbor.agent.get_harbor_docker_container_id", return_value="abc123"
        ) as lookup:
            await agent.setup(SimpleNamespace(session_id="session-1"))

        lookup.assert_called_once_with("session-1")
        self.assertEqual(agent._container_id, "abc123")
        # The conversation must exist before anything is attached to it.
        agent._client.ensure_conversation.assert_awaited_once_with(
            f"trial-{agent.context_id}"
        )
        agent._client.attach_container.assert_awaited_once_with(
            f"trial-{agent.context_id}", "abc123"
        )

    async def test_a_missing_container_fails_the_trial(self) -> None:
        # Better to fail here than to open a conversation with no machine
        # behind it and grade whatever the shell happens to reach.
        agent = build_agent()
        agent.context_id = uuid4()
        agent._client = AsyncMock()

        with patch(
            "exo_harbor.agent.get_harbor_docker_container_id",
            side_effect=RuntimeError("no running main container"),
        ):
            with self.assertRaises(RuntimeError):
                await agent.setup(SimpleNamespace(session_id="session-1"))

        agent._client.ensure_conversation.assert_not_awaited()


class RunTest(unittest.IsolatedAsyncioTestCase):
    def build_ready_agent(self) -> ExoAgent:
        agent = build_agent()
        agent.context_id = uuid4()
        agent._container_id = "abc123"
        agent._client = AsyncMock()
        agent._client.snapshot_sandbox.return_value = "snapshot-1"
        return agent

    async def test_sends_the_instruction_then_hands_off_for_reflection(self) -> None:
        agent = self.build_ready_agent()
        context = SimpleNamespace(metadata={"existing": "kept"})

        with patch("exo_harbor.agent.export_trial_trajectory", AsyncMock()) as export:
            await agent.run("do the thing", SimpleNamespace(), context)

        agent._client.send.assert_awaited_once_with(
            f"trial-{agent.context_id}", "do the thing", timeout_sec=None
        )
        self.assertEqual(
            context.metadata,
            {
                "existing": "kept",
                "exo_conversation_id": f"trial-{agent.context_id}",
                "exo_snapshot_id": "snapshot-1",
                "exo_instruction": "do the thing",
            },
        )
        export.assert_awaited_once()

    async def test_a_timeout_still_snapshots_and_hands_off(self) -> None:
        # The trial is over, but the container is alive until the verifier
        # finishes -- this is the last chance to capture it for reflection.
        agent = self.build_ready_agent()
        agent._client.send.side_effect = TimeoutError("task timeout")
        context = SimpleNamespace(metadata=None)

        with patch("exo_harbor.agent.export_trial_trajectory", AsyncMock()) as export:
            with self.assertRaises(TimeoutError):
                await agent.run("do the thing", SimpleNamespace(), context)

        agent._client.snapshot_sandbox.assert_awaited_once()
        self.assertEqual(context.metadata["exo_snapshot_id"], "snapshot-1")
        export.assert_awaited_once()

    async def test_a_failed_snapshot_does_not_mask_the_real_error(self) -> None:
        # Raising from the finally would replace the timeout and Harbor would
        # record the wrong reason for the failure.
        agent = self.build_ready_agent()
        agent._client.send.side_effect = TimeoutError("task timeout")
        agent._client.snapshot_sandbox.side_effect = RuntimeError("no sandbox")
        context = SimpleNamespace(metadata=None)

        with patch("exo_harbor.agent.export_trial_trajectory", AsyncMock()):
            with self.assertRaises(TimeoutError):
                await agent.run("do the thing", SimpleNamespace(), context)

        # No snapshot id means the plugin skips reflection rather than trying
        # to restore something that does not exist.
        self.assertIsNone(context.metadata["exo_snapshot_id"])

    async def test_a_failed_trajectory_export_does_not_fail_the_trial(self) -> None:
        agent = self.build_ready_agent()
        context = SimpleNamespace(metadata=None)

        with patch(
            "exo_harbor.agent.export_trial_trajectory",
            AsyncMock(side_effect=ValueError("no events")),
        ):
            await agent.run("do the thing", SimpleNamespace(), context)

        self.assertEqual(context.metadata["exo_snapshot_id"], "snapshot-1")


class HarborContainerLookupTest(unittest.TestCase):
    """The Compose project name mirrors Harbor's own normalization by hand.

    Recheck these on a Harbor bump: if the rules drift, the filter silently
    matches nothing and every trial fails to find its container.
    """

    def resolve(self, session_id: str, output: str = "abc123") -> str:
        with patch("exo_harbor.agent._docker", return_value=output) as docker:
            container_id = get_harbor_docker_container_id(session_id)
        self.args = docker.call_args.args
        return container_id

    def project_filter(self) -> str:
        return next(
            arg
            for arg in self.args
            if arg.startswith("label=com.docker.compose.project=")
        ).removeprefix("label=com.docker.compose.project=")

    def test_lowercases_and_replaces_disallowed_characters(self) -> None:
        self.resolve("Trial.ID:42")
        self.assertEqual(self.project_filter(), "trial-id-42")

    def test_refuses_to_guess_between_multiple_containers(self) -> None:
        # Submitting an arbitrary one would silently grade the wrong machine.
        with self.assertRaises(RuntimeError):
            self.resolve("session-1", output="abc123 def456")

    def test_missing_container_is_an_error(self) -> None:
        with self.assertRaises(RuntimeError):
            self.resolve("session-1", output="")


if __name__ == "__main__":
    unittest.main()
