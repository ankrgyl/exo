import tempfile
import unittest
from pathlib import Path

from exo_harbor.exo import ExoClient


class ExoClientTest(unittest.TestCase):
    def test_commands_select_the_exo_harness(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            client = ExoClient(
                executable="/repo/target/debug/exo",
                root=Path(directory) / "state",
                repo_root=Path(directory),
                logs_dir=Path(directory) / "logs",
            )
            self.assertEqual(
                client._command("agent", "show", "harbor")[:6],
                [
                    "/repo/target/debug/exo",
                    "--root",
                    str(Path(directory) / "state"),
                    "--harness",
                    "exo",
                    "agent",
                ],
            )
