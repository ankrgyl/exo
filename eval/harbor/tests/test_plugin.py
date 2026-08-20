from __future__ import annotations

import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

from exo_harbor import conventions
from exo_harbor.plugin import (
    REFLECTION_INSTRUCTIONS,
    ExoSessionPlugin,
    _export_trajectory,
    build_feedback,
    strip_setup_noise,
)


def trial_event(metadata: dict[str, object] | None, tmp_path: Path):
    return SimpleNamespace(
        result=SimpleNamespace(
            id="trial-1",
            agent_result=SimpleNamespace(metadata=metadata),
            verifier_result=SimpleNamespace(rewards={"reward": 1.0}),
            exception_info=None,
        ),
        config=SimpleNamespace(trials_dir=tmp_path),
        trial_name="trial-1",
    )


class ExoSessionPluginTest(unittest.IsolatedAsyncioTestCase):
    async def test_trajectory_export_failure_does_not_fail_trial(self) -> None:
        with (
            patch(
                "exo_harbor.plugin.export_trial_trajectory",
                AsyncMock(side_effect=ValueError("missing turn")),
            ),
            patch("exo_harbor.plugin.logger.exception") as log_exception,
        ):
            await _export_trajectory(
                AsyncMock(),
                "trial-1",
                "trial-1",
                "instruction",
                "model",
                Path("trajectory.json"),
            )

        log_exception.assert_called_once()

    async def test_reflection_restores_the_submitted_snapshot(self) -> None:
        plugin = ExoSessionPlugin()
        client = AsyncMock()
        client.create_sandbox_from_snapshot.return_value = "sandbox-new"
        plugin._client = client
        plugin._model = "gpt-5.5"
        event = trial_event(
            {
                conventions.CONVERSATION_METADATA_KEY: "trial-abc",
                conventions.SNAPSHOT_METADATA_KEY: "snapshot-1",
                conventions.INSTRUCTION_METADATA_KEY: "do the thing",
            },
            Path("/tmp/trials"),
        )

        with patch("exo_harbor.plugin.export_trial_trajectory", AsyncMock()):
            await plugin._reflect_on_trial(event)

        client.create_sandbox_from_snapshot.assert_awaited_once_with(
            "trial-abc", "snapshot-1"
        )
        # Reflection must land in the same conversation as the trial, or the
        # agent reflects without any memory of what it did.
        conversation, prompt = client.send.await_args.args
        self.assertEqual(conversation, "trial-abc")
        self.assertIn("Grader feedback:", prompt)

    async def test_a_failed_reflection_does_not_abort_the_job(self) -> None:
        # Reflection runs after grading, so it cannot change this trial's
        # score -- but raising here forfeits every remaining trial.
        plugin = ExoSessionPlugin()
        client = AsyncMock()
        client.create_sandbox_from_snapshot.side_effect = OSError(
            7, "Argument list too long"
        )
        plugin._client = client
        plugin._model = "gpt-5.5"
        event = trial_event(
            {
                conventions.CONVERSATION_METADATA_KEY: "trial-abc",
                conventions.SNAPSHOT_METADATA_KEY: "snapshot-1",
                conventions.INSTRUCTION_METADATA_KEY: "do the thing",
            },
            Path("/tmp/trials"),
        )

        with patch("exo_harbor.plugin.logger.exception") as log_exception:
            await plugin._reflect_on_trial(event)

        log_exception.assert_called_once()

    async def test_trial_without_a_snapshot_skips_reflection(self) -> None:
        # A trial that timed out before snapshotting has nothing to restore.
        # It should still keep its partial trajectory, not raise.
        plugin = ExoSessionPlugin()
        plugin._client = AsyncMock()
        plugin._model = "gpt-5.5"

        await plugin._reflect_on_trial(trial_event({}, Path("/tmp/trials")))

        plugin._client.send.assert_not_awaited()

    async def test_job_end_deletes_snapshots(self) -> None:
        plugin = ExoSessionPlugin()
        client = AsyncMock()
        client.delete_snapshots.return_value = 3
        plugin._client = client

        await plugin.on_job_end(AsyncMock())

        client.delete_snapshots.assert_awaited_once_with()


class FeedbackTest(unittest.TestCase):

    def test_includes_rewards_exception_and_verifier_output(self) -> None:
        with TemporaryDirectory() as directory:
            verifier_dir = Path(directory)
            (verifier_dir / "test-stdout.txt").write_text("one test failed")
            result = SimpleNamespace(
                verifier_result=SimpleNamespace(rewards={"reward": 0.5}),
                exception_info=SimpleNamespace(
                    model_dump=lambda **_kwargs: {"message": "verification failed"}
                ),
            )

            feedback = json.loads(build_feedback(result, verifier_dir))

        self.assertEqual(feedback["rewards"], {"reward": 0.5})
        self.assertEqual(feedback["exception"]["message"], "verification failed")
        self.assertEqual(
            feedback["verifier_logs"]["test-stdout.txt"], "one test failed"
        )


class FeedbackSizeTest(unittest.TestCase):
    def test_oversized_logs_are_dropped_so_the_prompt_can_spawn(self) -> None:
        # A single argv string is capped at 128 KiB by the kernel, and django's
        # test output alone is ~150 KB. Exceeding it used to abort the job.
        from exo_harbor.plugin import MAX_ARG_STRLEN

        with TemporaryDirectory() as directory:
            verifier_dir = Path(directory)
            (verifier_dir / "test-stdout.txt").write_text("x" * 400_000)
            result = SimpleNamespace(
                verifier_result=SimpleNamespace(rewards={"reward": 0.0}),
                exception_info=None,
            )

            feedback = build_feedback(result, verifier_dir)

        self.assertLess(len(feedback.encode("utf-8")), MAX_ARG_STRLEN)
        # The grade survives even when the logs do not.
        self.assertEqual(json.loads(feedback)["rewards"], {"reward": 0.0})


class StripSetupNoiseTest(unittest.TestCase):
    def test_drops_everything_before_the_pytest_banner(self) -> None:
        text = (
            "Get:1 http://archive.ubuntu.com/ubuntu noble InRelease [126 kB]\n"
            "(Reading database ... 45%\n"
            "============================= test session starts ====\n"
            "collected 1 item\n"
        )

        self.assertEqual(
            strip_setup_noise(text),
            "============================= test session starts ====\ncollected 1 item\n",
        )


if __name__ == "__main__":
    unittest.main()
