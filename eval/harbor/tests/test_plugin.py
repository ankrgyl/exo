import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

from exo_harbor.plugin import (
    ExoSessionPlugin,
    _export_learning_report,
    _export_trajectory,
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
                "conversation-1",
                "trial-1",
                "instruction",
                "model",
                Path("trajectory.json"),
            )

        log_exception.assert_called_once()

    async def test_learning_export_failure_does_not_fail_trial(self) -> None:
        with (
            patch(
                "exo_harbor.plugin.export_learning_report",
                AsyncMock(side_effect=ValueError("missing reflection")),
            ),
            patch("exo_harbor.plugin.logger.exception") as log_exception,
        ):
            await _export_learning_report(
                AsyncMock(),
                "conversation-1",
                "trial-1",
                "router",
                "learned",
                Path("learning.json"),
            )

        log_exception.assert_called_once()

    async def test_job_end_without_started_client_has_no_side_effects(self) -> None:
        plugin = ExoSessionPlugin()

        await plugin.on_job_end(AsyncMock())

    async def test_job_end_deletes_snapshots(self) -> None:
        plugin = ExoSessionPlugin()
        client = AsyncMock()
        client.delete_snapshots.return_value = 3
        plugin._client = client

        await plugin.on_job_end(AsyncMock())

        client.delete_snapshots.assert_awaited_once_with()

    async def test_job_end_exports_aggregate_learning_summary(self) -> None:
        plugin = ExoSessionPlugin(reflection_strategy="router")
        client = AsyncMock()
        client.exo_root = Path("/runs/exo")
        client.delete_snapshots.return_value = 0
        plugin._client = client
        plugin._job_dir = Path("/runs/job")
        plugin._model = "fixed-model"
        result = SimpleNamespace(
            trial_results=[
                SimpleNamespace(
                    trial_name="task-1",
                    task_name="first-task",
                    verifier_result=SimpleNamespace(rewards={"reward": 1.0}),
                )
            ]
        )

        with patch("exo_harbor.plugin._export_job_learning_summary") as export:
            await plugin.on_job_end(result)

        export.assert_called_once_with(
            job_dir=Path("/runs/job"),
            exo_root=Path("/runs/exo"),
            strategy="router",
            model="fixed-model",
            trial_metadata={
                "task-1": {
                    "task_name": "first-task",
                    "rewards": {"reward": 1.0},
                }
            },
        )


if __name__ == "__main__":
    unittest.main()
