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
