import unittest
from unittest.mock import AsyncMock

from exo_harbor.plugin import ExoSessionPlugin


class ExoSessionPluginTest(unittest.IsolatedAsyncioTestCase):
    async def test_job_end_has_no_side_effects(self) -> None:
        plugin = ExoSessionPlugin()

        await plugin.on_job_end(AsyncMock())


if __name__ == "__main__":
    unittest.main()
