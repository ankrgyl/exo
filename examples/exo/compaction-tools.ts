import {
  DEFAULT_COMPACTION_POLICY,
  readActiveCheckpoint,
  resolveCompactionPolicy,
  type CompactionCheckpoint,
  type CompactionPolicy,
  type Conversation,
  type HarnessToolRegistry,
  type JsonObject,
  type Message,
  type ToolDefinition,
  type ToolInstance,
  type ToolResult,
  type TurnContext,
} from "@exo/harness";

// Compaction is the one piece of harness policy that silently removes things
// from the agent's own view of its history. An agent that cannot see that
// happening cannot reason about a gap in what it remembers, and cannot tune the
// policy that caused it — which is exactly the kind of self-knowledge this
// harness exists to support. These tools make it inspectable; the checkpoint
// event and summary artifact make it recoverable.

function conversationOf(context: TurnContext): Conversation {
  return context.exoharness.current.conversation;
}

export function registerCompactionTools(registry: HarnessToolRegistry): void {
  registry.register(describeCompactionTool());
  registry.register(readCompactionSummaryTool());
}

function localTool(
  definition: ToolDefinition,
  execute: (context: TurnContext) => Promise<ToolResult>,
): ToolInstance {
  return {
    source: "built_in",
    definition,
    handler: {
      execute(_args, execution) {
        return execute(execution.context);
      },
    },
  };
}

const NO_ARGS: ToolDefinition["parameters"] = {
  type: "object",
  additionalProperties: false,
  properties: {},
  required: [],
};

function describeCompactionTool(): ToolInstance {
  return localTool(
    {
      name: "describe_compaction",
      description:
        "Report the conversation compaction policy in effect and the active compaction checkpoint, if any. Compaction replaces older history in your prompt with a summary once the prompt grows past a share of the model's input limit; the raw history stays in the event log. Use this to find out whether part of your context has been summarized away, how much of it, and under what settings. Read-only.",
      parameters: NO_ARGS,
    },
    async (context) => {
      const policy = resolveCompactionPolicy(context.agentConfig.compaction);
      const checkpoint = await readActiveCheckpoint(conversationOf(context));
      const described: JsonObject = {
        ok: true,
        policy: policyJson(policy),
        checkpoint: checkpoint === null ? null : checkpointJson(checkpoint),
      };
      return described;
    },
  );
}

function readCompactionSummaryTool(): ToolInstance {
  return localTool(
    {
      name: "read_compaction_summary",
      description:
        "Read the full text of the summary that currently stands in for your compacted history. Use this when you need to check exactly what the summary says before relying on it, or before concluding that a detail was lost. If a detail is missing, the raw events are still queryable with list_conversation_events. Read-only.",
      parameters: NO_ARGS,
    },
    async (context) => {
      const conversation = conversationOf(context);
      const checkpoint = await readActiveCheckpoint(conversation);
      if (checkpoint === null) {
        const empty: JsonObject = {
          ok: true,
          summary: null,
          artifactPath: null,
          artifactVersion: null,
        };
        return empty;
      }
      const summary = await conversation.readArtifactText({
        artifactId: checkpoint.artifactId,
        version: checkpoint.artifactVersion,
      });
      const found: JsonObject = {
        ok: true,
        summary,
        artifactPath: checkpoint.artifactPath,
        artifactVersion: checkpoint.artifactVersion,
      };
      return found;
    },
  );
}

/**
 * Prompt block telling the agent its history has been compacted.
 *
 * Only emitted once a checkpoint exists *and* its summary is actually in the
 * prompt. Explaining compaction to an agent whose history is still intact
 * spends prompt space on nothing — and worse, when a checkpoint's artifact
 * cannot be read, materialization deliberately falls back to the **full** log
 * and inserts no summary. Announcing a summary then would describe a context
 * the agent does not have: it would go hunting for detail that is already in
 * front of it, or tell the user history is missing when none is.
 *
 * The counts are concrete so the agent can judge how much it is missing rather
 * than guess.
 */
export async function compactionInstruction(
  context: TurnContext,
): Promise<Message | null> {
  const conversation = conversationOf(context);
  const checkpoint = await readActiveCheckpoint(conversation);
  if (checkpoint === null) {
    return null;
  }
  // Same read the prompt assembly does; if it comes back empty, assembly took
  // the full-history fallback and there is no summary to point at.
  //
  // A read that *throws* is the same situation and must not escape. This runs
  // while instructions are being assembled, before materialization gets its
  // chance to fall back — so propagating would fail every turn on a
  // conversation whose raw log is perfectly usable, over a notice that is
  // decoration.
  let summary: string | null = null;
  try {
    summary = await conversation.readArtifactText({
      artifactId: checkpoint.artifactId,
      version: checkpoint.artifactVersion,
    });
  } catch {
    return null;
  }
  if (summary === null || summary.length === 0) {
    return null;
  }
  return {
    role: "developer",
    content: `## Compacted history

The earlier part of this conversation no longer appears verbatim in your prompt. ${checkpoint.compactedEventCount} events were replaced by the summary shown above (${checkpoint.summaryChars} characters).

The raw history was not deleted. If you need detail the summary omits — an exact command, a full error, something the user said verbatim — query it with list_conversation_events rather than guessing or telling the user it is gone. read_compaction_summary returns the summary text in full, and describe_compaction reports the policy that produced it.`,
  };
}

function policyJson(policy: CompactionPolicy): JsonObject {
  return {
    enabled: policy.enabled,
    thresholdRatio: policy.thresholdRatio,
    keepRecentTurns: policy.keepRecentTurns,
    maxSummaryChars: policy.maxSummaryChars,
    summaryModel: policy.summaryModel,
    fallbackCharBudget: policy.fallbackCharBudget,
    isDefault:
      JSON.stringify(policy) === JSON.stringify(DEFAULT_COMPACTION_POLICY),
  };
}

function checkpointJson(checkpoint: CompactionCheckpoint): JsonObject {
  return {
    upToEventId: checkpoint.upToEventId,
    compactedEventCount: checkpoint.compactedEventCount,
    summaryChars: checkpoint.summaryChars,
    promptTokensBefore: checkpoint.promptTokensBefore,
    model: checkpoint.model,
    artifactPath: checkpoint.artifactPath,
    artifactVersion: checkpoint.artifactVersion,
    previousCheckpointId: checkpoint.previousCheckpointId,
  };
}
