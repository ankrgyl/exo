from __future__ import annotations

import asyncio
import unittest
from pathlib import Path
from unittest.mock import patch

from exo_harbor import conventions
from exo_harbor.exo import ExoClient


CLIENT = ExoClient(
    exo_bin=Path("/repo/target/debug/exo"),
    exo_root=Path("/runs/one/exo"),
    repo_root=Path("/repo"),
)


class ArgvTest(unittest.TestCase):

    def test_selects_the_exo_harness(self) -> None:
        # Without it, `agent create --module` is rejected outright: --module is
        # only valid with --harness typescript or exo. The flag's own help text
        # omits `exo`, so this is easy to drop by reading the docs alone.
        argv = CLIENT._argv("agent", "create")
        self.assertEqual(argv[argv.index("--harness") + 1], "exo")

    def test_harness_module_comes_from_this_repo_layout(self) -> None:
        self.assertTrue(
            (Path(__file__).parents[3] / conventions.HARNESS_MODULE).is_file(),
            f"{conventions.HARNESS_MODULE} should exist in this repo",
        )


class SandboxOwnerTest(unittest.IsolatedAsyncioTestCase):
    """Every sandbox command must name the conversation as the owner.

    A sandbox id resolves only against its owner. Dropping --conversation is
    silent: the sandbox gets created under the agent, and the trial
    conversation then cannot address it.
    """

    async def capture_argv(self, call) -> tuple[str, ...]:
        calls: list[tuple[str, ...]] = []

        async def fake_run(_self, *args: str, **_kwargs: object) -> str:
            calls.append(args)
            return "sandbox-1"

        with patch.object(ExoClient, "_run", fake_run):
            await call()
        return calls[0]

    async def test_sandbox_commands_are_scoped_to_the_conversation(self) -> None:
        cases = {
            "attach": lambda: CLIENT.attach_container("trial-1", "abc123"),
            "snapshot": lambda: CLIENT.snapshot_sandbox("trial-1", "sandbox-1"),
            "restore": lambda: CLIENT.restore_sandbox("trial-1", "snapshot-1"),
        }
        for command, call in cases.items():
            with self.subTest(command=command):
                argv = await self.capture_argv(call)
                self.assertEqual(argv[0], "sandbox")
                self.assertEqual(argv[1], command)
                self.assertEqual(
                    argv[argv.index("--agent") + 1], conventions.AGENT_SLUG
                )
                self.assertEqual(argv[argv.index("--conversation") + 1], "trial-1")


if __name__ == "__main__":
    unittest.main()
