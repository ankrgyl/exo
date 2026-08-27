import { randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

import {
  loadAgentTool,
  parseSkillFrontmatter,
  type Agent,
  type ArtifactVersion,
  type HarnessToolRegistry as ToolRegistry,
  type JsonObject,
  type JsonValue,
  type Message,
  type ToolInstance,
  type ToolResult,
  type TurnContext,
} from "@exo/harness";

import {
  classifyLearningRoute,
  enforceLearningRoute,
  lessonFeaturesFromUnknown,
  type LearningRoute,
} from "./learning-router";

export const LEARNING_LIFECYCLE_MARKER = "EXO_LEARNING_LIFECYCLE_V1";

const LEARNING_INDEX_PATH = "learning/index.json";
const MAX_CANDIDATES = 100;
const MAX_TITLE_CHARS = 120;
const MAX_EVIDENCE_CHARS = 2_000;
const MAX_EXPECTED_BENEFIT_CHARS = 1_000;
const MAX_ACTIVATION_DESCRIPTION_CHARS = 500;
const MAX_ACTIVATION_TERMS = 8;
const MAX_ACTIVATION_TERM_CHARS = 80;
const MAX_MEMORY_TEXT_CHARS = 600;
const MAX_SKILL_MD_CHARS = 100_000;
const MAX_TOOL_SOURCE_CHARS = 100_000;
const MAX_VALIDATION_COMMAND_CHARS = 4_000;

type LearningStatus = "proposed" | "promoted" | "rejected" | "discarded";

interface SkillPayload {
  skillMd: string;
  files: Array<{ path: string; contents: string }>;
  validationCommand: string;
}

interface ToolPayload {
  moduleName: string;
  toolName: string;
  moduleSource: string;
  apiKeyEnv: string | null;
  validationArguments: JsonObject;
  expectedResult: JsonValue;
}

interface LearningCandidate {
  id: string;
  route: LearningRoute;
  title: string;
  evidence: string;
  expectedBenefit: string;
  activationDescription: string;
  activationTerms: string[];
  minimumMatches: number;
  memoryText: string | null;
  skill: SkillPayload | null;
  tool: ToolPayload | null;
  discardReason: string | null;
  status: LearningStatus;
  createdAt: string;
  updatedAt: string;
  sourceConversationId: string;
  activationCount: number;
  lastActivatedAt: string | null;
  validation: JsonObject | null;
}

interface LearningIndex {
  candidates: LearningCandidate[];
}

type LearningHandle = Pick<
  Agent,
  "listArtifacts" | "readArtifactJson" | "writeArtifactJson"
>;

export function isLearningLifecycleTurn(context: TurnContext): boolean {
  return requestText(context).includes(LEARNING_LIFECYCLE_MARKER);
}

export function registerLearningTools(
  registry: ToolRegistry,
  context: TurnContext,
): void {
  registry.register(listLearningArtifactsTool());
  if (!isLearningLifecycleTurn(context)) {
    return;
  }
  for (const tool of proposalTools()) {
    registry.register(tool);
  }
  registry.register(classifyLearningRouteTool());
  registry.register(validateAndPromoteLearningTool());
}

export async function registerActivatedLearningTools(
  registry: ToolRegistry,
  context: TurnContext,
): Promise<void> {
  if (isLearningLifecycleTurn(context)) {
    return;
  }
  let index: LearningIndex;
  try {
    index = await readIndex(learningHandle(context));
  } catch {
    return;
  }
  const input = requestText(context).toLowerCase();
  const activated = index.candidates.filter(
    (candidate) =>
      candidate.status === "promoted" && activationMatches(candidate, input),
  );
  const skills = activated.filter(
    (candidate) => candidate.route === "skill" && candidate.skill !== null,
  );
  if (skills.length > 0) {
    registry.register(activatedSkillLoaderTool(skills));
  }
  for (const candidate of activated) {
    if (candidate.route !== "tool" || candidate.tool === null) {
      continue;
    }
    try {
      registry.register(await loadCandidateTool(candidate.tool, context));
    } catch (error) {
      console.error(
        `validated learning tool ${candidate.id} failed to load: ${errorText(error)}`,
      );
    }
  }
}

export async function learningInstruction(
  context: TurnContext,
): Promise<Message | null> {
  if (isLearningLifecycleTurn(context)) {
    return null;
  }
  const handle = learningHandle(context);
  let index: LearningIndex;
  try {
    index = await readIndex(handle);
  } catch (error) {
    return {
      role: "developer",
      content: `The validated learning catalog could not be loaded and no learning was activated: ${errorText(error)}`,
    };
  }
  const promoted = index.candidates.filter(
    (candidate) => candidate.status === "promoted",
  );
  if (promoted.length === 0) {
    return null;
  }

  const input = requestText(context).toLowerCase();
  const activated = promoted.filter((candidate) =>
    activationMatches(candidate, input),
  );
  if (activated.length === 0) {
    return null;
  }

  if (!(await activationAlreadyRecorded(context))) {
    const now = new Date().toISOString();
    for (const candidate of activated) {
      candidate.activationCount += 1;
      candidate.lastActivatedAt = now;
      candidate.updatedAt = now;
    }
    await writeIndex(handle, index);
    await context.exoharness.current.turn.addEvents([
      {
        type: "learning_activated",
        artifacts: activated.map((candidate) => ({
          id: candidate.id,
          route: candidate.route,
          title: candidate.title,
        })),
      },
    ]);
  }

  return {
    role: "developer",
    content: `Validated learning automatically activated for this task. Apply it before choosing a fresh approach. These artifacts passed their promotion checks:\n\n${activated
      .map(activatedCandidateText)
      .join("\n\n")}`,
  };
}

function proposalTools(): ToolInstance[] {
  return [
    proposalTool(
      "memory",
      "propose_memory_learning",
      "Propose a concise stable fact or heuristic as scoped memory. It remains inactive until validate_and_promote_learning succeeds.",
      { memoryText: { type: "string" } },
      ["memoryText"],
    ),
    proposalTool(
      "skill",
      "propose_skill_learning",
      "Propose a reusable multi-step procedure as a skill. It remains inactive and is not installed until its sandbox validation command passes.",
      {
        skill: {
          type: "object",
          additionalProperties: false,
          properties: {
            skillMd: { type: "string" },
            files: {
              type: "array",
              items: {
                type: "object",
                additionalProperties: false,
                properties: {
                  path: { type: "string" },
                  contents: { type: "string" },
                },
                required: ["path", "contents"],
              },
            },
            validationCommand: { type: "string" },
          },
          required: ["skillMd", "files", "validationCommand"],
        },
      },
      ["skill"],
    ),
    proposalTool(
      "tool",
      "propose_tool_learning",
      "Propose a repeated deterministic operation as an executable tool. It remains inactive until installation and its exact JSON self-test succeed.",
      {
        tool: {
          type: "object",
          additionalProperties: false,
          properties: {
            moduleName: { type: "string" },
            toolName: { type: "string" },
            moduleSource: { type: "string" },
            apiKeyEnv: {
              type: "string",
              description:
                "Required environment variable name, or an empty string when none.",
            },
            validationArgumentsJson: { type: "string" },
            expectedResultJson: { type: "string" },
          },
          required: [
            "moduleName",
            "toolName",
            "moduleSource",
            "apiKeyEnv",
            "validationArgumentsJson",
            "expectedResultJson",
          ],
        },
      },
      ["tool"],
    ),
    proposalTool(
      "discard",
      "propose_learning_discard",
      "Record an evidence-backed decision that a task-specific, redundant, or unsupported lesson must not become active learning.",
      { discardReason: { type: "string" } },
      ["discardReason"],
    ),
  ];
}

function proposalTool(
  route: LearningRoute,
  name: string,
  description: string,
  routeProperties: JsonObject,
  routeRequired: string[],
): ToolInstance {
  const active = route !== "discard";
  const properties: JsonObject = {
    title: { type: "string" },
    evidence: { type: "string" },
    expectedBenefit: { type: "string" },
    ...(active
      ? {
          activationDescription: { type: "string" },
          activationTerms: {
            type: "array",
            items: { type: "string" },
          },
          minimumMatches: { type: "integer" },
        }
      : {}),
    ...routeProperties,
  };
  const required = [
    "title",
    "evidence",
    "expectedBenefit",
    ...(active
      ? ["activationDescription", "activationTerms", "minimumMatches"]
      : []),
    ...routeRequired,
  ];
  return {
    source: "library",
    definition: {
      name,
      description,
      parameters: {
        type: "object",
        additionalProperties: false,
        properties,
        required,
      },
    },
    handler: {
      async execute(args, execution): Promise<ToolResult> {
        const classification = classifyLearningRoute(
          lessonFeaturesFromUnknown(args),
        );
        const enforced = enforceLearningRoute(route, classification);
        if (!enforced.accepted) {
          return {
            ok: false,
            error: "route_conflict",
            proposedRoute: enforced.proposedRoute,
            suggestedRoute: enforced.route,
            corrected: enforced.corrected,
            reasons: enforced.reasons,
            scores: classification.scores,
          };
        }
        const candidate = parseCandidate(
          route,
          args,
          execution.context.exoharness.current.conversation.record.id,
        );
        const handle = learningHandle(execution.context);
        const index = await readIndex(handle);
        const duplicate = index.candidates.find(
          (existing) =>
            candidateFingerprint(existing) === candidateFingerprint(candidate),
        );
        if (duplicate !== undefined) {
          return {
            ok: true,
            candidateId: duplicate.id,
            route: duplicate.route,
            status: duplicate.status,
            duplicate: true,
          };
        }
        if (index.candidates.length >= MAX_CANDIDATES) {
          return {
            ok: false,
            error: `learning catalog is full at ${MAX_CANDIDATES} candidates`,
          };
        }
        index.candidates.push(candidate);
        await writeIndex(handle, index);
        return {
          ok: true,
          candidateId: candidate.id,
          route: candidate.route,
          status: candidate.status,
          duplicate: false,
        };
      },
    },
  };
}

function classifyLearningRouteTool(): ToolInstance {
  return {
    source: "library",
    definition: {
      name: "classify_learning_route",
      description:
        "Classify a lesson into memory, skill, tool, or discard from checkable features. Use this before proposing. Conflicting proposal tools are rejected.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          title: { type: "string" },
          evidence: { type: "string" },
          expectedBenefit: { type: "string" },
          memoryText: { type: "string" },
          skillMd: { type: "string" },
          toolSource: { type: "string" },
          discardReason: { type: "string" },
        },
        required: ["title", "evidence", "expectedBenefit"],
      },
    },
    handler: {
      async execute(args): Promise<ToolResult> {
        const classification = classifyLearningRoute(
          lessonFeaturesFromUnknown({
            ...args,
            skill: { skillMd: args.skillMd },
            tool: { moduleSource: args.toolSource },
          }),
        );
        return {
          ok: true,
          ...classification,
        };
      },
    },
  };
}

