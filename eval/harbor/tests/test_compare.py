from __future__ import annotations

import argparse
import importlib.util
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "compare.py"
SPEC = importlib.util.spec_from_file_location("compare_script", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
compare_script = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compare_script)


def summary(
    strategy: str,
    *,
    reward: float,
    memory_growth: int,
    skill_reuse: int,
    tool_reuse: int,
) -> dict:
    return {
        "schema_version": 2,
        "reflection_strategy": strategy,
        "model": "fixed-model",
        "trial_count": 2,
        "reflection_report_count": 2,
        "rewards": {"reward": {"count": 2, "mean": reward}},
        "memory": {"growth": memory_growth},
        "route_counts": {
            "memory": {
                "succeeded": memory_growth,
                "failed": 1 if strategy == "memory" else 0,
                "unresolved": 0,
            },
            "skill": {
                "succeeded": 1 if strategy in {"router", "lifecycle"} else 0,
                "failed": 0,
                "unresolved": 0,
            },
            "tool": {"succeeded": 0, "failed": 0, "unresolved": 0},
            "policy": {"succeeded": 0, "failed": 0, "unresolved": 0},
        },
        "task_reuse": {
            "prior_skill_reuse_count": skill_reuse,
            "prior_agent_tool_reuse_count": tool_reuse,
            "learning_activation_count": 1 if strategy == "lifecycle" else 0,
        },
        "lifecycle": {
            "proposal_count": 1 if strategy == "lifecycle" else 0,
            "promotion_count": 1 if strategy == "lifecycle" else 0,
            "rejection_count": 0,
            "discard_count": 0,
            "unresolved_count": 0,
        },
        "trials": [
            {
                "task_name": "learn",
                "rewards": {"reward": reward},
                "task_usage": {"learning_artifacts_activated": []},
            },
            {
                "task_name": "transfer",
                "rewards": {"reward": reward},
                "task_usage": {
                    "learning_artifacts_activated": (
                        [{"id": "learn-1"}] if strategy == "lifecycle" else []
                    )
                },
            },
        ],
    }


