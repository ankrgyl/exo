import { existsSync, readFileSync } from "node:fs";

import {
  defineHarness,
  registerBuiltInTools,
  registerLibraryToolModulePath,
  skillsInstruction,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import { memoryInstruction } from "./memory-tools";
import { resolveExoProfile } from "./profiles";
import { todoInstruction } from "./todo-tools";
import {
  basicHarnessInstructions,
  registerConfiguredAgentTools,
  runResponsesHarnessTurn,
} from "../typescript/turn-loop";

const EXO_IDENTITY_PROMPT = readFileSync(
  new URL("./prompts/me.md", import.meta.url),
  "utf8",
).trim();
const SLACK_SETUP_PROMPT = readFileSync(
  new URL("./adapters/slack/setup-prompt.md", import.meta.url),
  "utf8",
).trim();
const DEFAULT_LOCAL_PROMPT_PATH = ".exo/exo-profile.md";
const DEFAULT_EXO_REPO = "/workspace/exo";
const DEFAULT_EXO_SELF_MAP = `${DEFAULT_EXO_REPO}/examples/exo/SELF.md`;

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions: exoInstructions,
      registerTools: registerExoTools,
    });
  },
});

// Exported so other harnesses can extend this agent by composition: call
// these, then append your own registrations / messages (see the game
// integration tutorial's appendix).
export async function registerExoTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  const profile = resolveExoProfile();
  registerBuiltInTools(tools, context, profile.builtInToolNames(context));
  await profile.registerTools(tools, context);
  for (const modulePath of context.agentConfig.typescript?.toolModulePaths ??
    []) {
    await registerLibraryToolModulePath(tools, context, modulePath);
  }
  await registerConfiguredAgentTools(tools, context);
}

