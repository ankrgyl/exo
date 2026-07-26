import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast
from unittest.mock import AsyncMock, patch
from uuid import UUID

from exo_harbor.continual import ContinualExoPlugin
from exo_harbor.protocol import FeedbackProcessed
from harbor.models.agent.context import AgentContext
from harbor.models.environment_type import EnvironmentType
from harbor.models.verifier.result import VerifierResult


class FakeJob:
    def __init__(self, job_dir: Path) -> None:
        self.job_dir = job_dir
        self.config = SimpleNamespace(
            environment=SimpleNamespace(type=EnvironmentType.DOCKER),
            n_concurrent_trials=1,
        )
        self.callback = None

    def on_trial_ended(self, callback):
        self.callback = callback


class ContinualExoPluginTest(unittest.IsolatedAsyncioTestCase):
    async def test_feedback_is_awaited_and_recorded(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            job_dir = Path(directory) / "job"
            trial_dir = job_dir / "trial-name"
            verifier_dir = trial_dir / "verifier"
            verifier_dir.mkdir(parents=True)
            (verifier_dir / "test-stdout.txt").write_text("tests passed\n")
            (verifier_dir / "test-stderr.txt").write_text("")

            plugin = ContinualExoPlugin(exo_repo_root=directory)
            job = FakeJob(job_dir)
            await plugin.on_job_start(cast(Any, job))
            assert job.callback is not None
            context = AgentContext(
                metadata={
                    "exo": {
                        "conversation_id": "conversation-id",
                        "socket_path": str(Path(directory) / "harbor.sock"),
                        "conversation_mode": "per_task",
                    }
                }
            )
            result = SimpleNamespace(
                agent_result=context,
                step_results=None,
                verifier_result=VerifierResult(rewards={"reward": 1}),
                exception_info=None,
            )
            event = SimpleNamespace(
                trial_name="trial-name",
                trial_id=UUID("11111111-1111-1111-1111-111111111111"),
                task_name="example-task",
                result=result,
            )

            with patch(
                "exo_harbor.continual.send_verification_result",
                AsyncMock(
                    return_value=FeedbackProcessed(
                        trial_id=str(event.trial_id), summary="retained"
                    )
                ),
            ) as send:
                await job.callback(cast(Any, event))

            assert send.await_args is not None
            request = send.await_args.args[1]
            self.assertEqual(request.rewards, {"reward": 1})
            self.assertEqual(request.verifier_stdout, "tests passed\n")
            sidecar = json.loads((trial_dir / "exo-feedback.json").read_text())
            self.assertEqual(sidecar["status"], "processed")
            self.assertEqual(sidecar["summary"], "retained")

    async def test_rejects_non_sequential_jobs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            plugin = ContinualExoPlugin(exo_repo_root=directory)
            job = FakeJob(Path(directory) / "job")
            job.config.n_concurrent_trials = 2
            with self.assertRaisesRegex(ValueError, "one trial at a time"):
                await plugin.on_job_start(cast(Any, job))
