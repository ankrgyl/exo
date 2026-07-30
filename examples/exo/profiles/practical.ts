import {
  HarnessToolRegistry,
  registerAdapterTools,
  registerSkillTools,
  type BuiltInToolName,
} from "@exo/harness";

import { registerGuardianTools } from "../guardian-tools";
import { registerIntrospectionTools } from "../introspection-tools";
import { registerMemoryTools } from "../memory-tools";
import { registerSandboxTools } from "../sandbox-tools";
import { registerSchedulerTools } from "../scheduler-tools";
import { registerTodoTools } from "../todo-tools";
import { registerWebTools } from "../web-tools";
import type { ExoProfile } from "./types";

export const practicalProfile: ExoProfile = {
  name: "practical",
  builtInToolNames(context) {
    const names = bootstrapBuiltInToolNames();
    if (context.agentConfig.enableAgentToolCreation) {
      names.push("install_agent_tool", "uninstall_agent_tool");
    }
    return names;
  },
  registerTools(tools, context) {
    const libraryTools = new HarnessToolRegistry(context);
    registerSchedulerTools(libraryTools);
    registerAdapterTools(libraryTools);
    registerIntrospectionTools(libraryTools);
    registerSandboxTools(libraryTools);
    registerMemoryTools(libraryTools);
    registerTodoTools(libraryTools);
    registerSkillTools(libraryTools);
    registerWebTools(libraryTools);
    for (const tool of libraryTools.instances()) {
      tools.register({ ...tool, source: "library" });
    }
    registerGuardianTools(tools);
  },
};

function bootstrapBuiltInToolNames(): BuiltInToolName[] {
  return ["shell", "inspect_tools", "manage_tool"];
}
