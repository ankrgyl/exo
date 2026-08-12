import { describe, expect, it } from "vitest";

import {
  composeTrialPrompt,
  parseTrialRequest,
  parseTrialResponse,
  parseTrialSocketRequest,
} from "./trial";

const request = parseTrialRequest({
  type: "trial_run",
  request_id: "request-1",
  target: "trial-1",
  container_id: "container-1",
  instructions: "Fix the service",
});

describe("trial adapter protocol", () => {
  it("parses cancellation as a socket control request", () => {
    expect(
      parseTrialSocketRequest({
        type: "trial_cancel",
        request_id: "request-1",
        target: "trial-1",
      }),
    ).toEqual({
      type: "trial_cancel",
      request_id: "request-1",
      target: "trial-1",
    });
  });

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
          snapshot_id: "snapshot-1",
          summary: "done",
        }),
        request,
      ),
    ).toEqual({
      type: "trial_complete",
      request_id: "request-1",
      target: "trial-1",
      conversation_id: "conversation-1",
      snapshot_id: "snapshot-1",
      summary: "done",
    });
  });

  it("validates a cancellation snapshot response", () => {
    expect(
      parseTrialResponse(
        JSON.stringify({
          type: "trial_cancelled",
          request_id: "request-1",
          target: "trial-1",
          conversation_id: "conversation-1",
          snapshot_id: "snapshot-1",
        }),
        request,
      ),
    ).toEqual({
      type: "trial_cancelled",
      request_id: "request-1",
      target: "trial-1",
      conversation_id: "conversation-1",
      snapshot_id: "snapshot-1",
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

  it("composes and validates feedback in the restored trial", () => {
    const feedback = parseTrialRequest({
      type: "trial_feedback",
      request_id: "feedback-1",
      target: "trial-1",
      instructions: "Extract reusable lessons and improve your tools.",
      feedback: "The verifier found an edge case.",
    });
    const prompt = composeTrialPrompt(feedback, "adapter-1", false);
    expect(prompt).toContain("restored snapshot");
    expect(prompt).toContain("The verifier found an edge case.");
    expect(prompt).toContain("Extract reusable lessons");
    expect(prompt).toContain('"type":"feedback_complete"');

    expect(
      parseTrialResponse(
        JSON.stringify({
          type: "feedback_complete",
          request_id: "feedback-1",
          target: "trial-1",
          conversation_id: "conversation-1",
          summary: "learned",
        }),
        feedback,
      ),
    ).toEqual({
      type: "feedback_complete",
      request_id: "feedback-1",
      target: "trial-1",
      conversation_id: "conversation-1",
      summary: "learned",
    });
  });
});
