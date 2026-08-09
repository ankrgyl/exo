import { describe, expect, it } from "vitest";

import { composeTrialPrompt, parseTrialResponse, parseTrialRun } from "./trial";

const request = parseTrialRun({
  type: "trial_run",
  request_id: "request-1",
  target: "trial-1",
  container_id: "container-1",
  instructions: "Fix the service",
});

describe("trial adapter protocol", () => {
  it("parses a trial and composes the explicit completion protocol", () => {
    const prompt = composeTrialPrompt(request, "adapter-1", false);
    expect(prompt).toContain("Fix the service");
    expect(prompt).toContain("send_adapter_message");
    expect(prompt).toContain("target `trial-1`");
    expect(prompt).toContain('"request_id":"request-1"');
  });

  it("validates the runtime-completed response", () => {
    expect(
      parseTrialResponse(
        JSON.stringify({
          type: "trial_complete",
          request_id: "request-1",
          target: "trial-1",
          conversation_id: "conversation-1",
          summary: "done",
        }),
        request,
      ),
    ).toEqual({
      type: "trial_complete",
      request_id: "request-1",
      target: "trial-1",
      conversation_id: "conversation-1",
      summary: "done",
    });
  });

  it("validates the runtime-started response", () => {
    expect(
      parseTrialResponse(
        JSON.stringify({
          type: "trial_started",
          request_id: "request-1",
          target: "trial-1",
          conversation_id: "conversation-1",
        }),
        request,
      ),
    ).toEqual({
      type: "trial_started",
      request_id: "request-1",
      target: "trial-1",
      conversation_id: "conversation-1",
    });
  });
});
