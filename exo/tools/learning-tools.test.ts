import { describe, expect, it } from "vitest";

import {
  HarnessToolRegistry,
  createSkillToolInstances,
  type ArtifactVersion,
  type EventData,
  type JsonObject,
  type JsonValue,
  type TurnContext,
} from "@exo/harness";

import {
  LEARNING_LIFECYCLE_MARKER,
  learningInstruction,
  registerActivatedLearningTools,
  registerLearningTools,
} from "./learning-tools";

class FakeHandle {
  private versions: {
    artifactId: string;
    path: string;
    version: number;
    value: unknown;
  }[] = [];
  private sequence = 0;

  async listArtifacts(): Promise<ArtifactVersion[]> {
    return this.versions.map(({ artifactId, path, version }) => ({
      artifactId,
      path,
      version,
      createdAt: "1970-01-01T00:00:00Z",
      sizeBytes: 0,
    }));
  }

  async readArtifactJson<T>({
    artifactId,
    version,
  }: {
    artifactId: string;
    version?: number;
  }): Promise<T | null> {
    const selected = this.versions.find(
      (item) =>
        item.artifactId === artifactId &&
        (version === undefined || item.version === version),
    );
    return selected ? (selected.value as T) : null;
  }

  async writeArtifactJson({
    path,
    value,
  }: {
    path: string;
    value: JsonValue;
  }): Promise<ArtifactVersion> {
    this.sequence += 1;
    const version = this.sequence;
    const artifactId = `${path}@${version}`;
    this.versions.push({ artifactId, path, version, value });
    return {
      artifactId,
      path,
      version,
      createdAt: "1970-01-01T00:00:00Z",
      sizeBytes: 0,
    };
  }
}

class FakeConversation {
  readonly events: Array<{ turnId: string; data: EventData }> = [];
  readonly record = { id: "conversation-1" };

  async getEvents(query: {
    turnId?: string | null;
    types?: string[] | null;
  }): Promise<{ events: Array<{ turnId: string; data: EventData }> }> {
    return {
      events: this.events.filter(
        (event) =>
          (query.turnId == null || event.turnId === query.turnId) &&
          (query.types == null || query.types.includes(event.data.type)),
      ),
    };
  }
}

function makeContext(
  input: string,
  options: {
    agent?: FakeHandle;
    conversation?: FakeConversation;
    turnId?: string;
    shellExitCode?: number;
    enableAgentToolCreation?: boolean;
    shellCommands?: string[];
  } = {},
): TurnContext {
  const agent = options.agent ?? new FakeHandle();
  const conversation = options.conversation ?? new FakeConversation();
  const turnId = options.turnId ?? "turn-1";
  return {
    request: { input: [{ role: "user", content: input }] },
    agentConfig: {
      enableAgentToolCreation: options.enableAgentToolCreation ?? true,
    },
    exoharness: {
      current: {
        agent,
        conversation,
        turn: {
          record: { id: turnId },
          async addEvents(events: EventData[]) {
            conversation.events.push(
              ...events.map((data) => ({ turnId, data })),
            );
            return { eventIds: [], latestEventId: "event" };
          },
        },
      },
    },
    async executeTool(request: {
      functionName: string;
      arguments: JsonObject;
    }) {
      if (
        request.functionName === "shell" &&
        typeof request.arguments.command === "string"
      ) {
        options.shellCommands?.push(request.arguments.command);
      }
      return {
        exit_code: options.shellExitCode ?? 0,
        stdout: "validated\n",
        stderr: "",
      };
    },
  } as unknown as TurnContext;
}

function lifecycleTools(context: TurnContext): HarnessToolRegistry {
  const tools = new HarnessToolRegistry(context);
  registerLearningTools(tools, context);
  return tools;
}

async function normalTools(context: TurnContext): Promise<HarnessToolRegistry> {
  const tools = lifecycleTools(context);
  await registerActivatedLearningTools(tools, context);
  return tools;
}

async function call(
  tools: HarnessToolRegistry,
  name: string,
  args: JsonObject,
  context: TurnContext,
) {
  const tool = tools.get(name);
  if (tool === undefined) {
    throw new Error(`missing tool ${name}`);
  }
  return tool.handler.execute(args, { context });
}

function memoryProposal(): JsonObject {
  return {
    title: "FLINT output contract",
    evidence: "The Harbor verifier awarded 1.0 after byte-exact output checks.",
    expectedBenefit: "Avoid repeating newline and ordering mistakes.",
    activationDescription: "Tasks applying the FLINT records contract.",
    activationTerms: ["flint", "records"],
    minimumMatches: 2,
    memoryText: "FLINT records require byte-exact output with a final newline.",
  };
}