function validateAndPromoteLearningTool(): ToolInstance {
  return {
    source: "library",
    definition: {
      name: "validate_and_promote_learning",
      description:
        "Validate one proposed learning candidate and make it active only if its route-specific checks pass. Skills must pass their sandbox command; tools must install and return the exact declared self-test result. Discard candidates become terminal without creating an active artifact.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          candidateId: { type: "string" },
        },
        required: ["candidateId"],
      },
    },
    handler: {
      async execute(args, execution): Promise<ToolResult> {
        const candidateId = requiredString(args.candidateId, "candidateId", 80);
        const handle = learningHandle(execution.context);
        const index = await readIndex(handle);
        const candidate = index.candidates.find(
          (item) => item.id === candidateId,
        );
        if (candidate === undefined) {
          return {
            ok: false,
            candidateId,
            error: "unknown learning candidate",
          };
        }
        if (candidate.status !== "proposed") {
          return {
            ok:
              candidate.status === "promoted" ||
              candidate.status === "discarded",
            candidateId,
            route: candidate.route,
            status: candidate.status,
            alreadyTerminal: true,
          };
        }

        let outcome: PromotionOutcome;
        try {
          outcome = await promoteCandidate(candidate, execution.context);
        } catch (error) {
          outcome = {
            status: "rejected",
            validation: {
              kind: "executor_error",
              error: errorText(error),
            },
          };
        }
        candidate.status = outcome.status;
        candidate.validation = outcome.validation;
        candidate.updatedAt = new Date().toISOString();
        await writeIndex(handle, index);
        const ok =
          outcome.status === "promoted" || outcome.status === "discarded";
        return {
          ok,
          candidateId,
          route: candidate.route,
          status: outcome.status,
          validation: outcome.validation,
          ...(ok ? {} : { error: "learning candidate failed validation" }),
        };
      },
    },
  };
}

