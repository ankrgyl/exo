#!/usr/bin/env tsx

import { spawnSync, type SpawnSyncReturns } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

type HarnessKey = "codex" | "claude" | "cursor" | "pi";

interface HarnessDefinition {
  key: HarnessKey;
  envName: string;
  secret: string;
  model: string;
  module: string;
  image: string;
  imageBuildArgs: string[];
}

interface CliArgs {
  only: Set<HarnessKey>;
  root: string | null;
  keepRoot: boolean;
  buildImages: boolean;
  sandbox: boolean;
  braintrust: boolean;
  timeoutMs: number;
}

interface BraintrustE2eConfig {
  org: string;
  project: string;
}

interface AgentRef {
  slug: string;
  id: string;
}

interface HistoryReplayResult {
  conversation: string;
  fork: string;
  codeWord: string;
}

interface SandboxEscapeResult {
  conversation: string;
}

interface HarnessCheckResult {
  agent: AgentRef;
  history: HistoryReplayResult;
  sandbox: SandboxEscapeResult | null;
  braintrust: string | null;
}

type E2eResult =
  | ({ harness: HarnessKey; status: "passed" } & HarnessCheckResult)
  | { harness: HarnessKey; status: "skipped"; reason: string }
  | { harness: HarnessKey; status: "failed"; reason: string };

interface CommandOptions {
  timeoutMs?: number;
}

interface ConversationEventsResult {
  events: EventRecord[];
  cursor?: string | null;
}

interface EventRecord {
  id?: string;
}

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
loadDotEnv(join(repoRoot, ".env"), { override: true });

const args = parseArgs(process.argv.slice(2));
const runId = randomUUID().slice(0, 8);
const root = args.root ?? mkdtempSync(join(tmpdir(), "exo-agent-e2e-"));
const exoBin = resolveExoBin();

const harnesses: HarnessDefinition[] = [
  {
    key: "codex",
    envName: "OPENAI_API_KEY",
    secret: "openai",
    model: "gpt-5.4",
    module: "exoharness/examples/typescript/codex-harness.ts",
    image: "exo-codex-sandbox:latest",
    imageBuildArgs: ["exoharness/containers/codex-sandbox"],
  },
  {
    key: "claude",
    envName: "ANTHROPIC_API_KEY",
    secret: "anthropic",
    model: "claude-sonnet-4-6",
    module: "exoharness/examples/typescript/claude-code-harness.ts",
    image: "exo-claude-code-sandbox:latest",
    imageBuildArgs: ["exoharness/containers/claude-code-sandbox"],
  },
  {
    key: "pi",
    envName: "OPENAI_API_KEY",
    secret: "openai",
    model: "gpt-5.5",
    module: "exoharness/examples/typescript/pi-harness.ts",
    image: "exo-pi-sandbox:latest",
    imageBuildArgs: ["exoharness/containers/pi-sandbox"],
  },
  {
    key: "cursor",
    envName: "CURSOR_API_KEY",
    secret: "cursor",
    model: "auto",
    module: "exoharness/examples/typescript/cursor-sdk-harness.ts",
    image: "exo-cursor-sdk-sandbox:latest",
    imageBuildArgs: [
      "-f",
      "exoharness/containers/cursor-sdk-sandbox/Containerfile",
      ".",
    ],
  },
];

await main();

async function main(): Promise<void> {
  const selectedHarnesses = harnesses.filter((harness) => {
    return args.only.size === 0 || args.only.has(harness.key);
  });

  if (selectedHarnesses.length === 0) {
    fail(
      `no harnesses selected; valid values are ${harnesses.map((h) => h.key).join(", ")}`,
    );
  }

  log(`using exo root ${root}`);
  if (args.buildImages) {
    for (const harness of selectedHarnesses) {
      buildImage(harness);
    }
  }

  const results: E2eResult[] = [];
  for (const harness of selectedHarnesses) {
    if (!process.env[harness.envName]) {
      results.push({
        harness: harness.key,
        status: "skipped",
        reason: `${harness.envName} is not set`,
      });
      continue;
    }

    try {
      const result = await runHarnessChecks(harness);
      results.push({ harness: harness.key, status: "passed", ...result });
    } catch (error) {
      results.push({
        harness: harness.key,
        status: "failed",
        reason: errorMessage(error),
      });
    }
  }

  if (!args.keepRoot) {
    rmSync(root, { recursive: true, force: true });
  }

  const failed = results.filter((result) => result.status === "failed");
  const skipped = results.filter((result) => result.status === "skipped");
  for (const result of results) {
    if (result.status === "passed") {
      log(
        `${result.harness}: passed history/replay${args.sandbox ? " and sandbox" : ""} checks` +
          (result.braintrust ? `; Braintrust trace ${result.braintrust}` : ""),
      );
    } else if (result.status === "skipped") {
      log(`${result.harness}: skipped (${result.reason})`);
    } else {
      log(`${result.harness}: failed (${result.reason})`);
    }
  }

  if (failed.length > 0) {
    process.exitCode = 1;
  } else if (skipped.length === selectedHarnesses.length) {
    fail("all selected harnesses were skipped");
  }
}

