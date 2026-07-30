import { HarnessToolRegistry, type TurnContext } from "@exo/harness";
import { describe, expect, it } from "vitest";

import { resolveExoProfile } from "./index";

describe("Exo profiles", () => {
  it("defaults to the practical profile", () => {
    expect(resolveExoProfile(undefined).name).toBe("practical");
  });

  it("gives bootstrap exactly the recovery capabilities", () => {
    const profile = resolveExoProfile("bootstrap");
    const context = {
      agentConfig: { enableAgentToolCreation: false },
    } as TurnContext;
    const builtInNames = profile.builtInToolNames(context);
    expect(builtInNames).toEqual(["shell", "inspect_tools", "manage_tool"]);

    const tools = new HarnessToolRegistry(context);
    profile.registerTools(tools, context);
    expect([
      ...builtInNames,
      ...tools.definitions().map(({ name }) => name),
    ]).toEqual([
      "shell",
      "inspect_tools",
      "manage_tool",
      "rebuild_and_restart_exo",
    ]);
  });

  it("keeps practical extensions classified as library tools", () => {
    const profile = resolveExoProfile("practical");
    const context = {
      agentConfig: { enableAgentToolCreation: false },
    } as TurnContext;
    const tools = new HarnessToolRegistry(context);
    expect(profile.builtInToolNames(context)).toEqual([
      "shell",
      "inspect_tools",
      "manage_tool",
    ]);
    profile.registerTools(tools, context);

    expect(tools.get("create_adapter")?.source).toBe("library");
    expect(tools.get("snapshot_sandbox")?.source).toBe("library");
    expect(tools.get("web_search")?.source).toBe("library");
    expect(tools.get("rebuild_and_restart_exo")?.source).toBe("built_in");
    expect([
      ...profile.builtInToolNames(context),
      ...tools
        .instances()
        .filter(({ source }) => source === "built_in")
        .map(({ definition }) => definition.name),
    ]).toEqual([
      "shell",
      "inspect_tools",
      "manage_tool",
      "rebuild_and_restart_exo",
    ]);
  });

  it("exposes legacy agent-tool creation only when enabled", () => {
    const profile = resolveExoProfile("practical");
    const context = {
      agentConfig: { enableAgentToolCreation: true },
    } as TurnContext;

    expect(profile.builtInToolNames(context)).toEqual([
      "shell",
      "inspect_tools",
      "manage_tool",
      "install_agent_tool",
      "uninstall_agent_tool",
    ]);
  });

  it("rejects unknown profiles", () => {
    expect(() => resolveExoProfile("unknown")).toThrow(
      "expected bootstrap or practical",
    );
  });
});
