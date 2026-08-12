"""Build the feedback given to Exo after Harbor verifies a trial."""

from __future__ import annotations

import json
from pathlib import Path

from harbor.models.trial.result import TrialResult


REFLECTION_INSTRUCTIONS = """Review your work on this completed trial and the grader feedback. Inspect the restored submitted environment when useful. Determine what went well or wrong and extract general lessons that will help you solve similar tasks in the future. Before completing this reflection, persist every useful generalizable lesson in durable memory so later trial conversations can use it. When warranted, also create or improve reusable tools or change your own policy or implementation. The feedback_complete summary is only a report to the evaluator; it does not persist learning. If there is genuinely nothing worth retaining, say so explicitly in that summary. Focus on improving future behavior; this submission has already been graded."""

MAX_FEEDBACK_BYTES = 250_000


def build_feedback(result: TrialResult, verifier_dir: Path) -> str:
    """Return structured rewards, failures, and available verifier logs."""
    remaining = MAX_FEEDBACK_BYTES
    logs: dict[str, str] = {}
    if verifier_dir.is_dir():
        for path in sorted(item for item in verifier_dir.rglob("*") if item.is_file()):
            if remaining <= 0:
                break
            data = path.read_bytes()
            kept = data[:remaining]
            text = kept.decode("utf-8", errors="replace")
            remaining -= len(kept)
            if len(kept) < len(data):
                text += f"\n[truncated {len(data) - len(kept)} bytes]"
            logs[str(path.relative_to(verifier_dir))] = text

    payload = {
        "rewards": (
            result.verifier_result.rewards
            if result.verifier_result is not None
            else None
        ),
        "exception": (
            result.exception_info.model_dump(mode="json")
            if result.exception_info is not None
            else None
        ),
        "verifier_logs": logs,
    }
    return json.dumps(payload, indent=2, sort_keys=True)