function listLearningArtifactsTool(): ToolInstance {
  return {
    source: "library",
    definition: {
      name: "list_learning_artifacts",
      description:
        "List learning candidates, their lifecycle status, activation triggers, validation evidence, and activation counts. Candidate source content is omitted.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {},
        required: [],
      },
    },
    handler: {
      async execute(_args, execution): Promise<ToolResult> {
        const index = await readIndex(learningHandle(execution.context));
        return {
          ok: true,
          candidates: index.candidates.map((candidate) => ({
            id: candidate.id,
            route: candidate.route,
            title: candidate.title,
            status: candidate.status,
            evidence: candidate.evidence,
            expectedBenefit: candidate.expectedBenefit,
            activationDescription: candidate.activationDescription,
            activationTerms: candidate.activationTerms,
            minimumMatches: candidate.minimumMatches,
            activationCount: candidate.activationCount,
            lastActivatedAt: candidate.lastActivatedAt,
            validation: candidate.validation,
          })),
        };
      },
    },
  };
}

function activatedSkillLoaderTool(
  candidates: LearningCandidate[],
): ToolInstance {
  return {
    source: "library",
    definition: {
      name: "use_learning_skill",
      description: `Load one validated skill activated for this task. Available candidates: ${candidates
        .map((candidate) => `${candidate.id} (${candidate.title})`)
        .join(", ")}.`,
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          candidateId: { type: "string" },
        },
        required: ["candidateId"],
      },
    },
    handler: {
      async execute(args): Promise<ToolResult> {
        const candidateId = requiredString(args.candidateId, "candidateId", 80);
        const candidate = candidates.find((item) => item.id === candidateId);
        if (candidate === undefined || candidate.skill === null) {
          return {
            ok: false,
            error: "skill candidate is not activated for this task",
          };
        }
        const name =
          parseSkillFrontmatter(candidate.skill.skillMd)?.fields.name ??
          candidate.title;
        return {
          ok: true,
          candidateId,
          name,
          skillMd: candidate.skill.skillMd,
          files: candidate.skill.files,
        };
      },
    },
  };
}

