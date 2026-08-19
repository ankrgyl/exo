import { accessSync, constants } from "node:fs";

import { HarnessToolRegistry, type TurnContext } from "@exo/harness";
import { describe, expect, it } from "vitest";

import {
  DEFERRED_SCRIPT,
  GUARDIAN_SCRIPT,
  parseRebuildReason,
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

  it("defines the narrow asynchronous rebuild facade with a reason", () => {
    const tool = rebuildAndRestartExoTool();

    expect(tool.source).toBe("built_in");
    expect(tool.definition.name).toBe("rebuild_and_restart_exo");
    expect(tool.definition.parameters).toEqual({
      type: "object",
      additionalProperties: false,
      properties: {
        reason: {
          type: ["string", "null"],
          description:
            "Short free-text note describing why this rebuild was requested, for example the change being activated. Prefer a concrete description over null.",
        },
      },
      required: ["reason"],
    });
  });

  it("resolves host scripts that exist and are executable", () => {
    // A wrong relative path here is invisible until a self-update is queued:
    // the spawn fails with ENOENT and the update record stays "queued".
    for (const script of [GUARDIAN_SCRIPT, DEFERRED_SCRIPT]) {
      expect(() => accessSync(script, constants.X_OK)).not.toThrow();
    }
  });

  it("parses rebuild reasons for the durable update record", () => {
    expect(parseRebuildReason({ reason: "Add webhook adapter" })).toBe(
      "Add webhook adapter",
    );
    expect(parseRebuildReason({ reason: "  " })).toBeNull();
    expect(parseRebuildReason({ reason: null })).toBeNull();
    expect(() => parseRebuildReason({ reason: 12 })).toThrow(
      "rebuild_and_restart_exo reason must be a string or null",
    );
  });
});
