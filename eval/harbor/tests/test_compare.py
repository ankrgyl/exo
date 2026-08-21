from __future__ import annotations

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
                "succeeded": 1 if strategy == "router" else 0,
                "failed": 0,
                "unresolved": 0,
            },
            "tool": {"succeeded": 0, "failed": 0, "unresolved": 0},
            "policy": {"succeeded": 0, "failed": 0, "unresolved": 0},
        },
        "task_reuse": {
            "prior_skill_reuse_count": skill_reuse,
            "prior_agent_tool_reuse_count": tool_reuse,
        },
        "trials": [
            {"task_name": "learn"},
            {"task_name": "transfer"},
        ],
    }


class ComparisonTest(unittest.TestCase):
    def test_reports_router_minus_memory_deltas(self) -> None:
        comparison = compare_script.build_comparison(
            memory=summary(
                "memory",
                reward=0.5,
                memory_growth=3,
                skill_reuse=0,
                tool_reuse=0,
            ),
            router=summary(
                "router",
                reward=1.0,
                memory_growth=1,
                skill_reuse=1,
                tool_reuse=2,
            ),
            memory_summary_path=Path("/runs/memory.json"),
            router_summary_path=Path("/runs/router.json"),
            provider="openrouter",
            source_digest="abc123",
            arm_order=["memory", "router"],
        )

        deltas = comparison["router_minus_memory"]
        self.assertEqual(deltas["reward_mean"]["reward"], 0.5)
        self.assertEqual(deltas["memory_growth"], -2)
        self.assertEqual(deltas["route_actions"]["succeeded"]["skill"], 1)
        self.assertEqual(deltas["route_actions"]["failed"]["memory"], -1)
        self.assertEqual(deltas["prior_task_reuse"]["prior_skill_reuse_count"], 1)
        self.assertEqual(
            deltas["prior_task_reuse"]["prior_agent_tool_reuse_count"], 2
        )

    def test_rejects_mismatched_task_sequences(self) -> None:
        memory = summary(
            "memory",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        router = summary(
            "router",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        router["trials"][1]["task_name"] = "different"

        with self.assertRaisesRegex(ValueError, "different task sequences"):
            compare_script.build_comparison(
                memory=memory,
                router=router,
                memory_summary_path=Path("memory.json"),
                router_summary_path=Path("router.json"),
                provider="openrouter",
                source_digest="abc123",
                arm_order=["memory", "router"],
            )

    def test_rejects_swapped_reflection_strategies(self) -> None:
        memory = summary(
            "router",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        router = summary(
            "memory",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )

        with self.assertRaisesRegex(ValueError, "memory reflection strategy"):
            compare_script.build_comparison(
                memory=memory,
                router=router,
                memory_summary_path=Path("memory.json"),
                router_summary_path=Path("router.json"),
                provider="openrouter",
                source_digest="abc123",
                arm_order=["memory", "router"],
            )

    def test_rejects_unexpected_learning_task_order(self) -> None:
        memory = summary(
            "memory",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        router = summary(
            "router",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )

        with self.assertRaisesRegex(ValueError, "expected learning order"):
            compare_script.build_comparison(
                memory=memory,
                router=router,
                memory_summary_path=Path("memory.json"),
                router_summary_path=Path("router.json"),
                provider="openrouter",
                source_digest="abc123",
                arm_order=["memory", "router"],
                expected_task_sequence=["transfer", "learn"],
            )

    def test_rejects_invalid_arm_order(self) -> None:
        memory = summary(
            "memory",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        router = summary(
            "router",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )

        with self.assertRaisesRegex(ValueError, "arm order"):
            compare_script.build_comparison(
                memory=memory,
                router=router,
                memory_summary_path=Path("memory.json"),
                router_summary_path=Path("router.json"),
                provider="openrouter",
                source_digest="abc123",
                arm_order=["memory", "memory"],
            )

    def test_rejects_incomplete_reflection_reports(self) -> None:
        memory = summary(
            "memory",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        router = summary(
            "router",
            reward=1.0,
            memory_growth=1,
            skill_reuse=0,
            tool_reuse=0,
        )
        router["reflection_report_count"] = 1

        with self.assertRaisesRegex(ValueError, "incomplete reflection reports"):
            compare_script.build_comparison(
                memory=memory,
                router=router,
                memory_summary_path=Path("memory.json"),
                router_summary_path=Path("router.json"),
                provider="openrouter",
                source_digest="abc123",
                arm_order=["memory", "router"],
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
