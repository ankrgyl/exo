"""Build the feedback given to Exo after Harbor verifies a trial."""

from __future__ import annotations

import json
from pathlib import Path

from harbor.models.trial.result import TrialResult

MEMORY_REFLECTION_INSTRUCTIONS = """Review your work on this completed trial and the grader feedback. Inspect the restored submitted environment when useful. Determine what went well or wrong and extract general lessons that will help you solve similar tasks in the future. Before completing this reflection, persist every useful generalizable lesson in durable memory so later trial conversations can use it. When warranted, also create or improve reusable tools or change your own policy or implementation. The feedback_complete summary is only a report to the evaluator; it does not persist learning. If there is genuinely nothing worth retaining, say so explicitly in that summary. Focus on improving future behavior; this submission has already been graded."""

ROUTER_REFLECTION_INSTRUCTIONS = """Review your work on this completed trial and the grader feedback. Inspect the restored submitted environment when useful. Determine what went well or wrong and extract only evidence-supported lessons that will help on future tasks.

Route each useful lesson to the narrowest durable form that fits:
- Memory: a concise, stable fact or heuristic that is likely to matter across different future tasks. Compare it with the durable memories already in your context. Do not add duplicates; if a new lesson supersedes an old memory, forget the old entry before remembering the replacement.
- Skill: a reusable multi-step procedure, checklist, or knowledge package.
- Tool: a deterministic operation that would otherwise require repeated mechanical work.
- Policy or implementation: a broad behavior or system change that should apply across the agent. Use this route only when you can make and validate an actual durable change with the control surfaces available in this trial. A restart by itself is not a policy improvement.
- Discard: a task-specific detail, unsupported guess, or lesson that is not likely to be useful again.

Do not put every lesson in durable memory. A poor reward is evidence that the attempted approach may be wrong, not proof that the opposite is universally correct. A good reward should preserve the specific behavior that evidence supports. Persist worthwhile learning through the selected memory, skill, tool, policy, or implementation mechanism before completing this reflection. The feedback_complete summary is only a report to the evaluator; it does not persist learning. If there is genuinely nothing worth retaining, say so explicitly in that summary. Focus on improving future behavior; this submission has already been graded."""

REFLECTION_STRATEGIES = {
    "memory": MEMORY_REFLECTION_INSTRUCTIONS,
    "router": ROUTER_REFLECTION_INSTRUCTIONS,
}

MAX_FEEDBACK_BYTES = 250_000


def reflection_instructions(strategy: str) -> str:
    """Return the reflection prompt for one experimental strategy."""
    try:
        return REFLECTION_STRATEGIES[strategy]
    except KeyError as error:
        choices = ", ".join(sorted(REFLECTION_STRATEGIES))
        raise ValueError(
            f"unknown reflection strategy {strategy!r}; expected one of: {choices}"
        ) from error


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
