from __future__ import annotations

import asyncio
import unittest
from pathlib import Path
from unittest.mock import patch

from exo_harbor import conventions
from exo_harbor.exo import ExoClient, ExoCommandError, _parse_trailing_id


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


class ParseTrailingIdTest(unittest.TestCase):
    def test_reads_the_sandbox_id_from_an_attach(self) -> None:
        self.assertEqual(
            _parse_trailing_id(
                "attached Docker container as sandbox sandbox-019ff873-1ebb for trial",
                pattern=r"attached Docker container as sandbox (\S+) for ",
                command="conversation sandbox attach",
            ),
            "sandbox-019ff873-1ebb",
        )

    def test_unrecognized_output_fails_loudly(self) -> None:
        # The CLI prints prose, so a reworded confirmation must break here
        # rather than hand a wrong id to the verifier.
        with self.assertRaises(ExoCommandError):
            _parse_trailing_id(
                "sandbox attached.",
                pattern=r"attached Docker container as sandbox (\S+) for ",
                command="conversation sandbox attach",
            )


if __name__ == "__main__":
    unittest.main()