interface PromotionOutcome {
  status: "promoted" | "rejected" | "discarded";
  validation: JsonObject;
}

async function promoteCandidate(
  candidate: LearningCandidate,
  context: TurnContext,
): Promise<PromotionOutcome> {
  if (candidate.route === "discard") {
    return {
      status: "discarded",
      validation: {
        kind: "discard",
        reason: candidate.discardReason ?? "task-specific or unsupported",
      },
    };
  }
  const externalFeedback = externalFeedbackEvidence(context);
  if (externalFeedback === null) {
    return {
      status: "rejected",
      validation: {
        kind: "missing_external_feedback",
        error:
          "active learning requires the evaluator feedback payload from this reflection",
      },
    };
  }
  if (candidate.route === "memory") {
    return {
      status: "promoted",
      validation: {
        kind: "evidence_gate",
        evidencePresent: candidate.evidence.length > 0,
        externalFeedback,
        scopedActivation: true,
      },
    };
  }
  if (candidate.route === "skill") {
    if (candidate.skill === null) {
      throw new Error("skill candidate has no skill payload");
    }
    const shell = await executeShell(
      context,
      stagedSkillValidationCommand(candidate.skill),
    );
    if (shell.exitCode !== 0) {
      return {
        status: "rejected",
        validation: {
          kind: "sandbox_command",
          command: candidate.skill.validationCommand,
          exitCode: shell.exitCode,
          externalFeedback,
          stdout: truncate(shell.stdout, 2_000),
          stderr: truncate(shell.stderr, 2_000),
        },
      };
    }
    return {
      status: "promoted",
      validation: {
        kind: "sandbox_command",
        command: candidate.skill.validationCommand,
        exitCode: shell.exitCode,
        externalFeedback,
        stdout: truncate(shell.stdout, 2_000),
        publishedToLearningCatalog: true,
      },
    };
  }

  if (candidate.tool === null) {
    throw new Error("tool candidate has no tool payload");
  }
  if (!context.agentConfig.enableAgentToolCreation) {
    return {
      status: "rejected",
      validation: {
        kind: "tool_creation_disabled",
        externalFeedback,
        error: "agent tool creation must be enabled to promote a tool",
      },
    };
  }
  const tool = await loadCandidateTool(candidate.tool, context);
  if (tool.definition.name !== candidate.tool.toolName) {
    return {
      status: "rejected",
      validation: {
        kind: "tool_self_test",
        externalFeedback,
        error: `proposed module exports ${tool.definition.name}, expected ${candidate.tool.toolName}`,
      },
    };
  }
  const actual = await tool.handler.execute(
    candidate.tool.validationArguments,
    {
      context,
    },
  );
  if (canonicalJson(actual) !== canonicalJson(candidate.tool.expectedResult)) {
    return {
      status: "rejected",
      validation: {
        kind: "tool_self_test",
        externalFeedback,
        expected: candidate.tool.expectedResult,
        actual,
      },
    };
  }
  return {
    status: "promoted",
    validation: {
      kind: "tool_self_test",
      externalFeedback,
      toolName: candidate.tool.toolName,
      expected: candidate.tool.expectedResult,
      actual,
      publishedToLearningCatalog: true,
    },
  };
}

async function loadCandidateTool(
  tool: ToolPayload,
  context: TurnContext,
): Promise<ToolInstance> {
  const temporaryDirectory = await fs.mkdtemp(
    path.join(os.tmpdir(), "exo-learning-tool."),
  );
  try {
    const modulePath = path.join(temporaryDirectory, `${tool.moduleName}.ts`);
    await fs.writeFile(modulePath, tool.moduleSource, "utf8");
    const initialization: JsonObject =
      tool.apiKeyEnv === null ? {} : { apiKeyEnv: tool.apiKeyEnv };
    return await loadAgentTool(context, modulePath, initialization);
  } finally {
    await fs.rm(temporaryDirectory, { recursive: true, force: true });
  }
}

