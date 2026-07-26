import { HarnessToolRegistry, type TurnContext } from "@exo/harness";
import { describe, expect, it } from "vitest";

import {
  rebuildAndRestartExoTool,
  registerGuardianTools,
} from "./guardian-tools";

describe("guardian tools", () => {
  it("registers only rebuild_and_restart_exo", () => {
    const registry = new HarnessToolRegistry({} as TurnContext);

    registerGuardianTools(registry);

    expect(registry.definitions().map(({ name }) => name)).toEqual([
      "rebuild_and_restart_exo",
    ]);
  });

  it("defines the narrow asynchronous rebuild facade", () => {
    const tool = rebuildAndRestartExoTool();

    expect(tool.source).toBe("built_in");
    expect(tool.definition.name).toBe("rebuild_and_restart_exo");
    expect(tool.definition.parameters).toEqual({
      type: "object",
      additionalProperties: false,
      properties: {},
      required: [],
    });
  });
});
