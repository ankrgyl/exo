// Tutorial harness: a minimal coding agent.
//
// It reuses exo's built-in `shell` tool as the coding workhorse, adds a
// `todowrite` tool for tracking multi-step work, and uses a concise system
// prompt. exo's turn loop already re-invokes the model while it keeps making
// tool calls, so this file only supplies the prompt, the tools, and the
// task-tracking state. See the "Custom Coding Agent" tutorial.

import os from "node:os";

import {
  defineHarness,
  defineTool,
  registerBuiltInTools,
  registerLibraryTools,
  type Conversation,
  type HarnessToolRegistry,
  type JsonValue,
  type Message,
  type TurnContext,
} from "@exo/harness";

import { runResponsesHarnessTurn } from "@exo/model-runtime/turn-loop";

// --- Task tracking -------------------------------------------------------

type TodoStatus = "pending" | "in_progress" | "completed" | "cancelled";

interface Todo {
  content: string;
  status: TodoStatus;
}

const TODO_ARTIFACT_PATH = "coding-agent/todos.json";

async function readTodos(conversation: Conversation): Promise<Todo[]> {
  const latest = (await conversation.listArtifacts())
    .filter((version) => version.path === TODO_ARTIFACT_PATH)
    .sort((a, b) => b.version - a.version)[0];
  if (!latest) {
    return [];
  }
  const todos = await conversation.readArtifactJson<Todo[]>({
    artifactId: latest.artifactId,
  });
  return todos ?? [];
}

async function writeTodos(
  conversation: Conversation,
  todos: Todo[],
): Promise<void> {
  await conversation.writeArtifactJson({
    path: TODO_ARTIFACT_PATH,
    value: todos as unknown as JsonValue,
  });
}

const todoTool = defineTool({
  definition: {
    name: "todowrite",
    description:
      "Record and update your task list. Rewrite the FULL list on every call. " +
      "Use it for any task needing 3 or more steps; skip it for trivial one-step " +
      "work. Keep exactly one item in_progress, and mark an item completed only " +
      "after you have verified it is actually done.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        todos: {
          type: "array",
          description: "The full, updated task list.",
          items: {
            type: "object",
            additionalProperties: false,
            properties: {
              content: { type: "string" },
              status: {
                type: "string",
                enum: ["pending", "in_progress", "completed", "cancelled"],
              },
            },
            required: ["content", "status"],
          },
        },
      },
      required: ["todos"],
    },
  },
  initializationParameters: {
    type: "object",
    additionalProperties: false,
    properties: {},
  },
  initialize() {
    return {
      async execute(args, execution) {
        const todos = (args.todos ?? []) as unknown as Todo[];
        await writeTodos(
          execution.context.exoharness.current.conversation,
          todos,
        );
        const remaining = todos.filter(
          (todo) => todo.status !== "completed" && todo.status !== "cancelled",
        ).length;
        return { ok: true, remaining };
      },
    };
  },
});

// --- Tools ---------------------------------------------------------------

async function registerCodingTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  // shell is the coding workhorse: it reads (cat), searches (grep), edits
  // (sed), builds, and runs — all inside the conversation's sandbox.
  registerBuiltInTools(tools, context, ["shell"]);
  await registerLibraryTools(tools, context, todoTool);
}

// --- Context building ----------------------------------------------------

const SYSTEM_PROMPT = `You are a coding agent working from a terminal. You have a shell tool that runs commands in a sandbox and a todowrite tool for tracking multi-step work.

Be concise and direct; this is a CLI, so keep prose short and let tool calls do the work. Prioritize technical accuracy over agreement.

Working style:
- For any task with 3 or more steps, call todowrite first to lay out a plan, then keep it updated as you go. Keep exactly one item in_progress at a time. Mark an item completed only after you have verified it works — a command that ran, a test that passed — never on intent alone.
- Inspect the codebase with commands (ls, cat, grep) instead of guessing.
- When you finish, verify your work by running it.
- Reference code locations as file_path:line_number so they are easy to find.`;

async function codingInstructions(context: TurnContext): Promise<Message[]> {
  const conversation = context.exoharness.current.conversation;
  const messages: Message[] = [
    { role: "system", content: SYSTEM_PROMPT },
    {
      role: "developer",
      content:
        `<env>\n` +
        `Platform: ${os.platform()}\n` +
        `Today's date: ${new Date().toDateString()}\n` +
        `Files live in the sandbox; use the shell tool to read and change them.\n` +
        `</env>`,
    },
  ];

  // Re-surface the live task list every round so the model stays oriented
  // across many tool-call steps.
  const todos = await readTodos(conversation);
  if (todos.length > 0) {
    const rendered = todos
      .map((todo) => `- [${todo.status}] ${todo.content}`)
      .join("\n");
    messages.push({
      role: "developer",
      content: `Your current task list (from todowrite):\n${rendered}`,
    });
  }

  return messages;
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions: codingInstructions,
      registerTools: registerCodingTools,
    });
  },
});
