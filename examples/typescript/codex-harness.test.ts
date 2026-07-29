import { describe, expect, it } from "vitest";

import type { TurnContext } from "@exo/harness";

import {
  buildCodexDynamicTools,
  codexChatgptLoginParams,
  codexSandboxCommand,
  codexSandboxEnv,
  credentialFingerprint,
  resolveCodexCredential,
} from "./codex-harness";

describe("Codex harness authentication", () => {
  it("exposes Exo adapter tools through the Codex app-server", () => {
    const tools = buildCodexDynamicTools({
      conversationConfig: { shellProgram: "/bin/bash" },
    } as TurnContext);

    expect(
      tools.map((tool) =>
        typeof tool === "object" && tool !== null && !Array.isArray(tool)
          ? tool.name
          : null,
      ),
    ).toEqual([
      "create_adapter",
      "list_adapters",
      "disable_adapter",
      "delete_adapter",
      "send_adapter_message",
    ]);
  });

  it("passes API keys only through the API-key environment", () => {
    const env = codexSandboxEnv(
      { name: "gpt", model: "gpt-5.5", apiKey: "sk-test" },
      {
        kind: "api_key",
        token: "sk-test",
        fingerprint: credentialFingerprint("api_key", "sk-test"),
      },
    );

    expect(env.OPENAI_API_KEY).toBe("sk-test");
    expect(env.CODEX_ACCESS_TOKEN).toBeUndefined();
    expect(env.CODEX_HOME).not.toContain("sk-test");
  });

  it("keeps ChatGPT access tokens out of the sandbox environment", () => {
    const env = codexSandboxEnv(
      { name: "gpt", model: "gpt-5.5" },
      {
        kind: "chatgpt",
        token: "chatgpt-test-token",
        accountId: "account-test",
        fingerprint: credentialFingerprint("chatgpt", "chatgpt-test-token"),
      },
    );

    expect(env.CODEX_ACCESS_TOKEN).toBeUndefined();
    expect(env.OPENAI_API_KEY).toBeUndefined();
    expect(env.CODEX_HOME).not.toContain("chatgpt-test-token");
  });

  it("uses Codex external auth with the ChatGPT account ID", () => {
    expect(
      codexChatgptLoginParams("chatgpt-test-token", "account-test"),
    ).toEqual({
      type: "chatgptAuthTokens",
      accessToken: "chatgpt-test-token",
      chatgptAccountId: "account-test",
    });
  });

  it("does not forward arbitrary host Codex variables", () => {
    const previous = process.env.CODEX_BIN;
    process.env.CODEX_BIN = "/host/secret/codex";
    try {
      const env = codexSandboxEnv(
        { name: "gpt", model: "gpt-5.5", apiKey: "sk-test" },
        {
          kind: "api_key",
          token: "sk-test",
          fingerprint: credentialFingerprint("api_key", "sk-test"),
        },
      );
      expect(env.CODEX_BIN).toBeUndefined();
    } finally {
      if (previous === undefined) {
        delete process.env.CODEX_BIN;
      } else {
        process.env.CODEX_BIN = previous;
      }
    }
  });

  it("removes model credentials from command subprocess environments", () => {
    const command = codexSandboxCommand({
      conversationConfig: { shellProgram: "/bin/bash" },
    } as TurnContext).join(" ");

    expect(command).toContain("shell_environment_policy.exclude");
    expect(command).toContain("OPENAI_API_KEY");
    expect(command).toContain("CODEX_ACCESS_TOKEN");
  });

  it("rejects custom endpoints in subscription mode before authentication", async () => {
    await expect(
      resolveCodexCredential({
        name: "custom",
        model: "gpt-5.5",
        baseUrl: "https://example.invalid/v1",
      }),
    ).rejects.toThrow("custom base URL but no API-key secret");
  });

  it("uses non-reversible credential fingerprints", () => {
    const fingerprint = credentialFingerprint("chatgpt", "secret-token");
    expect(fingerprint).toMatch(/^[0-9a-f]{64}$/);
    expect(fingerprint).not.toContain("secret-token");
    expect(credentialFingerprint("api_key", "secret-token")).not.toBe(
      fingerprint,
    );
  });
});
