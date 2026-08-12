import asyncio
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import AsyncMock, MagicMock, patch
from uuid import uuid4

from exo_harbor.agent import ExoAgent
from exo_harbor.protocol import TrialCancelled, TrialStarted


class ExoAgentTest(unittest.IsolatedAsyncioTestCase):
    async def test_timeout_exports_partial_trajectory_after_trial_started(self) -> None:
        with TemporaryDirectory() as directory:
            agent = ExoAgent(
                logs_dir=Path(directory),
                exo_root=Path(directory) / "exo-root",
                exo_bin=Path("/opt/exo/bin/exo"),
                exo_repo_root=Path("/src/exo"),
                exo_model="gpt-5.6-sol",
            )
            agent.context_id = uuid4()
            agent._container_id = "container-1"

            async def time_out(*args, on_started, on_cancelled, **kwargs):
                on_started(
                    TrialStarted(
                        request_id="request-1",
                        target=str(agent.context_id),
                        conversation_id="conversation-1",
                    )
                )
                on_cancelled(
                    TrialCancelled(
                        request_id="request-1",
                        target=str(agent.context_id),
                        conversation_id="conversation-1",
                        snapshot_id="snapshot-1",
                    )
                )
                raise asyncio.TimeoutError

            export = AsyncMock()
            with (
                patch("exo_harbor.agent.send_trial_run", side_effect=time_out),
                patch("exo_harbor.agent.export_trial_trajectory", export),
                self.assertRaises(asyncio.TimeoutError),
            ):
                context = MagicMock()
                context.metadata = {}
                await agent.run("Fix it", AsyncMock(), context)

            export.assert_awaited_once_with(
                agent._client,
                "conversation-1",
                str(agent.context_id),
                "Fix it",
                "gpt-5.6-sol",
                Path(directory) / "trajectory.json",
            )
            self.assertEqual(context.metadata["exo_snapshot_id"], "snapshot-1")
            self.assertEqual(context.metadata["exo_instruction"], "Fix it")


if __name__ == "__main__":
    unittest.main()
