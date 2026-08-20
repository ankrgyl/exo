import { practicalProfile, registerPracticalTools } from "./practical";
import type { ExoProfile } from "./types";

/**
 * `practical` without host-side web access.
 *
 * web_search and web_fetch run in the harness runner process rather than in
 * the sandbox, so a sandbox with networking disabled — or a benchmark harness
 * that firewalls the task container — does not take them away. Withholding the
 * tools is the only way to actually remove that reach.
 *
 * This exists for benchmark runs whose tasks are derived from public history:
 * on SWE-bench a task id is the upstream pull request number, so an agent with
 * web access can fetch the reference patch and score without solving anything.
 * Everything else practical offers — memory, skills, tool creation, the
 * guardian — stays, so self-improvement is still measurable.
 */
export const offlineProfile: ExoProfile = {
  name: "offline",
  builtInToolNames: practicalProfile.builtInToolNames,
  registerTools(tools, context) {
    registerPracticalTools(tools, context, { web: false });
  },
};