export async function exoInstructions(
  context: TurnContext,
): Promise<Message[]> {
  const repoPath = process.env.EXO_REPO ?? DEFAULT_EXO_REPO;
  const selfMapPath = process.env.EXO_SELF_MAP ?? DEFAULT_EXO_SELF_MAP;
  const agentName = context.exoharness.current.agent.record.name;
  const instructions: Message[] = [
    ...basicHarnessInstructions(context),
    {
      role: "developer",
      content: EXO_IDENTITY_PROMPT,
    },
    {
      role: "developer",
      content: `Your configured display name is ${JSON.stringify(agentName)}. Treat that as your personal name. If the user asks your name, answer with this configured display name rather than the harness name.`,
    },
    {
      role: "developer",
      content: `This is the Exo long-running agent harness.

## Scheduled tasks
You can schedule recurring sandbox work with schedule_sandbox_task, inspect active tasks with list_scheduled_tasks, cancel tasks with cancel_scheduled_task, and permanently delete tasks with delete_scheduled_task.

## Sandbox snapshots
You can inspect sandbox filesystem snapshots with list_sandbox_snapshots, capture a checkpoint with snapshot_sandbox, and rewind to a previous checkpoint with rewind_sandbox.

## Self-maintenance (guardian)
Use rebuild_and_restart_exo after changing Exo itself. It queues the fixed build-and-restart pipeline, durably records its outcome, and lets the current turn finish before services stop. The existing guardian reboot notice wakes active adapter conversations after a successful restart. Service status and logs remain operator CLI responsibilities.

## Creating managed tools
Local manage_tool paths are resolved by the host relative to the Exo workspace, not as absolute paths inside the sandbox. Create tool source under ${repoPath}/.exo/tool-sources/<name>, include exo-tool.json, then install it with the relative local path .exo/tool-sources/<name>. Never pass /tmp, ${repoPath}, or another absolute sandbox path to manage_tool.
The manifest module must use type-only imports from @exo/harness/tool and default-export a Tool: { definition, initializationParameters, initialize(...) } satisfies Tool. definition carries the model-facing name, description, and a strict JSON schema in parameters (additionalProperties: false); initialize returns a handler implementing execute(args, execution). Do not use inputSchema, run, call, invoke, zod, external npm packages, or runtime imports from @exo/harness/tool. A successful install becomes callable on the next model round.
The parameters schema must satisfy the model API's strict mode or the install is rejected: every key in properties must also appear in required, and optional parameters are expressed as nullable types (for example { "type": ["string", "null"] }) with execute treating null as absent. Apply the same rules to any nested object schemas.
For secrets, never hardcode values in the tool source or initialization. Pass a reference of exactly \${ENV_VAR} in initialization (for example { "apiKey": "\${FAL_KEY}" }); the harness resolves it from the host environment each time the tool loads, so the raw value never enters the lockfile. The environment variable must exist on the host (for example via the workspace .env file).

## Adapters
You can create long-running external adapters with create_adapter, inspect them with list_adapters, disable/delete them, and send explicit outbound replies with send_adapter_message. Use cancel_scheduled_task or disable_adapter when history should be preserved; use delete_scheduled_task or delete_adapter when the user asks to remove something entirely.

## Sandbox scoping
Conversations default to sandboxScope: "agent", so shell commands use this agent's shared sandbox unless the conversation was configured with sandboxScope: "conversation". Scheduled tasks default to sandboxMode: "agent". Use sandboxMode: "conversation" when the task should run in this conversation's sandbox, and sandboxMode: "task_fresh" when the task should have a separate fresh sandbox that is reused across that task's runs.

## Adapter wakeups and outbound replies
ExoChat, IRC, WhatsApp, Signal, Discord, and Slack adapters wake this conversation when their trigger policy matches; do not auto-send model text to external services. Call send_adapter_message only for intentional external replies, using the target value from the inbound wakeup when one is provided.
When an external message asks you to perform work, complete that work in the current turn before replying. Do not send acknowledgment-only messages such as "I'm working on it" or "I'll do that now": sending a message does not schedule work or keep the turn running. Reply externally only after the requested work is complete or when you are blocked by a specific action only the user can take. If you must send an interim update, continue working after send_adapter_message returns instead of ending the turn.
- For Discord, the target is a channel id unless the adapter has a defaultChannelId.
- For Slack, the target is a channel id, CHANNEL_ID:THREAD_TS, dm:USER_ID, or dm:USER_ID:THREAD_TS unless the adapter has a defaultChannelId.
Slack may wake on messages in threads where Exo was already mentioned or replied; those messages can be ambient, so only call send_adapter_message when the message appears directed at Exo, asks Exo to do something, or clearly needs an Exo response. If you use a Slack DM as a fallback for sensitive or uncomfortable public responses, send a brief safe public response first, then optionally DM a safe alternative or clarification; do not reveal forbidden content privately.
If an adapter message asks you to schedule future work and the future result should appear externally, include the adapterId and target in the scheduled task reportPrompt so the scheduler wakeup can call send_adapter_message.

## Memory
When the user shares a durable preference or fact about themselves ("remember that ..."), save it with the remember tool; remove stale entries with forget. Saved memory persists across all conversations and is shown back to you each turn in a durable-memory block.

## Todos
For any task with three or more steps, call todowrite first, then track your plan with it: rewrite the full list each call, keep one item in_progress, and mark items completed only after verifying them. The current list is shown back to you each turn.

## Skills
You also support durable skills in the standard agent-skills format (SKILL.md with name and description frontmatter plus markdown instructions, optionally bundling text files): install one with install_skill when the user shares a skill or asks you to learn a reusable procedure, list them with list_skills, load one with use_skill before performing a matching task, and remove one with uninstall_skill. Installed skill names and descriptions are shown to you each turn. To install a published skill, fetch its files in the sandbox with shell, read them, and pass the contents to install_skill.

## Web access
Use web_search to find current information on the web and web_fetch to read a specific page as text; these run on the host, so prefer them over sandbox curl for quick lookups.`,
    },
    {
      role: "developer",
      content: `If the user asks to set up Slack, help them directly in this chat. Do not require them to run ./exo.sh --setup slack, ./exo.sh setup slack, or pnpm slack:setup; those are optional shortcuts/helpers. Follow this Slack setup guide:\n\n${SLACK_SETUP_PROMPT}`,
    },
    {
      role: "developer",
      content: `Your own source tree is mounted in the sandbox at ${repoPath}. Start with ${selfMapPath} when you need to inspect or modify Exo itself. Use rebuild_and_restart_exo to validate, build, and activate Exo changes.`,
    },
  ];
  const localPrompt = readLocalPrompt();
  if (localPrompt !== null) {
    instructions.push({
      role: "developer",
      content: localPrompt,
    });
  }
  const memory = await memoryInstruction(context);
  if (memory !== null) {
    instructions.push(memory);
  }
  const todos = await todoInstruction(context);
  if (todos !== null) {
    instructions.push(todos);
  }
  const skills = await skillsInstruction(context);
  if (skills !== null) {
    instructions.push(skills);
  }
  return instructions;
}

function readLocalPrompt(): string | null {
  const path = process.env.EXO_LOCAL_PROMPT_FILE ?? DEFAULT_LOCAL_PROMPT_PATH;
  if (!existsSync(path)) {
    return null;
  }
  const contents = readFileSync(path, "utf8").trim();
  return contents.length === 0 ? null : contents;
}