function externalFeedbackEvidence(context: TurnContext): JsonObject | null {
  const text = requestText(context);
  const feedbackMarker = "Feedback:\n";
  const instructionsMarker = "\nReflection instructions:";
  const start = text.lastIndexOf(feedbackMarker);
  const end = text.indexOf(instructionsMarker, start + feedbackMarker.length);
  if (start < 0 || end < 0) {
    return null;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(
      text.slice(start + feedbackMarker.length, end).trim(),
    ) as unknown;
  } catch {
    return null;
  }
  if (!isRecord(parsed)) {
    return null;
  }
  const rewards: JsonObject = {};
  if (isRecord(parsed.rewards)) {
    for (const [name, value] of Object.entries(parsed.rewards)) {
      if (typeof value === "number" && Number.isFinite(value)) {
        rewards[name] = value;
      }
    }
  }
  const verifierLogsPresent =
    isRecord(parsed.verifier_logs) &&
    Object.keys(parsed.verifier_logs).length > 0;
  const exceptionPresent =
    parsed.exception !== null && parsed.exception !== undefined;
  if (
    Object.keys(rewards).length === 0 &&
    !verifierLogsPresent &&
    !exceptionPresent
  ) {
    return null;
  }
  return {
    rewards,
    verifierLogsPresent,
    exceptionPresent,
  };
}

function parseCandidate(
  route: LearningRoute,
  args: JsonObject,
  conversationId: string,
): LearningCandidate {
  const title = requiredString(args.title, "title", MAX_TITLE_CHARS);
  const evidence = requiredString(
    args.evidence,
    "evidence",
    MAX_EVIDENCE_CHARS,
  );
  const expectedBenefit = requiredString(
    args.expectedBenefit,
    "expectedBenefit",
    MAX_EXPECTED_BENEFIT_CHARS,
  );
  const activationDescription =
    route === "discard"
      ? "Never activate; this lesson was explicitly discarded."
      : requiredString(
          args.activationDescription,
          "activationDescription",
          MAX_ACTIVATION_DESCRIPTION_CHARS,
        );
  const activationTerms =
    route === "discard"
      ? []
      : stringArray(args.activationTerms, "activationTerms");
  if (activationTerms.length > MAX_ACTIVATION_TERMS) {
    throw new Error(`activationTerms exceeds ${MAX_ACTIVATION_TERMS} entries`);
  }
  const normalizedTerms = [
    ...new Set(
      activationTerms.map((term) =>
        requiredString(
          term,
          "activation term",
          MAX_ACTIVATION_TERM_CHARS,
        ).toLowerCase(),
      ),
    ),
  ];
  const minimumMatches =
    route === "discard" ? 0 : integer(args.minimumMatches, "minimumMatches");
  if (
    route !== "discard" &&
    (normalizedTerms.length === 0 ||
      minimumMatches < 1 ||
      minimumMatches > normalizedTerms.length)
  ) {
    throw new Error(
      "active candidates require activation terms and minimumMatches within their count",
    );
  }

  const memoryText =
    route === "memory"
      ? requiredString(args.memoryText, "memoryText", MAX_MEMORY_TEXT_CHARS)
      : null;
  const skill = route === "skill" ? parseSkillPayload(args.skill) : null;
  const tool = route === "tool" ? parseToolPayload(args.tool) : null;
  const discardReason =
    route === "discard"
      ? requiredString(args.discardReason, "discardReason", MAX_EVIDENCE_CHARS)
      : null;

  const now = new Date().toISOString();
  return {
    id: `learn_${randomUUID().slice(0, 8)}`,
    route,
    title,
    evidence,
    expectedBenefit,
    activationDescription,
    activationTerms: normalizedTerms,
    minimumMatches,
    memoryText,
    skill,
    tool,
    discardReason,
    status: "proposed",
    createdAt: now,
    updatedAt: now,
    sourceConversationId: conversationId,
    activationCount: 0,
    lastActivatedAt: null,
    validation: null,
  };
}

function parseSkillPayload(value: JsonValue | undefined): SkillPayload | null {
  if (value === null) {
    return null;
  }
  const object = jsonObject(value, "skill");
  const filesValue = object.files;
  if (!Array.isArray(filesValue)) {
    throw new Error("skill.files must be an array");
  }
  const skillMd = requiredString(object.skillMd, "skillMd", MAX_SKILL_MD_CHARS);
  const frontmatter = parseSkillFrontmatter(skillMd);
  if (frontmatter === null) {
    throw new Error("skillMd must contain YAML frontmatter");
  }
  const name = frontmatter.fields.name ?? "";
  if (!/^[a-z0-9]+(-[a-z0-9]+)*$/.test(name) || name.length > 64) {
    throw new Error(
      "skillMd frontmatter name must be 1-64 lowercase letters, digits, and single hyphens",
    );
  }
  const description = frontmatter.fields.description ?? "";
  if (description.length === 0 || description.length > 1_024) {
    throw new Error(
      "skillMd frontmatter description must be 1-1024 characters",
    );
  }
  const seen = new Set<string>();
  const files = filesValue.map((item) => {
    const file = jsonObject(item, "skill file");
    const path = requiredString(file.path, "skill file path", 500);
    const pathError = validateSkillFilePath(path);
    if (pathError !== null) {
      throw new Error(pathError);
    }
    if (path === "SKILL.md") {
      throw new Error("skill files must not replace the top-level skillMd");
    }
    if (seen.has(path)) {
      throw new Error(`duplicate skill file path: ${path}`);
    }
    seen.add(path);
    return {
      path,
      contents: requiredString(file.contents, "skill file contents", 200_000),
    };
  });
  return {
    skillMd,
    files,
    validationCommand: requiredString(
      object.validationCommand,
      "validationCommand",
      MAX_VALIDATION_COMMAND_CHARS,
    ),
  };
}

