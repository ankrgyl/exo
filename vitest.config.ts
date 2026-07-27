import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

// Mirror the tsconfig path aliases so tests can import modules that use them.
export default defineConfig({
  resolve: {
    alias: {
      "@exo/harness/tool": fileURLToPath(
        new URL("./exoharness/typescript/harness/tool.ts", import.meta.url),
      ),
      "@exo/harness": fileURLToPath(
        new URL("./exoharness/typescript/harness/index.ts", import.meta.url),
      ),
      "@exo/model-runtime/responses": fileURLToPath(
        new URL(
          "./exoharness/typescript/model-runtime/responses.ts",
          import.meta.url,
        ),
      ),
      "@exo/model-runtime/shared": fileURLToPath(
        new URL(
          "./exoharness/typescript/model-runtime/shared.ts",
          import.meta.url,
        ),
      ),
      "@exo/model-runtime/turn-loop": fileURLToPath(
        new URL(
          "./exoharness/typescript/model-runtime/turn-loop.ts",
          import.meta.url,
        ),
      ),
    },
  },
});