async function runHarnessChecks(
  harness: HarnessDefinition,
): Promise<HarnessCheckResult> {
  registerSecretAndModel(harness);
  const agent = createAgent(harness);
  const history = runHistoryReplayCheck(harness, agent);
  const sandbox = args.sandbox ? runSandboxEscapeCheck(harness, agent) : null;
  const braintrust = args.braintrust
    ? verifyBraintrust(harness, agent.id)
    : null;
  return { agent, history, sandbox, braintrust };
}

function registerSecretAndModel(harness: HarnessDefinition): void {
  runExo(["secret", "set", harness.secret, "--env", harness.envName]);
  runExo(["model", "register", harness.model, "--secret", harness.secret]);
}

function createAgent(harness: HarnessDefinition): AgentRef {
  const slug = `e2e-${harness.key}-${runId}`;
  const commandArgs = [
    "--harness",
    "typescript",
    "agent",
    "create",
    slug,
    "--module",
    harness.module,
    "--model",
    harness.model,
    "--sandbox-image",
    harness.image,
  ];
  if (args.braintrust) {
    const config = braintrustConfig();
    commandArgs.push(
      "--braintrust-org",
      config.org,
      "--braintrust-project",
      config.project,
    );
  }
  const output = runExo(commandArgs);
  return { slug, id: parseCreatedId(output, "agent") };
}

function runHistoryReplayCheck(
  harness: HarnessDefinition,
  agent: AgentRef,
): HistoryReplayResult {
  const workspace = mkdtempSync(
    join(tmpdir(), `exo-${harness.key}-workspace-`),
  );
  const toolMarker = `${harness.key}-tool-marker-${runId}`;
  writeFileSync(join(workspace, "README.md"), `${harness.key} e2e workspace\n`);
  writeFileSync(join(workspace, "tool-marker.txt"), `${toolMarker}\n`);

  const conversation = `history-${harness.key}-${runId}`;
  runExo(["conversation", "create", agent.slug, "--slug", conversation]);
  runExo([
    "conversation",
    "mount",
    "add",
    agent.slug,
    conversation,
    workspace,
    "/workspace",
    "--rw",
  ]);
  runExo(["agent", "update", agent.slug, "--networking", "enabled"]);

  const codeWord = `${harness.key}-blue-lantern-${runId}`;
  runChat(
    agent.slug,
    conversation,
    `Remember this exact code word for the next turn: ${codeWord}. Reply with only OK.`,
  );
  const second = runChat(
    agent.slug,
    conversation,
    "What exact code word did I ask you to remember? Reply with only the code word.",
  );
  assertIncludes(
    second,
    codeWord,
    `${harness.key} did not answer from prior exoharness history`,
  );

  const toolAnswer = runChat(
    agent.slug,
    conversation,
    [
      "Use a shell command in the sandbox to list /workspace and read /workspace/tool-marker.txt.",
      "Do not answer from memory.",
      "Reply with the file listing and the exact marker contents.",
    ].join("\n"),
    { timeoutMs: args.timeoutMs * 2 },
  );
  assertIncludes(
    toolAnswer,
    "tool-marker.txt",
    `${harness.key} did not list the mounted workspace through a tool call`,
  );
  assertIncludes(
    toolAnswer,
    toolMarker,
    `${harness.key} did not read the mounted marker file through a tool call`,
  );

  const firstTurnEndedId = firstTurnEndedEventId(agent.slug, conversation);
  const fork = `fork-${harness.key}-${runId}`;
  runExo([
    "conversation",
    "fork",
    agent.slug,
    conversation,
    "--slug",
    fork,
    "--up-to",
    firstTurnEndedId,
  ]);
  const forkAnswer = runChat(
    agent.slug,
    fork,
    "What exact code word did I ask you to remember? Reply with only the code word.",
  );
  assertIncludes(
    forkAnswer,
    codeWord,
    `${harness.key} fork did not replay prior exoharness history`,
  );

  return { conversation, fork, codeWord };
}

