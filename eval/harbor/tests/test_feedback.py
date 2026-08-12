import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace

from exo_harbor.feedback import REFLECTION_INSTRUCTIONS, build_feedback


class FeedbackTest(unittest.TestCase):
    def test_reflection_requires_durable_learning_before_completion(self) -> None:
        self.assertIn("persist every useful generalizable lesson", REFLECTION_INSTRUCTIONS)
        self.assertIn("does not persist learning", REFLECTION_INSTRUCTIONS)

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
