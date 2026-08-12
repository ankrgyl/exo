import unittest
from tempfile import TemporaryDirectory
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

from exo_harbor.exo import ExoClient


class ExoClientTest(unittest.IsolatedAsyncioTestCase):
    async def test_delete_snapshots_preserves_other_exo_state(self) -> None:
        with TemporaryDirectory() as temporary_directory:
            exo_root = Path(temporary_directory) / "exo"
            conversation = (
                exo_root
                / "exoharness"
                / "agents"
                / "agent-1"
                / "conversations"
                / "conversation-1"
            )
            snapshot = conversation / "snapshots" / "snapshot-1"
            snapshot.mkdir(parents=True)
            (snapshot / "payload.bin").write_bytes(b"snapshot")
            (conversation / "record.json").write_text("{}", encoding="utf-8")
            client = ExoClient(
                exo_bin=Path("/opt/exo/bin/exo"),
                exo_root=exo_root,
                repo_root=Path("/src/exo"),
            )

            count = await client.delete_snapshots()

            self.assertEqual(count, 1)
            self.assertFalse((conversation / "snapshots").exists())
            self.assertTrue((conversation / "record.json").exists())

    async def test_adapter_runner_uses_eval_root_and_records_pid(self) -> None:
        with TemporaryDirectory() as temporary_directory:
            exo_root = Path(temporary_directory) / "exo"
            client = ExoClient(
                exo_bin=Path("/opt/exo/bin/exo"),
                exo_root=exo_root,
                repo_root=Path("/src/exo"),
            )
            process = MagicMock(pid=1234)
            with patch("exo_harbor.exo.subprocess.Popen", return_value=process) as popen:
                client._spawn_adapter_runner()

            self.assertEqual(
                (exo_root / "exo-adapters.pid").read_text(encoding="utf-8"),
                "1234\n",
            )
            self.assertEqual(popen.call_args.kwargs["env"]["EXO_ROOT"], str(exo_root))

    async def test_evaluation_agent_can_create_tools(self) -> None:
        client = ExoClient(
            exo_bin=Path("/opt/exo/bin/exo"),
            exo_root=Path("/tmp/exo-root"),
            repo_root=Path("/src/exo"),
        )
        exists = AsyncMock(return_value=False)
        run = AsyncMock()
        with (
            patch.object(ExoClient, "_exists", exists),
            patch.object(ExoClient, "_run", run),
        ):
            await client.ensure_agent("gpt-5.5")

        self.assertIn("--tool-creation", run.await_args.args)
        self.assertIn("enabled", run.await_args.args)

    async def test_read_conversation_events_filters_one_turn(self) -> None:
        client = ExoClient(
            exo_bin=Path("/opt/exo/bin/exo"),
            exo_root=Path("/tmp/exo-root"),
            repo_root=Path("/src/exo"),
        )
        run = AsyncMock(return_value='{"events":[],"cursor":null}')
        with patch.object(ExoClient, "_run", run):
            result = await client.read_conversation_events(
                "conversation",
                types=["messages", "tool_result"],
                turn_id="turn-1",
                limit=10_000,
            )

        self.assertEqual(result, '{"events":[],"cursor":null}')
        run.assert_awaited_once_with(
            "conversation",
            "events",
            "harbor-eval",
            "conversation",
            "--type",
            "messages",
            "--type",
            "tool_result",
            "--turn-id",
            "turn-1",
            "--limit",
            "10000",
        )


if __name__ == "__main__":
    unittest.main()
