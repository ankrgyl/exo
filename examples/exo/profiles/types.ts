import type {
  BuiltInToolName,
  HarnessToolRegistry,
  TurnContext,
} from "@exo/harness";

export type ExoProfileName = "bootstrap" | "practical";

export interface ExoProfile {
  name: ExoProfileName;
  builtInToolNames(context: TurnContext): BuiltInToolName[];
  registerTools(
    tools: HarnessToolRegistry,
    context: TurnContext,
  ): Promise<void> | void;
}
