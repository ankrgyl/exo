import {
  defineHarness,
  registerBuiltInTools,
  registerLibraryToolModulePath,
  type HarnessToolRegistry,
  type TurnContext,
} from "@exo/harness";

import { runClaudeCodeHarnessTurn } from "../typescript/claude-code-harness";
import { runCodexHarnessTurn } from "../typescript/codex-harness";
import { modelFamily, type ModelFamily } from "../typescript/model-family";
import { resolveLlmBinding } from "../typescript/shared";
import { registerConfiguredAgentTools } from "../typescript/turn-loop";
import { exoInstructions } from "./harness";
import { resolveExoProfile } from "./profiles";

// The exo agent on a coding-agent executor, picked by model family: Anthropic
// models run on Claude Code, OpenAI models on codex. Exo's tools reach the
// executor through the registry bridge; shell stays native to the executor.
//
// Select with `--harness examples/exo/coding-executor-harness.ts` plus the
// executor's sandbox image (exo-claude-code-sandbox / exo-codex-sandbox) and
// networking enabled.

export function executorForFamily(
  family: ModelFamily,
): "claude-code" | "codex" | null {
  if (family === "anthropic") {
    return "claude-code";
  }
  if (family === "openai") {
    return "codex";
  }
  return null;
}

export default defineHarness({
  async runTurn(context) {
    const binding = await resolveLlmBinding(context);
    const executor = executorForFamily(modelFamily(binding.model));
    if (executor === null) {
      throw new Error(
        `no coding executor for model ${binding.model}: its family is unknown. Register the model under its upstream id, or use the default exo harness.`,
      );
    }
    const options = {
      instructions: exoInstructions,
      registerTools: registerCodingExecutorTools,
      // The sandbox boundary is the containment; decisions land in the
      // event log.
      approvals: "auto" as const,
    };
    if (executor === "claude-code") {
      await runClaudeCodeHarnessTurn(context, options);
      return;
    }
    await runCodexHarnessTurn(context, options);
  },
});

// registerExoTools minus the shell built-in: the executor brings its own.
export async function registerCodingExecutorTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  const profile = resolveExoProfile();
  registerBuiltInTools(
    tools,
    context,
    profile.builtInToolNames(context).filter((name) => name !== "shell"),
  );
  await profile.registerTools(tools, context);
  for (const modulePath of context.agentConfig.typescript?.toolModulePaths ??
    []) {
    await registerLibraryToolModulePath(tools, context, modulePath);
  }
  await registerConfiguredAgentTools(tools, context);
}
