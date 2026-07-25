import { describe, expect, it } from "vitest";

import {
  composeHarborPrompt,
  expectedResponseType,
  parseHarborRequest,
  parseHarborResponse,
  requestTarget,
} from "./harbor";

const taskStarted = parseHarborRequest({
  type: "task_started",
  message_id: "message-1",
  trial_id: "trial-1",
  task_name: "task",
  instruction: "Fix the task",
  conversation_id: "conversation-1",
  sandbox_id: "sandbox-1",
});

describe("Harbor protocol", () => {
  it("parses task_started and derives a stable target", () => {
    expect(taskStarted.type).toBe("task_started");
    expect(requestTarget(taskStarted)).toBe("harbor:trial-1:task_started");
    expect(expectedResponseType(taskStarted)).toBe("task_complete");
  });

  it("parses verification_result", () => {
    const request = parseHarborRequest({
      type: "verification_result",
      message_id: "message-2",
      trial_id: "trial-1",
      task_name: "task",
      conversation_id: "conversation-1",
      rewards: { reward: 1 },
      verifier_stdout: "passed",
      verifier_stderr: "",
      exception: null,
    });
    expect(expectedResponseType(request)).toBe("feedback_processed");
  });

  it("rejects an outbound response for another trial", () => {
    expect(() =>
      parseHarborResponse(
        JSON.stringify({
          type: "task_complete",
          trial_id: "trial-2",
        }),
        taskStarted,
      ),
    ).toThrow(/does not match active trial/);
  });

  it("rejects the wrong response phase", () => {
    expect(() =>
      parseHarborResponse(
        JSON.stringify({
          type: "feedback_processed",
          trial_id: "trial-1",
        }),
        taskStarted,
      ),
    ).toThrow(/response type must be task_complete/);
  });

  it("tells Exo exactly how to acknowledge the phase", () => {
    const prompt = composeHarborPrompt(
      taskStarted,
      "adapter-1",
      requestTarget(taskStarted),
    );
    expect(prompt).toContain("Fix the task");
    expect(prompt).toContain("send_adapter_message");
    expect(prompt).toContain("adapter-1");
    expect(prompt).toContain("harbor:trial-1:task_started");
    expect(prompt).toContain('"type":"task_complete"');
  });
});