function parseToolPayload(value: JsonValue | undefined): ToolPayload | null {
  if (value === null) {
    return null;
  }
  const object = jsonObject(value, "tool");
  const validationArguments = parseJsonObjectString(
    object.validationArgumentsJson,
    "validationArgumentsJson",
  );
  const expectedResult = parseJsonString(
    object.expectedResultJson,
    "expectedResultJson",
  );
  const moduleName = requiredString(object.moduleName, "moduleName", 100);
  if (!/^[A-Za-z0-9_-]+$/.test(moduleName)) {
    throw new Error(
      "moduleName must contain only letters, numbers, underscores, and dashes",
    );
  }
  const toolName = requiredString(object.toolName, "toolName", 100);
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(toolName)) {
    throw new Error(
      "toolName must start with a letter or underscore and contain only letters, numbers, and underscores",
    );
  }
  return {
    moduleName,
    toolName,
    moduleSource: requiredString(
      object.moduleSource,
      "moduleSource",
      MAX_TOOL_SOURCE_CHARS,
    ),
    apiKeyEnv: emptyStringToNull(object.apiKeyEnv, "apiKeyEnv", 200),
    validationArguments,
    expectedResult,
  };
}

async function executeShell(
  context: TurnContext,
  command: string,
): Promise<{ exitCode: number; stdout: string; stderr: string }> {
  const result = await context.executeTool({
    functionName: "shell",
    arguments: { command },
  });
  const object = jsonObject(result, "shell result");
  const exitCode = object.exit_code;
  if (typeof exitCode !== "number") {
    throw new Error("shell validation returned no numeric exit_code");
  }
  return {
    exitCode,
    stdout: typeof object.stdout === "string" ? object.stdout : "",
    stderr: typeof object.stderr === "string" ? object.stderr : "",
  };
}

function stagedSkillValidationCommand(skill: SkillPayload): string {
  const stagedFiles = [
    { path: "SKILL.md", contents: skill.skillMd },
    ...skill.files.filter((file) => file.path !== "SKILL.md"),
  ];
  const writes = stagedFiles.flatMap((file) => {
    const segments = file.path.split("/");
    const directory = segments.slice(0, -1).join("/");
    return [
      ...(directory.length > 0
        ? [`mkdir -p -- "$validation_root"/${shellQuote(directory)}`]
        : []),
      `printf %s ${shellQuote(Buffer.from(file.contents).toString("base64"))} | base64 --decode > "$validation_root"/${shellQuote(file.path)}`,
    ];
  });
  return [
    "set -eu",
    "validation_root=$(mktemp -d /tmp/exo-learning-skill.XXXXXX)",
    'cleanup() { rm -rf -- "$validation_root"; }',
    "trap cleanup EXIT HUP INT TERM",
    ...writes,
    'cd "$validation_root"',
    skill.validationCommand,
  ].join("\n");
}

function validateSkillFilePath(path: string): string | null {
  if (path.startsWith("/") || path.includes("\\")) {
    return `skill file path must be relative with forward slashes: ${path}`;
  }
  const segments = path.split("/");
  if (segments.some((segment) => segment.length === 0 || segment === "..")) {
    return `skill file path must not contain empty or .. segments: ${path}`;
  }
  return null;
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}

async function activationAlreadyRecorded(
  context: TurnContext,
): Promise<boolean> {
  const events = await context.exoharness.current.conversation.getEvents({
    direction: "asc",
    turnId: context.exoharness.current.turn.record.id,
    types: ["learning_activated"],
  });
  return events.events.length > 0;
}

function activationMatches(
  candidate: LearningCandidate,
  input: string,
): boolean {
  const matches = candidate.activationTerms.filter((term) =>
    activationTermMatches(input, term),
  );
  return matches.length >= candidate.minimumMatches;
}

function activationTermMatches(input: string, term: string): boolean {
  let offset = 0;
  while (offset <= input.length - term.length) {
    const index = input.indexOf(term, offset);
    if (index < 0) {
      return false;
    }
    const before = index === 0 ? "" : input[index - 1];
    const afterIndex = index + term.length;
    const after = afterIndex === input.length ? "" : input[afterIndex];
    const startIsBounded =
      !isAsciiWordCharacter(term[0]) || !isAsciiWordCharacter(before);
    const endIsBounded =
      !isAsciiWordCharacter(term[term.length - 1]) ||
      !isAsciiWordCharacter(after);
    if (startIsBounded && endIsBounded) {
      return true;
    }
    offset = index + 1;
  }
  return false;
}

