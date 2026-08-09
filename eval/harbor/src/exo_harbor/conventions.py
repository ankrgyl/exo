"""Names shared by Harbor's independently constructed plugin and agents."""

from __future__ import annotations

from pathlib import Path

# Fixed slugs are safe because each job has its own exo_root.
AGENT_SLUG = "harbor-eval"

# Keep adapter setup chatter out of measured trial conversations.
SETUP_CONVERSATION_SLUG = "harbor-setup"


def socket_path(exo_root: Path) -> Path:
    """Where the trial adapter listens for this job.

    Under exo_root rather than the adapter's ~/.exo/trial.sock default, so
    concurrent jobs on one host do not fight over one socket.
    """
    return exo_root / "trial.sock"
