import { describe, expect, it } from "vitest";

import { createToolRegistry, type TurnContext } from "@exo/harness";

import {
  executorForFamily,
  registerCodingExecutorTools,
} from "./coding-executor-harness";

function fakeContext(enableAgentToolCreation = false): TurnContext {
  return {
    agentConfig: { typescript: undefined, enableAgentToolCreation },
    conversationConfig: {},
  } as unknown as TurnContext;
}

describe("executorForFamily", () => {
  it("routes each family to its executor", () => {
    expect(executorForFamily("anthropic")).toBe("claude-code");
    expect(executorForFamily("openai")).toBe("codex");
    expect(executorForFamily("unknown")).toBeNull();
  });
});

describe("registerCodingExecutorTools", () => {
  it("registers the exo agent tools but not shell", async () => {
    const context = fakeContext();
    const tools = createToolRegistry(context);
    await registerCodingExecutorTools(tools, context);
    expect(tools.get("shell")).toBeUndefined();
    for (const name of [
      "remember",
      "todowrite",
      "web_search",
      "rewind_sandbox",
      "rebuild_and_restart_exo",
      "manage_tool",
    ]) {
      expect(tools.get(name), name).toBeDefined();
    }
  });

  it("includes the agent-tool-creation built-ins when enabled", async () => {
    const context = fakeContext(true);
    const tools = createToolRegistry(context);
    await registerCodingExecutorTools(tools, context);
    expect(tools.get("install_agent_tool")).toBeDefined();
    expect(tools.get("shell")).toBeUndefined();
  });
});