function isAsciiWordCharacter(value: string): boolean {
  return /^[a-z0-9_]$/i.test(value);
}

function activatedCandidateText(candidate: LearningCandidate): string {
  const header = `[${candidate.id}] ${candidate.route.toUpperCase()} — ${candidate.title}\nUse when: ${candidate.activationDescription}\nExpected benefit: ${candidate.expectedBenefit}`;
  if (candidate.route === "memory") {
    return `${header}\nValidated memory: ${candidate.memoryText}`;
  }
  if (candidate.route === "skill") {
    const skillName = candidate.skill
      ? (parseSkillFrontmatter(candidate.skill.skillMd)?.fields.name ?? null)
      : null;
    return `${header}\nValidated skill: call use_learning_skill with candidateId ${JSON.stringify(candidate.id)} to load ${JSON.stringify(skillName ?? candidate.title)}, then follow it.`;
  }
  return `${header}\nValidated tool: call ${candidate.tool?.toolName ?? "the promoted tool"} rather than rebuilding the operation.`;
}

function learningHandle(context: TurnContext): LearningHandle {
  return context.exoharness.current.agent;
}

async function readIndex(handle: LearningHandle): Promise<LearningIndex> {
  const latest = latestArtifactVersion(
    await handle.listArtifacts(),
    LEARNING_INDEX_PATH,
  );
  if (latest === null) {
    return { candidates: [] };
  }
  const raw = await handle.readArtifactJson<unknown>({
    artifactId: latest.artifactId,
    version: latest.version,
  });
  if (!isLearningIndex(raw)) {
    throw new Error(`corrupt learning artifact ${LEARNING_INDEX_PATH}`);
  }
  return raw;
}

async function writeIndex(
  handle: LearningHandle,
  index: LearningIndex,
): Promise<void> {
  await handle.writeArtifactJson({
    path: LEARNING_INDEX_PATH,
    value: index as unknown as JsonValue,
  });
}

function latestArtifactVersion(
  artifacts: ArtifactVersion[],
  path: string,
): ArtifactVersion | null {
  return (
    artifacts
      .filter((artifact) => artifact.path === path)
      .sort((a, b) => b.version - a.version)[0] ?? null
  );
}

function isLearningIndex(value: unknown): value is LearningIndex {
  if (!isRecord(value) || !Array.isArray(value.candidates)) {
    return false;
  }
  return value.candidates.every(isLearningCandidate);
}

function isLearningCandidate(value: unknown): value is LearningCandidate {
  if (
    !(
      isRecord(value) &&
      typeof value.id === "string" &&
      isLearningRoute(value.route) &&
      typeof value.title === "string" &&
      typeof value.evidence === "string" &&
      typeof value.expectedBenefit === "string" &&
      typeof value.activationDescription === "string" &&
      Array.isArray(value.activationTerms) &&
      value.activationTerms.every((term) => typeof term === "string") &&
      typeof value.minimumMatches === "number" &&
      Number.isInteger(value.minimumMatches) &&
      isLearningStatus(value.status) &&
      typeof value.createdAt === "string" &&
      typeof value.updatedAt === "string" &&
      typeof value.sourceConversationId === "string" &&
      typeof value.activationCount === "number" &&
      Number.isInteger(value.activationCount) &&
      value.activationCount >= 0 &&
      (value.lastActivatedAt === null ||
        typeof value.lastActivatedAt === "string") &&
      (value.validation === null || isRecord(value.validation)) &&
      (value.memoryText === null || typeof value.memoryText === "string") &&
      (value.skill === null || isSkillPayload(value.skill)) &&
      (value.tool === null || isToolPayload(value.tool)) &&
      (value.discardReason === null || typeof value.discardReason === "string")
    )
  ) {
    return false;
  }
  if (value.route === "memory") {
    return (
      value.memoryText !== null &&
      value.skill === null &&
      value.tool === null &&
      value.discardReason === null &&
      validActiveTrigger(value.activationTerms, value.minimumMatches)
    );
  }
  if (value.route === "skill") {
    return (
      value.memoryText === null &&
      value.skill !== null &&
      value.tool === null &&
      value.discardReason === null &&
      validActiveTrigger(value.activationTerms, value.minimumMatches)
    );
  }
  if (value.route === "tool") {
    return (
      value.memoryText === null &&
      value.skill === null &&
      value.tool !== null &&
      value.discardReason === null &&
      validActiveTrigger(value.activationTerms, value.minimumMatches)
    );
  }
  return (
    value.memoryText === null &&
    value.skill === null &&
    value.tool === null &&
    value.discardReason !== null &&
    value.activationTerms.length === 0 &&
    value.minimumMatches === 0
  );
}

