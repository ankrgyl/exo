import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

from exo_harbor.plugin import ExoSessionPlugin, _export_trajectory


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


if __name__ == "__main__":
    unittest.main()
