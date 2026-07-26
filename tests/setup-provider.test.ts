import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";

function runSetupFunction(command: string, input?: string) {
  return spawnSync("bash", ["-c", `source ./setup.sh; ${command}`], {
    cwd: process.cwd(),
    encoding: "utf8",
    input,
  });
}

describe("setup model providers", () => {
  it("maps Venice to its key, endpoint, and default model", () => {
    const result = runSetupFunction(
      'configure_model_provider venice; printf "%s\\n" "$MODEL_PROVIDER_LABEL|$MODEL_API_KEY_ENV|$MODEL_BASE_URL|$DEFAULT_UPSTREAM_MODEL"',
    );

    expect(result.status).toBe(0);
    expect(result.stdout.trim()).toBe(
      "Venice|VENICE_API_KEY|https://api.venice.ai/api/v1|zai-org-glm-5",
    );
  });

  it("offers Venice as the third interactive provider", () => {
    const result = runSetupFunction("choose_model_provider", "3\n");

    expect(result.status).toBe(0);
    expect(result.stdout).toBe("venice");
    expect(result.stderr).toContain("3) Venice");
  });
});