function validActiveTrigger(
  activationTerms: string[],
  minimumMatches: number,
): boolean {
  return (
    activationTerms.length > 0 &&
    minimumMatches >= 1 &&
    minimumMatches <= activationTerms.length
  );
}

function isSkillPayload(value: unknown): value is SkillPayload {
  return (
    isRecord(value) &&
    typeof value.skillMd === "string" &&
    validSkillFrontmatter(value.skillMd) &&
    typeof value.validationCommand === "string" &&
    Array.isArray(value.files) &&
    value.files.every(
      (file) =>
        isRecord(file) &&
        typeof file.path === "string" &&
        validateSkillFilePath(file.path) === null &&
        file.path !== "SKILL.md" &&
        typeof file.contents === "string",
    )
  );
}

function validSkillFrontmatter(skillMd: string): boolean {
  const frontmatter = parseSkillFrontmatter(skillMd);
  if (frontmatter === null) {
    return false;
  }
  const name = frontmatter.fields.name ?? "";
  const description = frontmatter.fields.description ?? "";
  return (
    /^[a-z0-9]+(-[a-z0-9]+)*$/.test(name) &&
    name.length <= 64 &&
    description.length >= 1 &&
    description.length <= 1_024
  );
}

function isToolPayload(value: unknown): value is ToolPayload {
  return (
    isRecord(value) &&
    typeof value.moduleName === "string" &&
    /^[A-Za-z0-9_-]+$/.test(value.moduleName) &&
    typeof value.toolName === "string" &&
    /^[A-Za-z_][A-Za-z0-9_]*$/.test(value.toolName) &&
    typeof value.moduleSource === "string" &&
    (value.apiKeyEnv === null || typeof value.apiKeyEnv === "string") &&
    isRecord(value.validationArguments) &&
    "expectedResult" in value
  );
}

function isLearningRoute(value: unknown): value is LearningRoute {
  return (
    value === "memory" ||
    value === "skill" ||
    value === "tool" ||
    value === "discard"
  );
}

function isLearningStatus(value: unknown): value is LearningStatus {
  return (
    value === "proposed" ||
    value === "promoted" ||
    value === "rejected" ||
    value === "discarded"
  );
}

function requiredString(
  value: JsonValue | undefined,
  name: string,
  maximum: number,
): string {
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new Error(`${name} must be a non-empty string`);
  }
  const result = value.trim();
  if (result.length > maximum) {
    throw new Error(`${name} exceeds ${maximum} characters`);
  }
  return result;
}

function emptyStringToNull(
  value: JsonValue | undefined,
  name: string,
  maximum: number,
): string | null {
  if (typeof value !== "string") {
    throw new Error(`${name} must be a string`);
  }
  return value.trim().length === 0
    ? null
    : requiredString(value, name, maximum);
}

function stringArray(value: JsonValue | undefined, name: string): string[] {
  if (
    !Array.isArray(value) ||
    !value.every((item) => typeof item === "string")
  ) {
    throw new Error(`${name} must be an array of strings`);
  }
  return value;
}

function integer(value: JsonValue | undefined, name: string): number {
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new Error(`${name} must be an integer`);
  }
  return value;
}

function jsonObject(value: unknown, name: string): JsonObject {
  if (!isRecord(value) || Array.isArray(value)) {
    throw new Error(`${name} must be an object`);
  }
  return value as JsonObject;
}

function parseJsonObjectString(
  value: JsonValue | undefined,
  name: string,
): JsonObject {
  return jsonObject(parseJsonString(value, name), name);
}

function parseJsonString(
  value: JsonValue | undefined,
  name: string,
): JsonValue {
  const text = requiredString(value, name, 20_000);
  try {
    return JSON.parse(text) as JsonValue;
  } catch {
    throw new Error(`${name} must contain valid JSON`);
  }
}

function requestText(context: TurnContext): string {
  return (context.request?.input ?? [])
    .map((message) =>
      typeof message.content === "string"
        ? message.content
        : JSON.stringify(message.content),
    )
    .join("\n");
}

function candidateFingerprint(candidate: LearningCandidate): string {
  return canonicalJson({
    route: candidate.route,
    title: candidate.title.toLowerCase(),
    activationTerms: candidate.activationTerms,
    memoryText: candidate.memoryText,
    skill: candidate.skill,
    tool: candidate.tool,
    discardReason: candidate.discardReason,
  });
}

function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(canonicalJson).join(",")}]`;
  }
  if (isRecord(value)) {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value) ?? "undefined";
}

function truncate(value: string, limit: number): string {
  return value.length <= limit ? value : `${value.slice(0, limit)}…`;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
