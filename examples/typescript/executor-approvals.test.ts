import { describe, expect, it } from "vitest";

import {
  claudePermissionResult,
  codexApprovalDecision,
} from "./executor-approvals";

describe("codexApprovalDecision", () => {
  it("answers command and file-change approvals by policy", () => {
    for (const method of [
      "item/commandExecution/requestApproval",
      "item/fileChange/requestApproval",
    ]) {
      expect(codexApprovalDecision("auto", method)).toEqual({
        decision: "accept",
      });
      expect(codexApprovalDecision("deny", method)).toEqual({
        decision: "decline",
      });
    }
  });

  it("grants no permissions under either policy", () => {
    expect(
      codexApprovalDecision("auto", "item/permissions/requestApproval"),
    ).toEqual({ scope: "turn", permissions: {} });
  });

  it("leaves non-approval methods to the caller", () => {
    expect(codexApprovalDecision("auto", "item/tool/call")).toBeUndefined();
    expect(codexApprovalDecision("deny", "thread/start")).toBeUndefined();
  });
});

describe("claudePermissionResult", () => {
  it("allows under auto", () => {
    expect(claudePermissionResult("auto", "Bash")).toEqual({
      behavior: "allow",
    });
  });

  it("denies with the tool named", () => {
    const result = claudePermissionResult("deny", "Bash");
    expect(result.behavior).toBe("deny");
    if (result.behavior === "deny") {
      expect(result.message).toContain("Bash");
    }
  });
});