function runSandboxEscapeCheck(
  harness: HarnessDefinition,
  agent: AgentRef,
): SandboxEscapeResult {
  const fileCheck = runFilesystemSandboxCheck(harness, agent);
  runNetworkDisabledCheck(harness, agent);
  return fileCheck;
}

function runFilesystemSandboxCheck(
  harness: HarnessDefinition,
  agent: AgentRef,
): SandboxEscapeResult {
  const workspace = mkdtempSync(
    join(tmpdir(), `exo-${harness.key}-sandbox-workspace-`),
  );
  const outside = mkdtempSync(join(tmpdir(), `exo-${harness.key}-outside-`));
  const allowedMarker = `${harness.key}-allowed-${runId}`;
  const outsideMarker = `${harness.key}-outside-secret-${runId}`;
  writeFileSync(join(workspace, "allowed.txt"), `${allowedMarker}\n`);
  writeFileSync(join(outside, "secret.txt"), `${outsideMarker}\n`);

  const conversation = `sandbox-${harness.key}-${runId}`;
  runExo(["conversation", "create", agent.slug, "--slug", conversation]);
  runExo([
    "conversation",
    "mount",
    "add",
    agent.slug,
    conversation,
    workspace,
    "/workspace",
    "--rw",
  ]);
  runExo(["agent", "update", agent.slug, "--networking", "enabled"]);

  const outsideSecret = join(outside, "secret.txt");
  const outsideWrite = join(outside, "escape.txt");
  const prompt = [
    "Run shell commands in the sandbox to check filesystem and network behavior.",
    "Do not infer or answer from memory; actually execute the checks.",
    "Report stdout/stderr/exit status for each check.",
    "1. cat /workspace/allowed.txt",
    `2. cat ${outsideSecret}`,
    `3. sh -lc 'printf escaped > ${outsideWrite}'`,
  ].join("\n");
  const answer = runChat(agent.slug, conversation, prompt, {
    timeoutMs: args.timeoutMs * 2,
  });

  assertIncludes(
    answer,
    allowedMarker,
    `${harness.key} did not read the mounted workspace file`,
  );
  if (answer.includes(outsideMarker)) {
    throw new Error(
      `${harness.key} leaked an unmounted host file into the answer`,
    );
  }
  if (existsSync(outsideWrite)) {
    throw new Error(`${harness.key} wrote outside the mounted workspace`);
  }

  const events = sandboxVisibilityEvents(harness, agent.slug, conversation);
  if (events.events.length === 0) {
    throw new Error(
      `${harness.key} filesystem sandbox check produced no visible tool/custom events`,
    );
  }
  const eventText = JSON.stringify(events);
  if (harness.key === "cursor" && !eventText.includes(outsideSecret)) {
    throw new Error(
      `${harness.key} filesystem sandbox check did not record the denied path attempt`,
    );
  }

  return { conversation };
}

function sandboxVisibilityEvents(
  harness: HarnessDefinition,
  agent: string,
  conversation: string,
): ConversationEventsResult {
  if (harness.key === "cursor") {
    return conversationEvents(agent, conversation, [
      "--type",
      "cursor_sdk_message",
      "--limit",
      "500",
    ]);
  }
  return conversationEvents(agent, conversation, [
    "--type",
    "tool_result",
    "--limit",
    "50",
  ]);
}

function runNetworkDisabledCheck(
  harness: HarnessDefinition,
  agent: AgentRef,
): void {
  const workspace = mkdtempSync(
    join(tmpdir(), `exo-${harness.key}-network-workspace-`),
  );
  const conversation = `network-${harness.key}-${runId}`;
  runExo(["conversation", "create", agent.slug, "--slug", conversation]);
  runExo([
    "conversation",
    "mount",
    "add",
    agent.slug,
    conversation,
    workspace,
    "/workspace",
    "--rw",
  ]);
  runExo(["agent", "update", agent.slug, "--networking", "disabled"]);

  let failedText: string | null = null;
  try {
    runChat(agent.slug, conversation, "Say OK.", {
      timeoutMs: Math.min(args.timeoutMs, 120_000),
    });
  } catch (error) {
    failedText = errorMessage(error);
  }
  if (!failedText) {
    throw new Error(
      `${harness.key} completed a model turn even though sandbox networking was disabled`,
    );
  }
  if (
    !/network|dns|resolve|connection|ENOTFOUND|EAI_AGAIN|fetch|api|timeout|timed out|disconnected/i.test(
      failedText,
    )
  ) {
    throw new Error(
      `${harness.key} failed with an unexpected network-disabled error:\n${failedText}`,
    );
  }

  const events = conversationEvents(agent.slug, conversation, [
    "--limit",
    "50",
  ]);
  if (events.events.length === 0) {
    throw new Error(
      `${harness.key} network-disabled failure produced no exoharness events`,
    );
  }
}

