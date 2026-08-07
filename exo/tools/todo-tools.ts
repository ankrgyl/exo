import type {
  Conversation,
  HarnessToolRegistry,
  JsonObject,
  JsonValue,
  Message,
  ToolInstance,
  ToolResult,
  TurnContext,
} from "@exo/harness";

// Task tracking for multi-step work, following the todowrite pattern used by
// coding agents like OpenCode and Claude Code: the model rewrites its FULL
// task list on every call, and the current list is injected back into the
// prompt each model round so the plan survives long tool loops. Scoped to the
// conversation (unlike memory, which is agent-global) because a task list
// belongs to the work in one conversation.
const TODO_ARTIFACT_PATH = "todos/exo-todos.json";
// Soft caps so always-injecting the list cannot grow the prompt without bound.
const MAX_TODOS = 50;
const MAX_CONTENT_CHARS = 300;

const TODO_STATUSES = [
  "pending",
  "in_progress",
  "completed",
  "cancelled",
] as const;

type TodoStatus = (typeof TODO_STATUSES)[number];

interface Todo {
  content: string;
  status: TodoStatus;
}

// The artifact subset both Conversation and the test fake provide.
type TodoHandle = Pick<
  Conversation,
  "listArtifacts" | "readArtifactJson" | "writeArtifactJson"
>;

function todoHandle(context: TurnContext): TodoHandle {
  return context.exoharness.current.conversation;
}

async function readTodos(handle: TodoHandle): Promise<Todo[]> {
  let raw: unknown;
  try {
    const latest = (await handle.listArtifacts())
      .filter((artifact) => artifact.path === TODO_ARTIFACT_PATH)
      .sort((a, b) => b.version - a.version)[0];
    if (latest === undefined) {
      return [];
    }
    raw = await handle.readArtifactJson({
      artifactId: latest.artifactId,
      version: latest.version,
    });
  } catch {
    // A corrupt or unreadable list (including a transient listArtifacts
    // failure) is not worth bricking prompt assembly over; unlike memory it
    // is fully regenerable, so treat it as empty and let the next todowrite
    // overwrite it.
    return [];
  }
  return isTodoList(raw) ? raw : [];
}

async function writeTodos(handle: TodoHandle, todos: Todo[]): Promise<void> {
  await handle.writeArtifactJson({
    path: TODO_ARTIFACT_PATH,
    value: todos as unknown as JsonValue,
  });
}

function todoWriteTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "todowrite",
      description:
        "Record and update your task list for the current conversation. Rewrite the FULL list on every call; the list you write replaces the previous one and is shown back to you each turn. Use it for any task needing 3 or more steps; skip it for trivial one-step work. Keep exactly one item in_progress, and mark an item completed only after you have verified it is actually done. Write an empty list to clear it.",
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
                content: {
                  type: "string",
                  description: "The task, phrased as an imperative step.",
                },
                status: {
                  type: "string",
                  enum: [...TODO_STATUSES],
                },
              },
              required: ["content", "status"],
            },
          },
        },
        required: ["todos"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        if (!isTodoList(args.todos)) {
          return {
            ok: false,
            error:
              "todos must be an array of { content, status } items with status one of pending | in_progress | completed | cancelled",
          };
        }
        const todos = args.todos;
        if (todos.length > MAX_TODOS) {
          return {
            ok: false,
            error: `list exceeds ${MAX_TODOS} items; consolidate steps`,
          };
        }
        const oversized = todos.find(
          (todo) =>
            todo.content.trim().length === 0 ||
            todo.content.length > MAX_CONTENT_CHARS,
        );
        if (oversized !== undefined) {
          return {
            ok: false,
            error: `each todo needs non-empty content of at most ${MAX_CONTENT_CHARS} characters`,
          };
        }
        await writeTodos(todoHandle(execution.context), todos);
        const remaining = todos.filter(
          (todo) => todo.status === "pending" || todo.status === "in_progress",
        ).length;
        return { ok: true, total: todos.length, remaining };
      },
    },
  };
}

export function createTodoToolInstances(): ToolInstance[] {
  return [todoWriteTool()];
}

export function registerTodoTools(registry: HarnessToolRegistry): void {
  for (const tool of createTodoToolInstances()) {
    registry.register(tool);
  }
}

// Build the developer message that shows the current task list to the model.
// Returns null when the list is empty or every item is done — no need to
// spend prompt space on a finished plan.
export async function todoInstruction(
  context: TurnContext,
): Promise<Message | null> {
  const todos = await readTodos(todoHandle(context));
  const open = todos.filter(
    (todo) => todo.status === "pending" || todo.status === "in_progress",
  );
  if (open.length === 0) {
    return null;
  }
  const lines = todos.map((todo) => `- [${todo.status}] ${todo.content}`);
  return {
    role: "developer",
    content: `Your current task list for this conversation (from todowrite). Keep it updated as you work: rewrite the full list, keep one item in_progress, and mark items completed only once verified.\n\n${lines.join("\n")}`,
  };
}

function isTodoList(value: unknown): value is Todo[] {
  return (
    Array.isArray(value) &&
    value.every(
      (item) =>
        isRecord(item) &&
        typeof item.content === "string" &&
        typeof item.status === "string" &&
        (TODO_STATUSES as readonly string[]).includes(item.status),
    )
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
