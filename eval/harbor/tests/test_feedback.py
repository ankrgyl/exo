import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace

from exo_harbor.feedback import (
    MEMORY_REFLECTION_INSTRUCTIONS,
    ROUTER_REFLECTION_INSTRUCTIONS,
    build_feedback,
    reflection_instructions,
)


class FeedbackTest(unittest.TestCase):
    def test_reflection_routes_learning_to_the_right_durable_form(self) -> None:
        for learning_form in (
            "Memory:",
            "Skill:",
            "Tool:",
            "Policy or implementation:",
            "Discard:",
        ):
            self.assertIn(learning_form, ROUTER_REFLECTION_INSTRUCTIONS)
        self.assertIn(
            "Do not put every lesson in durable memory",
            ROUTER_REFLECTION_INSTRUCTIONS,
        )
        self.assertNotIn(
            "persist every useful generalizable lesson in durable memory",
            ROUTER_REFLECTION_INSTRUCTIONS,
        )
        self.assertIn("does not persist learning", ROUTER_REFLECTION_INSTRUCTIONS)
        self.assertIn(
            "A restart by itself is not a policy improvement",
            ROUTER_REFLECTION_INSTRUCTIONS,
        )

    def test_reflection_requires_evidence_and_memory_hygiene(self) -> None:
        self.assertIn("evidence-supported lessons", ROUTER_REFLECTION_INSTRUCTIONS)
        self.assertIn("Do not add duplicates", ROUTER_REFLECTION_INSTRUCTIONS)
        self.assertIn("forget the old entry", ROUTER_REFLECTION_INSTRUCTIONS)
        self.assertIn("poor reward", ROUTER_REFLECTION_INSTRUCTIONS)
        self.assertIn("good reward", ROUTER_REFLECTION_INSTRUCTIONS)

    def test_reflection_strategy_retains_memory_first_baseline(self) -> None:
        self.assertIs(
            reflection_instructions("memory"), MEMORY_REFLECTION_INSTRUCTIONS
        )
        self.assertIs(
            reflection_instructions("router"), ROUTER_REFLECTION_INSTRUCTIONS
        )
        self.assertIn(
            "persist every useful generalizable lesson in durable memory",
            MEMORY_REFLECTION_INSTRUCTIONS,
        )

    def test_unknown_reflection_strategy_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "unknown reflection strategy"):
            reflection_instructions("guess")

    def test_includes_rewards_exception_and_verifier_output(self) -> None:
        with TemporaryDirectory() as directory:
            verifier_dir = Path(directory)
            (verifier_dir / "test-stdout.txt").write_text("one test failed")
            result = SimpleNamespace(
                verifier_result=SimpleNamespace(rewards={"reward": 0.5}),
                exception_info=SimpleNamespace(
                    model_dump=lambda **_kwargs: {"message": "verification failed"}
                ),
            )

            feedback = json.loads(build_feedback(result, verifier_dir))

        self.assertEqual(feedback["rewards"], {"reward": 0.5})
        self.assertEqual(feedback["exception"]["message"], "verification failed")
        self.assertEqual(
            feedback["verifier_logs"]["test-stdout.txt"], "one test failed"
        )


if __name__ == "__main__":
    unittest.main()
