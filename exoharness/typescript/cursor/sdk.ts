import {
  Agent,
  CursorAgentError,
  type Run,
  type RunResult,
  type SDKAgent,
  type SDKMessage,
} from "@cursor/sdk";

import { cursorModelId } from "./protocol";

export interface CursorSdkRunnerOptions {
  apiKey?: string;
  cwd?: string;
  model: string;
  name?: string;
  onDelta?: (update: unknown) => Promise<void> | void;
  onMessage?: (message: SDKMessage) => Promise<void> | void;
  onRunStarted?: (args: {
    agentId: string;
    runId: string;
  }) => Promise<void> | void;
}

export interface CursorSdkRunResult {
  agentId: string;
  runId: string;
  result: RunResult;
}

export class CursorSdkSession {
  private agent: SDKAgent | null = null;
  private key: string | null = null;

  async run(
    prompt: string,
    options: CursorSdkRunnerOptions,
  ): Promise<CursorSdkRunResult> {
    const key = cursorAgentKey(options);
    if (this.agent && this.key !== key) {
      await this.close();
    }
    if (!this.agent) {
      this.agent = await createCursorAgent(options);
      this.key = key;
    }

    try {
      const agent = this.agent;
      const run = await agent.send(prompt, {
        local: { force: true },
        onDelta: options.onDelta
          ? async ({ update }: { update: unknown }) => {
              await options.onDelta?.(update);
            }
          : undefined,
      });
      await options.onRunStarted?.({ agentId: agent.agentId, runId: run.id });
      await streamCursorMessages(run, options.onMessage);
      const result = await run.wait();
      return {
        agentId: agent.agentId,
        runId: run.id,
        result,
      };
    } catch (error) {
      await this.close();
      throw error;
    }
  }

  async close(): Promise<void> {
    const agent = this.agent;
    this.agent = null;
    this.key = null;
    if (agent) {
      await agent[Symbol.asyncDispose]();
    }
  }
}

export async function runCursorSdkTurn(
  prompt: string,
  options: CursorSdkRunnerOptions,
): Promise<CursorSdkRunResult> {
  const session = new CursorSdkSession();
  try {
    return await session.run(prompt, options);
  } finally {
    await session.close();
  }
}

export function cursorSdkErrorMessage(error: unknown): string {
  if (error instanceof CursorAgentError) {
    return `${error.message}${error.isRetryable ? " (retryable)" : ""}`;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

export function cursorSdkCwd(): string {
  return process.env.CURSOR_CWD ?? process.cwd();
}

export function cursorSdkApiKey(): string | undefined {
  return process.env.CURSOR_API_KEY;
}

async function createCursorAgent(
  options: CursorSdkRunnerOptions,
): Promise<SDKAgent> {
  return Agent.create({
    apiKey: options.apiKey,
    name: options.name,
    model: { id: cursorModelId(options.model) },
    local: {
      cwd: options.cwd,
      settingSources: [],
      sandboxOptions: { enabled: false },
    },
  });
}

function cursorAgentKey(options: CursorSdkRunnerOptions): string {
  return JSON.stringify({
    api_key: options.apiKey ? "set" : "env",
    cwd: options.cwd ?? null,
    model: cursorModelId(options.model),
    name: options.name ?? null,
  });
}

async function streamCursorMessages(
  run: Run,
  onMessage?: (message: SDKMessage) => Promise<void> | void,
): Promise<void> {
  if (!onMessage || !run.supports("stream")) {
    return;
  }
  for await (const message of run.stream()) {
    await onMessage(message);
  }
}
