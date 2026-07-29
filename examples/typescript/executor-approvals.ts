import type { PermissionResult } from "@anthropic-ai/claude-agent-sdk";

import type { JsonValue } from "@exo/harness";

// Approval policy for the coding-agent executors. "auto" approves inside the
// sandbox boundary (the containment the presets already rely on); "deny"
// refuses anything the runtime asks about. There is no interactive path yet:
// TurnContext has no ask-the-user channel, so a real approval UX needs
// substrate support first. Callers record every decision as an event.
export type CodingApprovalPolicy = "auto" | "deny";

const CODEX_APPROVAL_METHODS = new Set([
  "item/commandExecution/requestApproval",
  "item/fileChange/requestApproval",
]);

// The answer to a codex approval request, or undefined for non-approval
// methods. Permission requests grant nothing either way; without this the
// app-server defaults silently declined fileChange even in "auto".
export function codexApprovalDecision(
  policy: CodingApprovalPolicy,
  method: string,
): JsonValue | undefined {
  if (CODEX_APPROVAL_METHODS.has(method)) {
    return { decision: policy === "deny" ? "decline" : "accept" };
  }
  if (method === "item/permissions/requestApproval") {
    return { scope: "turn", permissions: {} };
  }
  return undefined;
}

// Claude Code asks per tool use; without a handler a prompt stalls the CLI.
export function claudePermissionResult(
  policy: CodingApprovalPolicy,
  toolName: string,
): PermissionResult {
  return policy === "deny"
    ? {
        behavior: "deny",
        message: `${toolName} denied by the harness approval policy`,
      }
    : { behavior: "allow" };
}
