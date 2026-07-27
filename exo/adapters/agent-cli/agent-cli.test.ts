import { describe, expect, it } from "vitest";

import {
  composeMessageText,
  defaultSocketPath,
  parseAgentCliRequest,
  resolveSandboxCwd,
} from "./agent-cli";

describe("resolveSandboxCwd", () => {
  it("maps the mount root itself", () => {
    expect(
      resolveSandboxCwd(
        "/Users/me/projects",
        "/Users/me/projects",
        "/agent-cli",
      ),
    ).toBe("/agent-cli");
  });

  it("maps nested directories under the root", () => {
    expect(
      resolveSandboxCwd(
        "/Users/me/projects/foo/bar",
        "/Users/me/projects",
        "/agent-cli",
      ),
    ).toBe("/agent-cli/foo/bar");
  });

  it("tolerates trailing slashes", () => {
    expect(
      resolveSandboxCwd(
        "/Users/me/projects/foo/",
        "/Users/me/projects/",
        "/agent-cli/",
      ),
    ).toBe("/agent-cli/foo");
  });

  it("rejects directories outside the root", () => {
    expect(
      resolveSandboxCwd("/tmp/elsewhere", "/Users/me/projects", "/agent-cli"),
    ).toBeNull();
  });

  it("rejects sibling directories with a shared prefix", () => {
    expect(
      resolveSandboxCwd(
        "/Users/me/projects-other",
        "/Users/me/projects",
        "/agent-cli",
      ),
    ).toBeNull();
  });
});

describe("composeMessageText", () => {
  it("includes the sandbox cwd and the prompt when inside the mount", () => {
    const text = composeMessageText(
      { cwd: "/Users/me/projects/foo", prompt: "set up node here" },
      "/Users/me/projects",
      "/agent-cli",
    );
    expect(text).toContain("/agent-cli/foo");
    expect(text).toContain("cd /agent-cli/foo");
    expect(text.endsWith("set up node here")).toBe(true);
  });

  it("warns when the cwd is outside the mount", () => {
    const text = composeMessageText(
      { cwd: "/tmp/elsewhere", prompt: "hello" },
      "/Users/me/projects",
      "/agent-cli",
    );
    expect(text).toContain("OUTSIDE the workspace mount");
    expect(text).toContain("/tmp/elsewhere");
    expect(text.endsWith("hello")).toBe(true);
  });
});

describe("parseAgentCliRequest", () => {
  it("accepts a valid request", () => {
    expect(parseAgentCliRequest({ cwd: "/tmp", prompt: "hi" })).toEqual({
      cwd: "/tmp",
      prompt: "hi",
    });
  });

  it("rejects relative cwd", () => {
    expect(() => parseAgentCliRequest({ cwd: "tmp", prompt: "hi" })).toThrow(
      /absolute path/,
    );
  });

  it("rejects empty prompts", () => {
    expect(() => parseAgentCliRequest({ cwd: "/tmp", prompt: "  " })).toThrow(
      /non-empty/,
    );
  });

  it("rejects non-object payloads", () => {
    expect(() => parseAgentCliRequest(["nope"])).toThrow(/JSON object/);
  });
});

describe("defaultSocketPath", () => {
  it("lives under the home directory", () => {
    expect(defaultSocketPath("/Users/me")).toBe(
      "/Users/me/.exo/agent-cli.sock",
    );
  });
});
