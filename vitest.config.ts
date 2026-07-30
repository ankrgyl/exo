import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

// Mirror the tsconfig path aliases so tests can import modules that use them.
export default defineConfig({
  resolve: {
    alias: {
      "@exo/harness/tool": fileURLToPath(
        new URL("./typescript/harness/tool.ts", import.meta.url),
      ),
      "@exo/harness": fileURLToPath(
        new URL("./typescript/harness/index.ts", import.meta.url),
      ),
      "@exo/model-runtime/responses": fileURLToPath(
        new URL("./typescript/model-runtime/responses.ts", import.meta.url),
      ),
      "@exo/model-runtime/cost": fileURLToPath(
        new URL("./typescript/model-runtime/cost.ts", import.meta.url),
      ),
    },
  },
});
