import {
  PromptHistoryCache,
  assistantMessagesText,
  createToolRegistry,
  registerAgentToolsFromDirectoryIfExists,
  registerBuiltInTools,
  registerLibraryToolModulePath,
  appendCustomEvent,
  COMPACTION_USAGE_EVENT,
  CompactionGate,
  estimatedTokensFromChars,
  resolveCompactionPolicy,
  runCompaction,
  summarizerMaxOutputTokens,
  turnMetadata,
  type BuiltInToolName,
  type CompactionPolicy,
  type EventData,
  type HarnessToolRegistry,
  type JsonObject,
  type Message,
  type SummarizeInput,
  type TurnContext,
} from "@exo/harness";
import {
  responseMessages,
  responseToLinguaEvents,
  responseCacheCreationTokens,
  responseToolCalls,
  responseUsageRecord,
  runtimeFromModelBinding,
  type NativeResponsesRequest,
  type ResponsesRuntimeLike,
  type TraceParent,
} from "@exo/model-runtime/responses";
import {
  ensureTable,
  getTable,
  inputOccupancy,
  maxInputTokens,
} from "@exo/model-runtime/cost";

import { resolveLlmBinding } from "./shared";

export interface ResponsesTurnLoopOptions {
  instructions?: (context: TurnContext) => Message[] | Promise<Message[]>;
  registerTools?: (
    tools: HarnessToolRegistry,
    context: TurnContext,
  ) => Promise<void> | void;
}

export async function runResponsesHarnessTurn(
  context: TurnContext,
  options: ResponsesTurnLoopOptions = {},
): Promise<void> {
  await ensureTable(); // load the price table once so cost is ready when events are built
  const modelBinding = await resolveLlmBinding(context);
  const runtime = runtimeFromModelBinding(context.agentConfig, modelBinding);
  await runtime.runTurn(context, (turnParent) =>
    runResponsesTurnLoop(
      runtime,
      context,
      turnParent,
      modelBinding.model,
      options,
    ),
  );
}

export async function createDefaultToolRegistry(
  context: TurnContext,
  builtInToolNames: BuiltInToolName[] = defaultBuiltInToolNames(context),
): Promise<HarnessToolRegistry> {
  const tools = createToolRegistry(context);
  registerBuiltInTools(tools, context, builtInToolNames);
  for (const modulePath of context.agentConfig.typescript?.toolModulePaths ??
    []) {
    await registerLibraryToolModulePath(tools, context, modulePath);
  }
  if (context.agentConfig.enableAgentToolCreation) {
    await registerAgentToolsFromDirectoryIfExists(tools, context);
  }
  return tools;
}

export function defaultBuiltInToolNames(
  context: TurnContext,
): BuiltInToolName[] {
  const names: BuiltInToolName[] = ["shell"];
  if (context.agentConfig.enableAgentToolCreation) {
    names.push("install_agent_tool", "uninstall_agent_tool");
  }
  return names;
}

export function basicHarnessInstructions(context: TurnContext): Message[] {
  return context.agentConfig.enableAgentToolCreation
    ? [...context.agentConfig.instructions, agentToolCreationInstruction()]
    : context.agentConfig.instructions;
}

export function agentToolCreationInstruction(): Message {
  return {
    role: "developer",
    content:
      "Agent-created tools are supported. When the user asks you to create a reusable tool, call install_agent_tool with a complete TypeScript moduleSource. Do not claim the tool was created unless install_agent_tool returns ok: true. The moduleSource must use type-only imports from @exo/harness/tool and default-export a Tool using { definition, initializationParameters, initialize(...) } satisfies Tool; definition.parameters must be a strict JSON schema object with additionalProperties: false; handlers must implement execute(args, execution), not invoke or call. Do not use zod, inputSchema, external npm packages, or runtime imports from @exo/harness/tool. After install_agent_tool succeeds, the new tool is available in the next model round of the same turn, so use it directly rather than falling back to shell. Use uninstall_agent_tool to remove an agent-created tool that is obsolete or conflicts with another tool name.",
  };
}