function lifecycleInput(): string {
  return `Feedback:
{"exception":null,"rewards":{"reward":1},"verifier_logs":{"reward.txt":"1"}}
Reflection instructions:
${LEARNING_LIFECYCLE_MARKER}`;
}

function reverseToolSource(): string {
  return `
import type { JsonObject, Tool, ToolResult } from "@exo/harness/tool";

const tool = {
  definition: {
    name: "lifecycle_reverse_text",
    description: "Reverse text for lifecycle validation.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: { text: { type: "string" } },
      required: ["text"],
    },
    outputSchema: {
      type: "object",
      additionalProperties: false,
      properties: { text: { type: "string" } },
      required: ["text"],
    },
  },
  initializationParameters: {
    type: "object",
    additionalProperties: false,
    properties: {},
  },
  initialize() {
    return {
      async execute(args: JsonObject): Promise<ToolResult> {
        const text = args.text;
        if (typeof text !== "string") throw new Error("text required");
        return { text: text.split("").reverse().join("") };
      },
    };
  },
} satisfies Tool;

export default tool;
`;
}

describe("learning lifecycle", () => {
  it("keeps proposals inactive until promotion, then activates matching learning once", async () => {
    const agent = new FakeHandle();
    const conversation = new FakeConversation();
    const reflection = makeContext(lifecycleInput(), {
      agent,
      conversation,
    });
    const tools = lifecycleTools(reflection);
    const proposed = (await call(
      tools,
      "propose_memory_learning",
      memoryProposal(),
      reflection,
    )) as { candidateId: string; status: string };

    expect(proposed.status).toBe("proposed");
    const before = makeContext("Apply FLINT to these records", {
      agent,
      conversation,
      turnId: "turn-2",
    });
    expect(await learningInstruction(before)).toBeNull();

    const promoted = (await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      reflection,
    )) as { ok: boolean; status: string };
    expect(promoted).toMatchObject({
      ok: true,
      status: "promoted",
      validation: {
        externalFeedback: {
          rewards: { reward: 1 },
          verifierLogsPresent: true,
          exceptionPresent: false,
        },
      },
    });

    const active = makeContext("Apply FLINT to these records", {
      agent,
      conversation,
      turnId: "turn-3",
    });
    const instruction = await learningInstruction(active);
    expect(String(instruction?.content)).toContain(
      "FLINT records require byte-exact output",
    );
    await learningInstruction(active);
    expect(
      conversation.events.filter(
        (event) =>
          event.turnId === "turn-3" && event.data.type === "learning_activated",
      ),
    ).toHaveLength(1);

    const listed = (await call(
      lifecycleTools(active),
      "list_learning_artifacts",
      {},
      active,
    )) as { candidates: Array<{ activationCount: number }> };
    expect(listed.candidates[0].activationCount).toBe(1);
  });

  it("rejects a skill whose sandbox validation command fails", async () => {
    const context = makeContext(lifecycleInput(), {
      shellExitCode: 7,
    });
    const tools = lifecycleTools(context);
    const proposal = {
      ...memoryProposal(),
      title: "FLINT normalization procedure",
      skill: {
        skillMd:
          "---\nname: flint-normalization\ndescription: Apply FLINT normalization when a task names the FLINT records contract.\n---\nVerify input and output bytes.",
        files: [],
        validationCommand: "test -f /app/ranked.txt",
      },
    } as JsonObject;
    const proposed = (await call(
      tools,
      "propose_skill_learning",
      proposal,
      context,
    )) as { candidateId: string };
    const result = (await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      context,
    )) as { ok: boolean; status: string; validation: { exitCode: number } };

    expect(result).toMatchObject({
      ok: false,
      status: "rejected",
      validation: { exitCode: 7 },
    });
  });

  it("rejects a skill proposal without standard frontmatter", async () => {
    const context = makeContext(lifecycleInput());
    const tools = lifecycleTools(context);

    await expect(
      call(
        tools,
        "propose_skill_learning",
        {
          ...memoryProposal(),
          title: "Malformed skill",
          skill: {
            skillMd: "Do something without frontmatter.",
            files: [],
            validationCommand: "true",
          },
        },
        context,
      ),
    ).rejects.toThrow("skillMd must contain YAML frontmatter");
  });

  it("keeps a failing skill out of durable state where prompt-only routing installs it immediately", async () => {
    const skillMd =
      "---\nname: broken-flint\ndescription: Apply a deliberately broken FLINT procedure.\n---\nWrite invalid output.";

    const promptOnlyAgent = new FakeHandle();
    const promptOnlyContext = makeContext("prompt-only reflection", {
      agent: promptOnlyAgent,
      shellExitCode: 7,
    });
    const promptOnlyInstaller = createSkillToolInstances().find(
      (tool) => tool.definition.name === "install_skill",
    );
    expect(promptOnlyInstaller).toBeDefined();
    const promptOnlyResult = await promptOnlyInstaller?.handler.execute(
      { skillMd, files: [] },
      { context: promptOnlyContext },
    );
    expect(promptOnlyResult).toMatchObject({ ok: true, name: "broken-flint" });
    expect(
      (await promptOnlyAgent.listArtifacts()).some(
        (artifact) => artifact.path === "skills/broken-flint.json",
      ),
    ).toBe(true);

    const lifecycleAgent = new FakeHandle();
    const lifecycleContext = makeContext(lifecycleInput(), {
      agent: lifecycleAgent,
      shellExitCode: 7,
    });
    const tools = lifecycleTools(lifecycleContext);
    const proposed = (await call(
      tools,
      "propose_skill_learning",
      {
        ...memoryProposal(),
        title: "Broken FLINT procedure",
        skill: {
          skillMd,
          files: [],
          validationCommand: "test -f /app/required-proof.txt",
        },
      },
      lifecycleContext,
    )) as { candidateId: string };
    const lifecycleResult = await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      lifecycleContext,
    );
    expect(lifecycleResult).toMatchObject({ ok: false, status: "rejected" });
    expect(
      (await lifecycleAgent.listArtifacts()).some(
        (artifact) => artifact.path === "skills/broken-flint.json",
      ),
    ).toBe(false);
  });

  it("does not leak promoted learning into unrelated tasks", async () => {
    const agent = new FakeHandle();
    const conversation = new FakeConversation();
    const reflection = makeContext(lifecycleInput(), {
      agent,
      conversation,
    });
    const tools = lifecycleTools(reflection);
    const proposed = (await call(
      tools,
      "propose_memory_learning",
      memoryProposal(),
      reflection,
    )) as { candidateId: string };
    await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      reflection,
    );

    const unrelated = makeContext("Fix an nginx configuration", {
      agent,
      conversation,
      turnId: "unrelated",
    });
    expect(await learningInstruction(unrelated)).toBeNull();
    expect(
      conversation.events.filter(
        (event) => event.data.type === "learning_activated",
      ),
    ).toHaveLength(0);
  });

  it("does not activate a short trigger inside a larger word", async () => {
    const agent = new FakeHandle();
    const reflection = makeContext(lifecycleInput(), { agent });
    const tools = lifecycleTools(reflection);
    const proposed = (await call(
      tools,
      "propose_memory_learning",
      {
        ...memoryProposal(),
        activationDescription: "Tasks about the Go language.",
        activationTerms: ["go"],
        minimumMatches: 1,
      },
      reflection,
    )) as { candidateId: string };
    await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      reflection,
    );

    expect(
      await learningInstruction(
        makeContext("Fix the Django application", { agent, turnId: "django" }),
      ),
    ).toBeNull();
    expect(
      await learningInstruction(
        makeContext("Fix the Go application", { agent, turnId: "go" }),
      ),
    ).not.toBeNull();
  });

  it("stages skill files before validation and promotes only after success", async () => {
    const shellCommands: string[] = [];
    const agent = new FakeHandle();
    const conversation = new FakeConversation();
    const context = makeContext(lifecycleInput(), {
      agent,
      conversation,
      shellCommands,
    });
    const tools = lifecycleTools(context);
    const proposed = (await call(
      tools,
      "propose_skill_learning",
      {
        ...memoryProposal(),
        title: "FLINT staged procedure",
        skill: {
          skillMd:
            "---\nname: flint-staged\ndescription: Apply the staged FLINT procedure when a task requests FLINT.\n---\nRun the bundled script.",
          files: [
            {
              path: "scripts/check.sh",
              contents: "#!/bin/sh\nprintf 'validated\\n'\n",
            },
          ],
          validationCommand: "sh scripts/check.sh",
        },
      },
      context,
    )) as { candidateId: string };

    const result = await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      context,
    );

    expect(result).toMatchObject({ ok: true, status: "promoted" });
    expect(shellCommands).toHaveLength(1);
    expect(shellCommands[0]).toContain("base64 --decode");
    expect(shellCommands[0]).toContain("sh scripts/check.sh");
    expect(shellCommands[0]).toContain("mktemp -d /tmp/exo-learning-skill");
    expect(
      (await agent.listArtifacts()).some((artifact) =>
        artifact.path.startsWith("skills/"),
      ),
    ).toBe(false);

    const future = makeContext("Apply FLINT to these records", {
      agent,
      conversation,
      turnId: "skill-use",
    });
    const futureTools = await normalTools(future);
    const skill = await call(
      futureTools,
      "use_learning_skill",
      { candidateId: proposed.candidateId },
      future,
    );
    expect(skill).toMatchObject({
      ok: true,
      candidateId: proposed.candidateId,
      name: "flint-staged",
    });
  });

  it("records an explicit discard without creating active learning", async () => {
    const agent = new FakeHandle();
    const context = makeContext(lifecycleInput(), { agent });
    const tools = lifecycleTools(context);
    const proposed = (await call(
      tools,
      "propose_learning_discard",
      {
        title: "One-off input path",
        evidence: "The path was unique to this Harbor container.",
        expectedBenefit: "Avoid polluting durable context.",
        discardReason: "Container-local paths do not transfer.",
      },
      context,
    )) as { candidateId: string };
    const result = (await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      context,
    )) as { ok: boolean; status: string };

    expect(result).toMatchObject({ ok: true, status: "discarded" });
    const future = makeContext("One-off input path", {
      agent,
      turnId: "future",
    });
    expect(await learningInstruction(future)).toBeNull();
  });

  it("rejects tool promotion when agent tool creation is disabled", async () => {
    const context = makeContext(lifecycleInput(), {
      enableAgentToolCreation: false,
    });
    const tools = lifecycleTools(context);
    const proposed = (await call(
      tools,
      "propose_tool_learning",
      {
        ...memoryProposal(),
        title: "FLINT deterministic transformer",
        tool: {
          moduleName: "flint-transform",
          toolName: "flint_transform",
          moduleSource: "export default { tools: [] };",
          apiKeyEnv: "",
          validationArgumentsJson: "{}",
          expectedResultJson: "{}",
        },
      },
      context,
    )) as { candidateId: string };
    const result = await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      context,
    );

    expect(result).toMatchObject({
      ok: false,
      status: "rejected",
      validation: { kind: "tool_creation_disabled" },
    });
  });

  it("rejects unsafe tool module paths before staging", async () => {
    const context = makeContext(lifecycleInput());
    const tools = lifecycleTools(context);

    await expect(
      call(
        tools,
        "propose_tool_learning",
        {
          ...memoryProposal(),
          title: "Unsafe module path",
          tool: {
            moduleName: "../../outside",
            toolName: "outside",
            moduleSource: reverseToolSource(),
            apiKeyEnv: "",
            validationArgumentsJson: "{}",
            expectedResultJson: "{}",
          },
        },
        context,
      ),
    ).rejects.toThrow("moduleName must contain only");
  });

  it("loads a tool in isolation and rejects it before install when its self-test differs", async () => {
    const context = makeContext(lifecycleInput());
    const tools = lifecycleTools(context);
    const proposed = (await call(
      tools,
      "propose_tool_learning",
      {
        ...memoryProposal(),
        title: "Lifecycle reverse tool",
        tool: {
          moduleName: "lifecycle-reverse-test",
          toolName: "lifecycle_reverse_text",
          moduleSource: reverseToolSource(),
          apiKeyEnv: "",
          validationArgumentsJson: '{"text":"abc"}',
          expectedResultJson: '{"text":"not-cba"}',
        },
      },
      context,
    )) as { candidateId: string };

    const result = await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      context,
    );

    expect(result).toMatchObject({
      ok: false,
      status: "rejected",
      validation: {
        kind: "tool_self_test",
        expected: { text: "not-cba" },
        actual: { text: "cba" },
      },
    });
  });

  it("registers a promoted tool only on matching tasks", async () => {
    const agent = new FakeHandle();
    const conversation = new FakeConversation();
    const context = makeContext(lifecycleInput(), { agent, conversation });
    const tools = lifecycleTools(context);
    const proposed = (await call(
      tools,
      "propose_tool_learning",
      {
        ...memoryProposal(),
        title: "Scoped lifecycle reverse tool",
        tool: {
          moduleName: "lifecycle-reverse-scoped",
          toolName: "lifecycle_reverse_text",
          moduleSource: reverseToolSource(),
          apiKeyEnv: "",
          validationArgumentsJson: '{"text":"abc"}',
          expectedResultJson: '{"text":"cba"}',
        },
      },
      context,
    )) as { candidateId: string };
    const promoted = await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      context,
    );
    expect(promoted).toMatchObject({ ok: true, status: "promoted" });

    const matching = makeContext("Apply FLINT to these records", {
      agent,
      conversation,
      turnId: "matching-tool",
    });
    const matchingTools = await normalTools(matching);
    expect(matchingTools.get("lifecycle_reverse_text")).toBeDefined();
    await expect(
      call(
        matchingTools,
        "lifecycle_reverse_text",
        { text: "router" },
        matching,
      ),
    ).resolves.toEqual({ text: "retuor" });

    const unrelated = makeContext("Count palette lines", {
      agent,
      conversation,
      turnId: "unrelated-tool",
    });
    const unrelatedTools = await normalTools(unrelated);
    expect(unrelatedTools.get("lifecycle_reverse_text")).toBeUndefined();
  });

  it("degrades safely when the learning catalog is corrupt", async () => {
    const agent = new FakeHandle();
    await agent.writeArtifactJson({
      path: "learning/index.json",
      value: { candidates: "invalid" },
    });
    const context = makeContext("Apply FLINT", { agent });

    const instruction = await learningInstruction(context);

    expect(String(instruction?.content)).toContain(
      "learning catalog could not be loaded",
    );
  });

  it("rejects a corrupt route-specific payload before activation", async () => {
    const agent = new FakeHandle();
    await agent.writeArtifactJson({
      path: "learning/index.json",
      value: {
        candidates: [
          {
            id: "learn-broken",
            route: "skill",
            title: "Broken skill",
            evidence: "claimed evidence",
            expectedBenefit: "none",
            activationDescription: "FLINT tasks",
            activationTerms: ["flint"],
            minimumMatches: 1,
            memoryText: null,
            skill: null,
            tool: null,
            discardReason: null,
            status: "promoted",
            createdAt: "2026-08-24T00:00:00Z",
            updatedAt: "2026-08-24T00:00:00Z",
            sourceConversationId: "conversation-1",
            activationCount: 0,
            lastActivatedAt: null,
            validation: {},
          },
        ],
      },
    });
    const context = makeContext("Apply FLINT", { agent });

    expect(String((await learningInstruction(context))?.content)).toContain(
      "learning catalog could not be loaded",
    );
    expect(
      (await normalTools(context)).get("use_learning_skill"),
    ).toBeUndefined();
  });

  it("rejects active learning when evaluator feedback is missing", async () => {
    const context = makeContext(LEARNING_LIFECYCLE_MARKER);
    const tools = lifecycleTools(context);
    const proposed = (await call(
      tools,
      "propose_memory_learning",
      memoryProposal(),
      context,
    )) as { candidateId: string };

    const result = await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      context,
    );

    expect(result).toMatchObject({
      ok: false,
      status: "rejected",
      validation: { kind: "missing_external_feedback" },
    });
  });

  it("accepts evaluator exception evidence when no numeric reward exists", async () => {
    const context = makeContext(`Feedback:
{"exception":{"message":"trial timed out"},"rewards":null,"verifier_logs":{}}
Reflection instructions:
${LEARNING_LIFECYCLE_MARKER}`);
    const tools = lifecycleTools(context);
    const proposed = (await call(
      tools,
      "propose_memory_learning",
      memoryProposal(),
      context,
    )) as { candidateId: string };

    const result = await call(
      tools,
      "validate_and_promote_learning",
      { candidateId: proposed.candidateId },
      context,
    );

    expect(result).toMatchObject({
      ok: true,
      status: "promoted",
      validation: {
        externalFeedback: {
          rewards: {},
          verifierLogsPresent: false,
          exceptionPresent: true,
        },
      },
    });
  });

  it("does not expose proposal or promotion tools outside lifecycle reflection", () => {
    const context = makeContext("normal task");
    const tools = lifecycleTools(context);
    expect(tools.get("list_learning_artifacts")).toBeDefined();
    expect(tools.get("propose_memory_learning")).toBeUndefined();
    expect(tools.get("propose_skill_learning")).toBeUndefined();
    expect(tools.get("propose_tool_learning")).toBeUndefined();
    expect(tools.get("propose_learning_discard")).toBeUndefined();
    expect(tools.get("validate_and_promote_learning")).toBeUndefined();
  });
});
