---
title: Custom Coding Agent
description: Build a coding agent one component at a time, and see how the harness supports each.
---

# Custom Coding Agent

This tutorial builds a coding agent — one that runs commands in a sandbox
and works through a multi-step task — by adding **one component at a time**
and seeing how the harness supports each. It builds directly on the two
hooks from the [Custom Agent Quickstart](./write-your-own-agent):
`registerTools` and `instructions`.

The finished file is
[`exoharness/examples/typescript/coding-agent-harness.ts`](https://github.com/exoharness/exo/blob/main/exoharness/examples/typescript/coding-agent-harness.ts).

## The loop is already there

A coding agent takes many steps per request — run a command, read the
output, run another, edit, re-run. You don't write that loop:
`runResponsesHarnessTurn` calls the model, runs any tools it requests,
appends the results, and calls the model again — repeating until the model
stops requesting tools (bounded by the agent's `maxToolRoundTrips`).

So everything below is just *what you plug into* that loop.

## Component 1: reuse a built-in tool

The harness ships a `shell` tool that runs commands inside the
conversation's sandbox. Because a `registerTools` hook owns the whole
registry, built-ins are opt-in — you add the one you want:

```ts
async function registerCodingTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  registerBuiltInTools(tools, context, ["shell"]);
  // (we add a custom tool next)
}
```

That alone is already a working coding agent: through the shell the model
can read files, search, edit, build, and run — all in the sandbox.

## Component 2: add a custom tool

Now a tool the harness doesn't ship: task tracking, so the model can hold a
plan across many steps. We define a `todowrite` tool where the model
rewrites its whole task list each call, and persist that list as a
conversation [artifact](../concepts/data-model) — the harness's durable
key/value storage — so it survives across turns:

```ts
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
```

The artifact helpers are small — the conversation exposes `writeArtifactJson`
/ `listArtifacts` / `readArtifactJson`, so we write the list to a fixed path
and read the latest version back:

```ts
const TODO_ARTIFACT_PATH = "coding-agent/todos.json";

async function readTodos(conversation: Conversation): Promise<Todo[]> {
  const latest = (await conversation.listArtifacts())
    .filter((v) => v.path === TODO_ARTIFACT_PATH)
    .sort((a, b) => b.version - a.version)[0];
  if (!latest) return [];
  return (
    (await conversation.readArtifactJson<Todo[]>({
      artifactId: latest.artifactId,
    })) ?? []
  );
}

async function writeTodos(conversation: Conversation, todos: Todo[]) {
  await conversation.writeArtifactJson({
    path: TODO_ARTIFACT_PATH,
    value: todos as unknown as JsonValue,
  });
}
```

Register it alongside `shell`:

```ts
await registerLibraryTools(tools, context, todoTool);
```

## Component 3: feed state back into context

Storing the list isn't enough — the model has to *see* it on every step.
The `instructions` hook (the same one from the quickstart) runs before each
model round, so read the list back there and append it:

```ts
const todos = await readTodos(conversation);
if (todos.length > 0) {
  const rendered = todos.map((t) => `- [${t.status}] ${t.content}`).join("\n");
  messages.push({
    role: "developer",
    content: `Your current task list (from todowrite):\n${rendered}`,
  });
}
```

Because the hook re-runs each round, the model always sees the up-to-date
plan as it works through the loop.

## The prompt

The `instructions` hook also returns the system prompt — it's just messages
you return. For this agent, a short one that names the tools and asks it to
verify its work:

```ts
const SYSTEM_PROMPT = `You are a coding agent working from a terminal. You have a shell tool that runs commands in a sandbox and a todowrite tool for tracking multi-step work.

Be concise and direct; this is a CLI, so keep prose short and let tool calls do the work. Prioritize technical accuracy over agreement.

Working style:
- For any task with 3 or more steps, call todowrite first to lay out a plan, then keep it updated as you go. Keep exactly one item in_progress at a time. Mark an item completed only after you have verified it works — a command that ran, a test that passed — never on intent alone.
- Inspect the codebase with commands (ls, cat, grep) instead of guessing.
- When you finish, verify your work by running it.
- Reference code locations as file_path:line_number so they are easy to find.`;
```

The full hook assembles the prompt, a small live-context block, and the
todo list:

```ts
async function codingInstructions(context: TurnContext): Promise<Message[]> {
  const conversation = context.exoharness.current.conversation;
  const messages: Message[] = [
    { role: "system", content: SYSTEM_PROMPT },
    {
      role: "developer",
      content:
        `<env>\nPlatform: ${os.platform()}\n` +
        `Today's date: ${new Date().toDateString()}\n` +
        `Files live in the sandbox; use the shell tool to read and change them.\n</env>`,
    },
  ];
  const todos = await readTodos(conversation);
  if (todos.length > 0) {
    const rendered = todos.map((t) => `- [${t.status}] ${t.content}`).join("\n");
    messages.push({
      role: "developer",
      content: `Your current task list (from todowrite):\n${rendered}`,
    });
  }
  return messages;
}
```

## Put it together

Both hooks go into the same `runResponsesHarnessTurn` call:

```ts
export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions: codingInstructions,
      registerTools: registerCodingTools,
    });
  },
});
```

## Run it

```bash
exo --harness typescript agent create "Coder" \
  --module exoharness/examples/typescript/coding-agent-harness.ts \
  --model gpt-5.5 \
  --sandbox-image python:3.12-slim
exo conversation create coder "Build"
exo repl --agent coder --conversation coder-build
```

The default sandbox image is `ubuntu:24.04`, which is bare — no Python,
Node, etc. Setting `--sandbox-image` on the *agent* makes every
conversation it owns boot that image; the model installs anything else it
needs with the shell tool.

A real run (via `conversation send`), asking it to write a buggy
`fizzbuzz.py`, find the bug, and fix it — abbreviated to show the shape:

```text
user: write fizzbuzz.py for 1..15 with an off-by-one that stops at 14;
      plan with todowrite, run it, fix it, run again to confirm.

assistant: [tool_call todowrite]  # 4-item plan, one in_progress
assistant: [tool_call shell] cat > fizzbuzz.py …   # range(1, 15)
assistant: [tool_call shell] python3 fizzbuzz.py    # → 1..14, last line "14"
assistant: [tool_call todowrite]  # mark "observe bug" done, next in_progress
assistant: [tool_call shell] # replace range(1, 15) → range(1, 16)
assistant: [tool_call shell] python3 fizzbuzz.py    # → 1..15, last "FizzBuzz"
assistant: [tool_call todowrite]  # all completed, remaining: 0
assistant: Done. Fixed fizzbuzz.py:1 (range(1, 15) → range(1, 16)); final
           run reaches 15.
```

Each round saw the current todo list re-injected, so the plan stayed intact
across all the steps — and the agent only marked steps completed after a
command confirmed them.

## Where to go next

- Register more built-in tools, or add custom tools for anything the shell
  can't express directly.
- Read [Executors & Harnesses](../concepts/executors) to see the
  built-in `codex` / `claude-code` / `cursor` harnesses, which wrap full
  coding-agent runtimes with durable exoharness state.