async function runResponsesTurnLoop(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  turnParent: TraceParent,
  model: string,
  options: ResponsesTurnLoopOptions,
): Promise<string | null> {
  const { conversation } = context.exoharness.current;
  const maxToolRoundTrips = context.agentConfig.maxToolRoundTrips;
  const policy = resolveCompactionPolicy(context.agentConfig.compaction);
  // One cache per turn: the loop materializes every round, so re-reading the
  // whole event log each time makes a turn cost O(rounds x events).
  const history = new PromptHistoryCache();
  const compaction = new CompactionGate();
  // Compaction is latched to once per turn, so at most one of these exists.
  const summarizerUsage: { usage: JsonObject | undefined } = {
    usage: undefined,
  };
  let latestEventId: string | null = null;

  for (let round = 0; ; round += 1) {
    if (
      maxToolRoundTrips !== null &&
      maxToolRoundTrips !== undefined &&
      round > maxToolRoundTrips
    ) {
      return latestEventId;
    }

    const tools = options.registerTools
      ? createToolRegistry(context)
      : await createDefaultToolRegistry(context);
    if (options.registerTools) {
      await options.registerTools(tools, context);
    }
    const instructions = options.instructions
      ? await options.instructions(context)
      : basicHarnessInstructions(context);
    let messages = [
      ...instructions,
      ...(await history.materialize(conversation)),
    ];

    // Compact *before* sending when the prompt already looks too large.
    //
    // The post-response trigger below is more accurate — it uses the provider's
    // own counts — but it only ever runs after a successful call. A prompt
    // already past the model's hard limit is rejected outright, and that error
    // leaves the turn before anything can shrink the history responsible for it;
    // every later turn then replays the same oversized log and fails the same
    // way. This check is what makes that state recoverable. Mirrors the Rust
    // executor's pre-request trigger.
    const toolDefinitions = tools.definitions();
    const preflightChars =
      promptChars(messages) + toolDefinitionChars(toolDefinitions);
    if (
      compaction.shouldAttempt({
        policy,
        promptTokens: estimatedTokensFromChars(preflightChars),
        maxInputTokens: maxInputTokens(getTable() ?? new Map(), model),
        materializedChars: preflightChars,
      })
    ) {
      compaction.markAttempted();
      const result = await runCompaction({
        conversation,
        turn: context.exoharness.current.turn,
        policy,
        model: policy.summaryModel ?? model,
        promptTokensBefore: null,
        summarize: (input) =>
          summarizeWithModel(
            runtime,
            policy,
            model,
            turnParent,
            round,
            input,
            summarizerUsage,
          ),
      });
      await recordSummarizerUsage(context, summarizerUsage.usage);
      summarizerUsage.usage = undefined;
      if (result.status === "compacted") {
        // The checkpoint just written replaces the prefix this prompt was built
        // from, so rebuild it before sending.
        history.invalidate();
        messages = [
          ...instructions,
          ...(await history.materialize(conversation)),
        ];
      }
    }

    const request: NativeResponsesRequest = {
      model,
      messages,
      tools: toolDefinitions,
      maxOutputTokens: context.agentConfig.maxOutputTokens,
      metadata: turnMetadata(context),
    };

    const response = context.streaming
      ? await runtime.completeStream(
          request,
          {
            onFirstChunk: (ttftMs) => context.stream.firstChunk(ttftMs),
            onTextDelta: (text) => context.stream.text(text),
          },
          {
            parent: turnParent,
            roundIndex: round,
          },
        )
      : await runtime.complete(request, {
          parent: turnParent,
          roundIndex: round,
        });

    const events = responseToLinguaEvents(response);
    if (events.length > 0) {
      latestEventId = await appendTurnEvents(context, events);
    }

    // Compact between rounds, using the token count the provider just reported.
    // Doing it here rather than at turn start means a single runaway turn can
    // still bring its own prompt back under the limit.
    const table = getTable() ?? new Map();
    const modelInputLimit = maxInputTokens(table, model);
    // Occupancy, not `input_tokens`: on Anthropic-family providers the latter
    // counts only the fresh slice, so a heavily cached prompt that fills the
    // window reports a tiny number and would never trip the threshold.
    const occupancy = response.usage
      ? inputOccupancy(table, model, {
          prompt: response.usage.input_tokens,
          completion: response.usage.output_tokens,
          cached: response.usage.input_tokens_details?.cached_tokens,
          cacheCreation: responseCacheCreationTokens(response),
        })
      : null;
    if (
      compaction.shouldAttempt({
        policy,
        promptTokens: occupancy,
        maxInputTokens: modelInputLimit,
        // Only the fallback trigger reads this, so skip walking the whole
        // prompt when the price table knows the model's limit.
        materializedChars: modelInputLimit === null ? promptChars(messages) : 0,
      })
    ) {
      compaction.markAttempted();
      const result = await runCompaction({
        conversation,
        turn: context.exoharness.current.turn,
        policy,
        model: policy.summaryModel ?? model,
        promptTokensBefore: occupancy,
        summarize: (input) =>
          summarizeWithModel(
            runtime,
            policy,
            model,
            turnParent,
            round,
            input,
            summarizerUsage,
          ),
      });
      await recordSummarizerUsage(context, summarizerUsage.usage);
      summarizerUsage.usage = undefined;
      if (result.status === "compacted") {
        // The cache holds exactly the prefix that was just replaced.
        history.invalidate();
      }
    }

    const toolCalls = responseToolCalls(response);
    const hasSyntheticToolResult = events.some(
      (event) => event.type === "tool_result",
    );
    if (toolCalls.length === 0) {
      if (hasSyntheticToolResult) {
        continue;
      }
      return latestEventId;
    }

    for (const toolCall of toolCalls) {
      const toolResultEvents = await runtime.traceToolCall(
        turnParent,
        context,
        toolCall,
        round,
        (toolCall) => tools.executePending([toolCall]),
      );
      if (toolResultEvents.length > 0) {
        latestEventId = await appendTurnEvents(context, toolResultEvents);
      }
    }
  }
}

