from __future__ import annotations

import importlib.util
import io
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).parents[1] / "eval.py"
SPEC = importlib.util.spec_from_file_location("eval_script", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
eval_script = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(eval_script)


class EvalScriptTest(unittest.TestCase):
    def parse(self, *arguments: str):
        with patch.object(sys, "argv", [str(SCRIPT), *arguments]):
            return eval_script.parse_args()

    def test_defaults_run_all_terminal_bench_tasks(self) -> None:
        args = self.parse()

        self.assertEqual(args.dataset, "terminal-bench")
        self.assertEqual(args.model, "gpt-5.5")
        self.assertIsNone(args.n_tasks)
        self.assertEqual(args.n_attempts, 1)

    def test_all_tasks_omits_harbor_limit(self) -> None:
        args = self.parse()

        command = eval_script.harbor_command(
            args,
            harbor="harbor",
            repo=Path("/repo"),
            exo=Path("/repo/exo"),
            run_dir=Path("/runs/one"),
            job_name="terminal-bench-all",
        )

        self.assertNotIn("--n-tasks", command)

    def test_flags_override_config_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "eval.toml"
            config.write_text(
                'dataset = "terminal-bench-sample"\n'
                'model = "configured-model"\n'
                "n_tasks = 4\n"
            )

            args = self.parse(
                f"--config={config}", "--model=flag-model", "--n-tasks=2"
            )

        self.assertEqual(args.dataset, "terminal-bench-sample")
        self.assertEqual(args.model, "flag-model")
        self.assertEqual(args.n_tasks, 2)

    def test_local_dataset_path_can_come_from_config(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            dataset = Path(directory) / "dataset"
            dataset.mkdir()
            config = Path(directory) / "eval.toml"
            config.write_text(
                'dataset = "endless-terminals"\n'
                f'dataset_path = "{dataset}"\n'
            )

            args = self.parse(f"--config={config}")

            self.assertEqual(
                eval_script.dataset_arguments(args), ["--path", str(dataset)]
            )

    def test_smoke_test_uses_the_bundled_task(self) -> None:
        args = self.parse("--dataset=smoke-test")

        self.assertEqual(
            eval_script.dataset_arguments(args),
            ["--path", str(SCRIPT.parent / "datasets/smoke-test")],
        )

    def test_self_evolution_smoke_test_uses_bundled_dataset(self) -> None:
        args = self.parse("--dataset=self-evolution-smoke-test")
        dataset = SCRIPT.parent / "datasets/self-evolution-smoke-test"

        self.assertEqual(
            eval_script.dataset_arguments(args),
            [
                "--path",
                str(dataset),
            ],
        )
        self.assertEqual(
            sorted(path.name for path in dataset.iterdir()),
            ["01-install-tool", "02-reuse-tool", "03-restart-policy"],
        )

    def test_terminal_bench_easy_selects_three_tasks(self) -> None:
        args = self.parse("--dataset=terminal-bench-easy")

        self.assertEqual(
            eval_script.dataset_arguments(args),
            [
                "--dataset",
                "terminal-bench@2.0",
                "--include-task-name",
                "fix-git",
                "--include-task-name",
                "prove-plus-comm",
                "--include-task-name",
                "cobol-modernization",
            ],
        )

    def test_result_paths_include_harbor_result_and_viewer(self) -> None:
        output = io.StringIO()

        with redirect_stdout(output):
            eval_script.print_result_paths(Path("/runs/one"), "terminal-bench-3")

        self.assertIn(
            "Harbor results: /runs/one/jobs/terminal-bench-3/result.json",
            output.getvalue(),
        )
        self.assertIn(
            "View: harbor view /runs/one/jobs",
            output.getvalue(),
        )


if __name__ == "__main__":
    unittest.main()
