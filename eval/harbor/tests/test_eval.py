from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
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

    def make_ordered_dataset(self, directory: str, names: list[str]) -> Path:
        import json

        dataset = Path(directory) / "dataset"
        dataset.mkdir()
        for name in names:
            task = dataset / name
            task.mkdir()
            (task / "task.toml").write_text('schema_version = "1.4"\n')
        # Order deliberately differs from both name order and creation order.
        (dataset / "task_order.json").write_text(json.dumps(names))
        return dataset

    def test_ordered_dataset_generates_task_list_config(self) -> None:
        import json

        with tempfile.TemporaryDirectory() as directory:
            names = ["02-second", "01-first", "03-third"]
            dataset = self.make_ordered_dataset(directory, names)
            run_dir = Path(directory) / "run"
            args = self.parse(f"--dataset-path={dataset}")

            arguments = eval_script.dataset_arguments(args, run_dir)

            self.assertEqual(arguments[0], "--config")
            config = json.loads(Path(arguments[1]).read_text())
            self.assertEqual(
                [task["path"] for task in config["tasks"]],
                [str(dataset / name) for name in names],
            )

    def test_ordered_dataset_applies_n_tasks_to_the_sequence_prefix(self) -> None:
        import json

        with tempfile.TemporaryDirectory() as directory:
            names = ["02-second", "01-first", "03-third"]
            dataset = self.make_ordered_dataset(directory, names)
            run_dir = Path(directory) / "run"
            args = self.parse(f"--dataset-path={dataset}", "--n-tasks=2")

            arguments = eval_script.dataset_arguments(args, run_dir)

            config = json.loads(Path(arguments[1]).read_text())
            self.assertEqual(
                [task["path"] for task in config["tasks"]],
                [str(dataset / "02-second"), str(dataset / "01-first")],
            )

            command = eval_script.harbor_command(
                args,
                harbor="harbor",
                repo=Path("/repo"),
                exo=Path("/repo/exo"),
                run_dir=run_dir,
                jobs_dir=Path("/runs/jobs"),
                job_name="ordered-2",
            )
            self.assertNotIn("--n-tasks", command)
            self.assertIn("--config", command)

    def test_ordered_dataset_rejects_unknown_task_names(self) -> None:
        import json

        with tempfile.TemporaryDirectory() as directory:
            dataset = self.make_ordered_dataset(directory, ["01-first"])
            (dataset / "task_order.json").write_text(
                json.dumps(["01-first", "02-missing"])
            )
            args = self.parse(f"--dataset-path={dataset}")

            with self.assertRaises(ValueError):
                eval_script.dataset_arguments(args, Path(directory) / "run")

    def test_unordered_dataset_path_still_uses_path_flag(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            dataset = Path(directory) / "dataset"
            dataset.mkdir()
            args = self.parse(f"--dataset-path={dataset}")

            self.assertEqual(
                eval_script.dataset_arguments(args, Path(directory) / "run"),
                ["--path", str(dataset)],
            )

    def test_config_is_found_next_to_eval_sh(self) -> None:
        # eval.sh runs the evaluation from the repository root, so a bare
        # filename must still resolve against the directory holding eval.sh.
        config = SCRIPT.parent / "test-config-resolution.toml"
        config.write_text('dataset = "smoke-test"\n')
        try:
            args = self.parse("--config=test-config-resolution.toml")
        finally:
            config.unlink()

        self.assertEqual(args.dataset, "smoke-test")

    def test_local_datasets_still_honour_task_filters(self) -> None:
        # A local checkout can hold a whole benchmark, so dropping the filter
        # would run every task in it.
        with tempfile.TemporaryDirectory() as directory:
            dataset = Path(directory) / "dataset"
            dataset.mkdir()
            args = self.parse(
                f"--dataset-path={dataset}",
                "--include-task-name=django__django-16938",
            )

            self.assertEqual(
                eval_script.dataset_arguments(args),
                [
                    "--path",
                    str(dataset),
                    "--include-task-name",
                    "django__django-16938",
                ],
            )

if __name__ == "__main__":
    unittest.main()
