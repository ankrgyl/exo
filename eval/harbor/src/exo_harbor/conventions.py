from __future__ import annotations

AGENT_SLUG = "harbor-eval"
HARNESS_MODULE = "exo/harness.ts"


def trial_conversation_slug(trial_id: str) -> str:
    return f"trial-{trial_id}"


# Keys the agent writes into Harbor's AgentContext.metadata and the plugin
# reads back in its trial-ended hook.
CONVERSATION_METADATA_KEY = "exo_conversation_id"
SNAPSHOT_METADATA_KEY = "exo_snapshot_id"
INSTRUCTION_METADATA_KEY = "exo_instruction"


def parse_flag(value: object) -> bool:
    """Read a Harbor `--ak` value as a bool.

    Harbor passes agent kwargs through as strings, so a bare `bool()` would
    read "false" as true.
    """
    if isinstance(value, bool):
        return value
    return str(value).strip().lower() in {"1", "true", "yes", "on"}