function firstTurnEndedEventId(agent: string, conversation: string): string {
  const result = conversationEvents(agent, conversation, [
    "--type",
    "turn_ended",
    "--limit",
    "10",
  ]);
  const event = result.events[0];
  if (!event?.id) {
    throw new Error(`no turn_ended event found for ${agent}/${conversation}`);
  }
  return event.id;
}

function conversationEvents(
  agent: string,
  conversation: string,
  extraArgs: string[],
): ConversationEventsResult {
  return parseJson<ConversationEventsResult>(
    runExo(["conversation", "events", agent, conversation, ...extraArgs]),
  );
}

function runChat(
  agent: string,
  conversation: string,
  prompt: string,
  options: CommandOptions = {},
): string {
  return runExo(["conversation", "send", agent, conversation, prompt], {
    timeoutMs: options.timeoutMs ?? args.timeoutMs,
  });
}

function verifyBraintrust(
  harness: HarnessDefinition,
  agentId: string,
): string | null {
  const config = braintrustConfig();
  const output = run(
    "bt",
    [
      "view",
      "logs",
      "--json",
      "--no-input",
      "--env-file",
      ".env",
      "--org",
      config.org,
      "--project",
      config.project,
      "--window",
      "1h",
      "--limit",
      "200",
      "--list-mode",
      "spans",
      "--filter",
      `metadata.agent_id = '${agentId}'`,
    ],
    { timeoutMs: 60_000 },
  );
  const rows = parseJson<unknown>(output);
  const serialized = JSON.stringify(rows);
  if (!serialized.includes(agentId)) {
    throw new Error(`Braintrust logs did not contain agent_id ${agentId}`);
  }
  if (!/executor_turn|llm|tool/i.test(serialized)) {
    throw new Error(
      `Braintrust logs for ${agentId} did not contain expected turn/LLM/tool spans`,
    );
  }
  if (!serialized.includes(`${harness.key}_observed_tool`)) {
    throw new Error(
      `Braintrust logs for ${agentId} did not contain ${harness.key}_observed_tool span`,
    );
  }
  return (
    firstStringMatch(serialized, /"root_span_id":"([^"]+)"/) ??
    firstStringMatch(serialized, /"span_id":"([^"]+)"/)
  );
}

function buildImage(harness: HarnessDefinition): void {
  log(`building ${harness.image}`);
  run("container", ["build", "-t", harness.image, ...harness.imageBuildArgs], {
    timeoutMs: 20 * 60_000,
  });
}

function resolveExoBin(): string {
  if (process.env.EXO_BIN) {
    return process.env.EXO_BIN;
  }
  const candidate = join(repoRoot, "target/debug/exo");
  if (!existsSync(candidate)) {
    run("cargo", ["build", "-p", "exo"], { timeoutMs: 10 * 60_000 });
  }
  return candidate;
}

function runExo(commandArgs: string[], options: CommandOptions = {}): string {
  return run(
    exoBin,
    ["--root", root, "--env-file-if-exists", ".env", ...commandArgs],
    options,
  );
}