async function appendTurnEvents(
  context: TurnContext,
  data: EventData[],
): Promise<string> {
  return (await context.exoharness.current.turn.addEvents(data)).latestEventId;
}

/**
 * Write what the compaction summarizer cost.
 *
 * On its own custom event, which prompt assembly ignores — so unlike a
 * `messages` event it is safe to write at any point, including while this or
 * another turn has a tool call outstanding. See `COMPACTION_USAGE_EVENT`.
 */
async function recordSummarizerUsage(
  context: TurnContext,
  usage: JsonObject | undefined,
): Promise<void> {
  if (usage === undefined) {
    return;
  }
  try {
    await appendCustomEvent(
      context.exoharness.current.turn,
      COMPACTION_USAGE_EVENT,
      usage,
    );
  } catch {
    // Accounting is not worth failing a turn over.
  }
}

function promptChars(messages: Message[]): number {
  let total = 0;
  for (const message of messages) {
    total +=
      typeof message.content === "string"
        ? message.content.length
        : JSON.stringify(message.content).length;
  }
  return total;
}

/**
 * Serialized size of the tool schemas sent with a request.
 *
 * Tools go into the same input window as the messages, and this harness
 * registers a lot of them. Sizing a request by its messages alone lets a
 * conversation sit under the compaction threshold on message text while the
 * request that actually goes out is over the model's hard limit — which is the
 * unrecoverable failure the preflight exists to prevent.
 */
function toolDefinitionChars(tools: unknown[]): number {
  let total = 0;
  for (const tool of tools) {
    total += JSON.stringify(tool)?.length ?? 0;
  }
  return total;
}

const SUMMARIZER_INSTRUCTION = `You are compacting the earlier portion of an agent conversation so it can be dropped from the prompt while remaining usable.

Write a dense factual summary of what happened. Prioritise, in order:
1. Decisions made and conclusions reached, with the reasoning that led to them.
2. Durable facts about the user, the task, and the environment.
3. Work completed, files or resources changed, and commands that mattered.
4. Open threads: what was in progress, what failed, what was agreed for later.

Rules:
- Write in the third person about what "the user" and "the agent" did.
- Preserve specifics: names, paths, ids, numbers, error messages. Those are what a summary usually loses and what is most expensive to lose.
- Do not speculate or add anything not present in the material.
- Do not address the reader or describe the summary itself. Output only the summary.`;

/**
 * Summarize a compacted span with a model call carrying no tools.
 *
 * When a previous summary exists it is merged rather than appended, so a long
 * conversation converges on a fixed-size summary instead of accumulating one
 * paragraph per compaction.
 */
async function summarizeWithModel(
  runtime: ResponsesRuntimeLike,
  policy: CompactionPolicy,
  model: string,
  turnParent: TraceParent,
  round: number,
  input: SummarizeInput,
  // Filled with what this call cost. The caller writes it at a point where no
  // tool call is outstanding — see `flushSummarizerUsage`.
  usageSink: { usage: JsonObject | undefined },
): Promise<string> {
  const merge =
    input.previousSummary === null
      ? ""
      : `\n\nA summary of even earlier history is provided first. Merge it with the new material into a single summary that covers both — do not simply append, and do not drop facts from the earlier summary.\n\n<earlier_summary>\n${input.previousSummary}\n</earlier_summary>`;

  const response = await runtime.complete(
    {
      model: policy.summaryModel ?? model,
      messages: [
        {
          role: "developer",
          content: `${SUMMARIZER_INSTRUCTION}${merge}\n\nKeep the summary under ${input.maxChars} characters.`,
        },
        ...input.messages,
      ],
      // No tools: the summarizer reads, it does not act.
      tools: [],
      // Bound the response at request time. `capSummary` truncates only after
      // generation, so without this a runaway summary is paid for in full
      // before being thrown away.
      maxOutputTokens: summarizerMaxOutputTokens(input.maxChars),
    },
    { parent: turnParent, roundIndex: round },
  );
  usageSink.usage = responseUsageRecord(response);
  return assistantMessagesText(responseMessages(response));
}
