import type {
  HarnessToolRegistry,
  ToolDefinition,
  ToolInstance,
} from "@exo/harness";

// Exo tools come in two layers: the TypeScript registry defines what the
// model can see and call, and the Rust tool runtime (ExoToolRuntime /
// execute_tool in crates/executor) executes anything the TypeScript handler
// delegates to it. A "host tool" is the standard bridge between the two: its
// definition lives here so the model can discover it, and its handler forwards
// the call to the Rust runtime function of the same name. To add a Rust-backed
// tool, implement the match arm in Rust, then register the definition with
// registerHostTool from a library tool module.

export function registerHostTool(
  registry: HarnessToolRegistry,
  definition: ToolDefinition,
): void {
  registry.register(hostTool(definition));
}

export function hostTool(definition: ToolDefinition): ToolInstance {
  return {
    source: "built_in",
    definition,
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: definition.name,
          arguments: args,
        });
      },
    },
  };
}
