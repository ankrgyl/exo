import type { ExoProfile } from "./types";

import { registerGuardianTools } from "../guardian-tools";

export const bootstrapProfile: ExoProfile = {
  name: "bootstrap",
  builtInToolNames() {
    return ["shell", "inspect_tools", "manage_tool"];
  },
  registerTools(tools) {
    registerGuardianTools(tools);
  },
};