class ComparisonTest(unittest.TestCase):
    def test_expected_sequence_respects_task_limit(self) -> None:
        args = argparse.Namespace(
            dataset="learning-router-transfer-test",
            include_task_names=[],
            n_tasks=2,
            n_attempts=2,
        )

        self.assertEqual(
            compare_script.expected_task_sequence(args),
            [
                "exo/learning-router-normalization-first",
                "exo/learning-router-normalization-transfer",
                "exo/learning-router-normalization-first",
                "exo/learning-router-normalization-transfer",
            ],
        )

    def test_reports_candidate_minus_baseline_deltas(self) -> None:
        comparison = compare_script.build_comparison(
            baseline=summary(
                "router",
                reward=0.5,
                memory_growth=3,
                skill_reuse=0,
                tool_reuse=0,
            ),
            candidate=summary(
                "lifecycle",
                reward=1.0,
                memory_growth=1,
                skill_reuse=1,
                tool_reuse=2,
            ),
            baseline_strategy="router",
            candidate_strategy="lifecycle",
            baseline_summary_path=Path("/runs/baseline.json"),
            candidate_summary_path=Path("/runs/candidate.json"),
            provider="openrouter",
            source_digest="abc123",
            arm_order=["baseline", "candidate"],
        )

        deltas = comparison["candidate_minus_baseline"]
        self.assertEqual(deltas["reward_mean"]["reward"], 0.5)
        self.assertEqual(deltas["task_reward_mean"]["transfer"]["reward"], 0.5)
        self.assertEqual(deltas["task_activations"]["learn"], 0)
        self.assertEqual(deltas["task_activations"]["transfer"], 1)
        self.assertEqual(deltas["memory_growth"], -2)
        self.assertEqual(deltas["route_actions"]["succeeded"]["skill"], 0)
        self.assertEqual(deltas["route_actions"]["failed"]["memory"], 0)
        self.assertEqual(deltas["prior_task_reuse"]["prior_skill_reuse_count"], 1)
        self.assertEqual(
            deltas["prior_task_reuse"]["prior_agent_tool_reuse_count"], 2
        )
        self.assertEqual(
            deltas["prior_task_reuse"]["learning_activation_count"], 1
        )
        self.assertEqual(deltas["lifecycle"]["promotion_count"], 1)

    def test_rejects_mismatched_task_sequences(self) -> None:
        baseline = summary(
            "router",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        candidate = summary(
            "lifecycle",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        candidate["trials"][1]["task_name"] = "different"

        with self.assertRaisesRegex(ValueError, "different task sequences"):
            compare_script.build_comparison(
                baseline=baseline,
                candidate=candidate,
                baseline_strategy="router",
                candidate_strategy="lifecycle",
                baseline_summary_path=Path("baseline.json"),
                candidate_summary_path=Path("candidate.json"),
                provider="openrouter",
                source_digest="abc123",
                arm_order=["baseline", "candidate"],
            )

    def test_rejects_swapped_reflection_strategies(self) -> None:
        baseline = summary(
            "memory",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        candidate = summary(
            "router",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )

        with self.assertRaisesRegex(ValueError, "router reflection strategy"):
            compare_script.build_comparison(
                baseline=baseline,
                candidate=candidate,
                baseline_strategy="router",
                candidate_strategy="lifecycle",
                baseline_summary_path=Path("baseline.json"),
                candidate_summary_path=Path("candidate.json"),
                provider="openrouter",
                source_digest="abc123",
                arm_order=["baseline", "candidate"],
            )

    def test_rejects_unexpected_learning_task_order(self) -> None:
        baseline = summary(
            "router",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        candidate = summary(
            "lifecycle",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )

        with self.assertRaisesRegex(ValueError, "expected learning order"):
            compare_script.build_comparison(
                baseline=baseline,
                candidate=candidate,
                baseline_strategy="router",
                candidate_strategy="lifecycle",
                baseline_summary_path=Path("baseline.json"),
                candidate_summary_path=Path("candidate.json"),
                provider="openrouter",
                source_digest="abc123",
                arm_order=["baseline", "candidate"],
                expected_task_sequence=["transfer", "learn"],
            )

    def test_rejects_invalid_arm_order(self) -> None:
        baseline = summary(
            "router",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        candidate = summary(
            "lifecycle",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )

        with self.assertRaisesRegex(ValueError, "arm order"):
            compare_script.build_comparison(
                baseline=baseline,
                candidate=candidate,
                baseline_strategy="router",
                candidate_strategy="lifecycle",
                baseline_summary_path=Path("baseline.json"),
                candidate_summary_path=Path("candidate.json"),
                provider="openrouter",
                source_digest="abc123",
                arm_order=["baseline", "baseline"],
            )

    def test_rejects_incomplete_reflection_reports(self) -> None:
        baseline = summary(
            "router",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        candidate = summary(
            "lifecycle",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        candidate["reflection_report_count"] = 1

        with self.assertRaisesRegex(ValueError, "incomplete reflection reports"):
            compare_script.build_comparison(
                baseline=baseline,
                candidate=candidate,
                baseline_strategy="router",
                candidate_strategy="lifecycle",
                baseline_summary_path=Path("baseline.json"),
                candidate_summary_path=Path("candidate.json"),
                provider="openrouter",
                source_digest="abc123",
                arm_order=["baseline", "candidate"],
            )

    def test_source_snapshot_excludes_generated_state(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            destination = root / "destination"
            source.mkdir()
            (source / "tracked.txt").write_text("same", encoding="utf-8")
            for ignored in (".git", ".local", ".exo", "target", "node_modules"):
                path = source / ignored
                path.mkdir()
                (path / "ignored.txt").write_text("ignore", encoding="utf-8")

            digest = compare_script.copy_source_snapshot(source, destination)

            self.assertEqual(
                digest,
                compare_script.workspace_digest(destination),
            )
            self.assertEqual((destination / "tracked.txt").read_text(), "same")
            for ignored in (".git", ".local", ".exo", "target", "node_modules"):
                self.assertFalse((destination / ignored).exists())

    def test_runtime_dependencies_are_linked_after_snapshotting(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            destination = root / "destination"
            dependencies = source / "node_modules"
            dependencies.mkdir(parents=True)
            destination.mkdir()

            compare_script.link_runtime_dependencies(source, destination)

            link = destination / "node_modules"
            self.assertTrue(link.is_symlink())
            self.assertEqual(link.resolve(), dependencies.resolve())

    def test_scores_gold_labels_for_the_transfer_protocol(self) -> None:
        baseline = summary(
            "router",
            reward=0.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        candidate = summary(
            "lifecycle",
            reward=1.0,
            memory_growth=0,
            skill_reuse=1,
            tool_reuse=0,
        )
        sequence = compare_script.EXPECTED_TASK_SEQUENCES[
            "learning-router-transfer-test"
        ]
        baseline["trial_count"] = 3
        baseline["reflection_report_count"] = 3
        candidate["trial_count"] = 3
        candidate["reflection_report_count"] = 3
        baseline["trials"] = [
            {
                "task_name": sequence[0],
                "rewards": {"reward": 1.0},
                "route_counts": {
                    "memory": {
                        "succeeded": 1,
                        "failed": 0,
                        "unresolved": 0,
                    }
                },
                "lifecycle": {},
                "task_usage": {"learning_artifacts_activated": []},
            },
            {
                "task_name": sequence[1],
                "rewards": {"reward": 0.0},
                "task_usage": {"learning_artifacts_activated": []},
            },
            {
                "task_name": sequence[2],
                "rewards": {"reward": 0.0},
                "task_usage": {"learning_artifacts_activated": []},
            },
        ]
        candidate["trials"] = [
            {
                "task_name": sequence[0],
                "rewards": {"reward": 1.0},
                "lifecycle": {
                    "promotions": [{"route": "skill", "status": "promoted"}]
                },
                "task_usage": {"learning_artifacts_activated": []},
            },
            {
                "task_name": sequence[1],
                "rewards": {"reward": 1.0},
                "task_usage": {
                    "learning_artifacts_activated": [{"id": "learn-1"}],
                    "skills_reused_from_prior_tasks": [
                        {"name": "flint-normalization"}
                    ],
                    "agent_tools_reused_from_prior_tasks": [],
                },
            },
            {
                "task_name": sequence[2],
                "rewards": {"reward": 1.0},
                "task_usage": {"learning_artifacts_activated": []},
            },
        ]

        comparison = compare_script.build_comparison(
            baseline=baseline,
            candidate=candidate,
            baseline_strategy="router",
            candidate_strategy="lifecycle",
            baseline_summary_path=Path("baseline.json"),
            candidate_summary_path=Path("candidate.json"),
            provider="openrouter",
            source_digest="abc123",
            arm_order=["baseline", "candidate"],
            expected_task_sequence=sequence,
        )

        self.assertEqual(comparison["schema_version"], 4)
        self.assertTrue(comparison["router_proof"]["proven"])
        self.assertEqual(
            comparison["router_proof"]["candidate_minus_baseline"][
                "route_accuracy"
            ],
            1.0,
        )

    def test_runtime_dependencies_must_be_installed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            destination = root / "destination"
            source.mkdir()
            destination.mkdir()

            with self.assertRaisesRegex(ValueError, "run pnpm install first"):
                compare_script.link_runtime_dependencies(source, destination)


if __name__ == "__main__":
    unittest.main()