function run(
  command: string,
  commandArgs: string[],
  options: CommandOptions = {},
): string {
  const timeoutMs = options.timeoutMs ?? args.timeoutMs;
  const result: SpawnSyncReturns<string> = spawnSync(command, commandArgs, {
    cwd: repoRoot,
    env: process.env,
    encoding: "utf8",
    timeout: timeoutMs,
    stdio: ["ignore", "pipe", "pipe"],
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    const rendered = [result.stdout, result.stderr]
      .filter(Boolean)
      .join("\n")
      .trim();
    throw new Error(
      `${command} ${commandArgs.join(" ")} failed with exit ${result.status}${rendered ? `\n${rendered}` : ""}`,
    );
  }
  return result.stdout.trim();
}

function parseArgs(rawArgs: string[]): CliArgs {
  const parsed: CliArgs = {
    only: new Set(),
    root: null,
    keepRoot: false,
    buildImages: false,
    sandbox: false,
    braintrust: false,
    timeoutMs: 300_000,
  };
  for (let index = 0; index < rawArgs.length; index += 1) {
    const arg = rawArgs[index];
    if (arg === "--") {
      continue;
    } else if (arg === "--only") {
      const value = readArgValue(rawArgs, ++index, arg);
      if (!isHarnessKey(value)) {
        fail(`invalid harness for --only: ${value}`);
      }
      parsed.only.add(value);
    } else if (arg === "--root") {
      parsed.root = readArgValue(rawArgs, ++index, arg);
    } else if (arg === "--keep-root") {
      parsed.keepRoot = true;
    } else if (arg === "--build-images") {
      parsed.buildImages = true;
    } else if (arg === "--sandbox") {
      parsed.sandbox = true;
    } else if (arg === "--braintrust") {
      parsed.braintrust = true;
    } else if (arg === "--no-braintrust") {
      parsed.braintrust = false;
    } else if (arg === "--timeout-ms") {
      parsed.timeoutMs = Number(readArgValue(rawArgs, ++index, arg));
      if (!Number.isFinite(parsed.timeoutMs) || parsed.timeoutMs <= 0) {
        fail("--timeout-ms must be a positive number");
      }
    } else if (arg === "--help" || arg === "-h") {
      printHelpAndExit();
    } else {
      fail(`unknown argument: ${arg}`);
    }
  }
  return parsed;
}

function isHarnessKey(value: string): value is HarnessKey {
  return (
    value === "codex" ||
    value === "claude" ||
    value === "cursor" ||
    value === "pi"
  );
}

function readArgValue(rawArgs: string[], index: number, flag: string): string {
  const value = rawArgs[index];
  if (!value) {
    fail(`${flag} requires a value`);
  }
  return value;
}

function parseCreatedId(output: string, noun: string): string {
  const match = output.match(
    new RegExp(`created ${noun} [^\\s]+ \\(([^)]+)\\)`),
  );
  if (!match) {
    throw new Error(`could not parse ${noun} id from: ${output}`);
  }
  return match[1] ?? "";
}

function braintrustConfig(): BraintrustE2eConfig {
  const apiKey = process.env.BRAINTRUST_API_KEY;
  const org = process.env.BRAINTRUST_ORG;
  const project = process.env.BRAINTRUST_PROJECT;
  if (!apiKey || !org || !project) {
    fail(
      "--braintrust requires BRAINTRUST_API_KEY, BRAINTRUST_ORG, and BRAINTRUST_PROJECT",
    );
  }
  return { org, project };
}

function assertIncludes(text: string, expected: string, message: string): void {
  if (!text.includes(expected)) {
    throw new Error(`${message}\nexpected: ${expected}\nactual:\n${text}`);
  }
}

function firstStringMatch(text: string, regex: RegExp): string | null {
  return text.match(regex)?.[1] ?? null;
}

function parseJson<T>(text: string): T {
  return JSON.parse(text) as T;
}

function loadDotEnv(path: string, options: { override: boolean }): void {
  if (!existsSync(path)) {
    return;
  }
  for (const line of readFileSync(path, "utf8").split(/\r?\n/u)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) {
      continue;
    }
    const match = trimmed.match(
      /^(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$/u,
    );
    if (!match) {
      continue;
    }
    const [, key, rawValue] = match;
    if (
      !key ||
      rawValue === undefined ||
      (!options.override && process.env[key] !== undefined)
    ) {
      continue;
    }
    process.env[key] = unquoteEnvValue(rawValue);
  }
}

function unquoteEnvValue(value: string): string {
  const trimmed = value.trim();
  if (
    (trimmed.startsWith('"') && trimmed.endsWith('"')) ||
    (trimmed.startsWith("'") && trimmed.endsWith("'"))
  ) {
    return trimmed.slice(1, -1);
  }
  return trimmed;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function log(message: string): void {
  console.log(`[agent-harness-e2e] ${message}`);
}

function fail(message: string): never {
  console.error(`[agent-harness-e2e] ${message}`);
  process.exit(1);
}

function printHelpAndExit(): never {
  console.log(`Usage: pnpm e2e:agent-harnesses [options]

Runs live Codex/Claude/Cursor history replay checks against exoharness.

Options:
  --only <codex|claude|cursor>  Run one harness. Repeatable.
  --sandbox                     Also run sandbox escape/network-denial checks.
  --build-images                Build the required Apple container images first.
  --braintrust                  Verify traces using BRAINTRUST_* env vars.
  --no-braintrust               Disable Braintrust trace verification.
  --root <path>                 Use an explicit exo root.
  --keep-root                   Do not delete the temporary exo root.
  --timeout-ms <ms>             Per-command timeout. Default: 300000.
`);
  process.exit(0);
}
