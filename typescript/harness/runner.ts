import readline from "node:readline";
import { pathToFileURL } from "node:url";

import {
  asBytes,
  toolResultEvent,
  type AddEventsRequest,
  type AddEventsResult,
  type Agent,
  type AgentConfig,
  type AgentRecord,
  type Artifact,
  type ArtifactVersion,
  type Binding,
  type BindingRecord,
  type Conversation,
  type ConversationConfig,
  type ConversationRecord,
  type Event,
  type EventData,
  type EventQuery,
  type ExoHarnessCurrent,
  type ExoHarness,
  type FileSystemMount,
  type ForkConversationRequest,
  type GetEventsResult,
  type JsonObject,
  type Message,
  type NewConversationRequest,
  type PendingToolCall,
  type SandboxProcess,
  type SandboxProcessStartRequest,
  type SendRequest,
  type Secret,
  type SecretMetadata,
  type ToolRequest,
  type ToolResult,
  type Turn,
  type TurnContext,
  type TurnRecord,
  type TypeScriptHarness,
} from "./index";

interface RawAgentConfig {
  instructions: Message[];
  harness: "basic" | "rlm" | "typescript" | "type_script" | "exo";
  typescript?: {
    module_path: string;
    tool_module_paths?: string[];
  } | null;
  enable_agent_tool_creation?: boolean;
  sandbox: {
    image?: string | null;
    provider: "daytona" | "apple_container" | "docker" | "local_process";
    mounts?: RawConversationConfig["mounts"] | null;
    enable_networking: boolean;
    scope: "agent" | "conversation";
  };
  model: string;
  max_output_tokens?: number | null;
  max_tool_round_trips?: number | null;
  braintrust?: unknown;
}

interface RawConversationConfig {
  sandbox_image?: string | null;
  sandbox_provider?:
    | "daytona"
    | "apple_container"
    | "docker"
    | "local_process"
    | null;
  shell_program?: string | null;
  sandbox_scope?: "agent" | "conversation" | null;
  mounts: Array<{
    host_path: string;
    mount_path: string;
    mode: "ro" | "rw";
    internal?: boolean | null;
  }>;
}

interface RawSendRequest {
  input: Message[];
  session_id?: string | null;
}

interface RawToolRequest {
  function_name: string;
  arguments: JsonObject;
}

interface RawAgentRecord {
  id: string;
  slug: string;
  name: string;
}

interface RawConversationRecord {
  id: string;
  slug: string;
  name: string;
  latest_event_id?: string | null;
}

interface RawTurnRecord {
  id: string;
  session_id: string;
}

interface RawArtifactVersion {
  artifact_id: string;
  path: string;
  version: number;
  created_at: string;
  size_bytes: number;
}

interface RawArtifact extends RawArtifactVersion {
  contents: number[];
}

type RawBinding =
  | {
      type: "env";
      name: string;
      env_var: string;
      secret_id: string;
    }
  | {
      type: "mcp";
      name: string;
      server_url: string;
      secret_id?: string | null;
    }
  | {
      type: "llm";
      name: string;
      model: string;
      provider?: string | null;
      base_url?: string | null;
      secret_id?: string | null;
    };

interface RawBindingRecord {
  id: string;
  type: "env" | "mcp" | "llm";
  name: string;
  created_at: string;
  binding: RawBinding;
}

type RawSecret =
  | {
      type: "key";
      value: string;
    }
  | {
      type: "oauth";
      provider?: string | null;
      account_id?: string | null;
      access_token?: string | null;
      refresh_token?: string | null;
      expires_at?: string | null;
    };

interface RawLogoutOauthResult {
  was_logged_in: boolean;
  remote_revocation_confirmed: boolean;
}

interface RawSecretMetadata {
  id: string;
  type: "key" | "oauth";
  name: string;
  created_at: string;
}

interface RawConversationHandleInfo {
  agent_id: string;
  record: RawConversationRecord;
}

interface RawTurnHandleInfo {
  conversation: RawConversationHandleInfo;
  record: RawTurnRecord;
}

interface RawGetEventsResult {
  events: RawEvent[];
  cursor?: string | null;
}

interface RawAddEventsResult {
  event_ids: string[];
  latest_event_id: string;
}

interface RawEvent {
  id: string;
  conversation_id: string;
  session_id?: string | null;
  turn_id?: string | null;
  created_at: string;
  data: EventData;
}

interface RawTypeScriptInitPayload {
  agent: RawAgentRecord;
  conversation: RawConversationHandleInfo;
  turn: RawTurnHandleInfo;
  agent_config: RawAgentConfig;
  conversation_config: RawConversationConfig;
  request: RawSendRequest;
  streaming: boolean;
  braintrust_parent?: string | null;
}

type RawRuntimeRequest =
  | { type: "execute_tool"; request: RawToolRequest }
  | {
      type: "start_sandbox_process";
      command: string[];
      env: Record<string, string>;
      reuse_key?: string | null;
    }
  | { type: "write_sandbox_process_stdin"; process_id: number; data: string }
  | { type: "close_sandbox_process_stdin"; process_id: number }
  | { type: "close_sandbox_process"; process_id: number };

type RawRuntimeResponsePayload =
  | { type: "tool_result"; result: ToolResult }
  | {
      type: "sandbox_process_started";
      process_id: number;
      sandbox_id?: string | null;
      sandbox_process_id?: string | null;
      reused?: boolean | null;
    }
  | { type: "unit" };

type RawSandboxProcessStream = "stdout" | "stderr";

type RawRuntimeEvent =
  | {
      type: "sandbox_process_output";
      process_id: number;
      stream: RawSandboxProcessStream;
      data: string;
    }
  | {
      type: "sandbox_process_exit";
      process_id: number;
      exit_code?: number | null;
    }
  | {
      type: "sandbox_process_error";
      process_id: number;
      message: string;
    };

type RawExoRequest =
  | { type: "preflight_secret_storage" }
  | { type: "list_agents" }
  | { type: "get_agent"; agent_id: string }
  | { type: "new_agent"; request: { slug: string; name: string } }
  | { type: "delete_agent"; agent_id: string }
  | { type: "list_bindings" }
  | { type: "get_binding"; binding_id: string }
  | { type: "list_secrets" }
  | { type: "get_secret"; secret_id: string }
  | { type: "logout_oauth_secret"; secret_id: string }
  | { type: "list_conversations"; agent_id: string }
  | { type: "get_conversation"; agent_id: string; conversation_id: string }
  | {
      type: "new_conversation";
      agent_id: string;
      request: { slug?: string | null; name?: string | null };
    }
  | { type: "delete_conversation"; agent_id: string; conversation_id: string }
  | { type: "agent_list_artifacts"; agent_id: string }
  | {
      type: "agent_read_artifact";
      agent_id: string;
      request: { artifact_id: string; version?: number };
    }
  | {
      type: "agent_write_artifact";
      agent_id: string;
      request: { path: string; contents: number[] };
    }
  | { type: "agent_list_bindings"; agent_id: string }
  | { type: "agent_get_binding"; agent_id: string; binding_id: string }
  | { type: "agent_list_secrets"; agent_id: string }
  | { type: "agent_get_secret"; agent_id: string; secret_id: string }
  | {
      type: "conversation_start_session";
      agent_id: string;
      conversation_id: string;
    }
  | {
      type: "conversation_end_session";
      agent_id: string;
      conversation_id: string;
      session_id: string;
    }
  | {
      type: "conversation_get_events";
      agent_id: string;
      conversation_id: string;
      query?: {
        cursor?: string | null;
        direction?: "asc" | "desc" | null;
        limit?: number | null;
        session_id?: string | null;
        turn_id?: string | null;
        types?: string[] | null;
      } | null;
    }
  | {
      type: "conversation_get_event";
      agent_id: string;
      conversation_id: string;
      event_id: string;
    }
  | {
      type: "conversation_add_events";
      agent_id: string;
      conversation_id: string;
      request: {
        session_id?: string | null;
        turn_id?: string | null;
        data: EventData[];
      };
    }
  | {
      type: "conversation_fork";
      agent_id: string;
      conversation_id: string;
      request: {
        up_to_inclusive?: string | null;
        slug?: string | null;
        name?: string | null;
      };
    }
  | {
      type: "conversation_list_artifacts";
      agent_id: string;
      conversation_id: string;
    }
  | {
      type: "conversation_read_artifact";
      agent_id: string;
      conversation_id: string;
      request: { artifact_id: string; version?: number };
    }
  | {
      type: "conversation_write_artifact";
      agent_id: string;
      conversation_id: string;
      request: { path: string; contents: number[] };
    }
  | {
      type: "conversation_list_bindings";
      agent_id: string;
      conversation_id: string;
    }
  | {
      type: "conversation_get_binding";
      agent_id: string;
      conversation_id: string;
      binding_id: string;
    }
  | {
      type: "conversation_list_secrets";
      agent_id: string;
      conversation_id: string;
    }
  | {
      type: "conversation_get_secret";
      agent_id: string;
      conversation_id: string;
      secret_id: string;
    }
  | {
      type: "turn_add_events";
      agent_id: string;
      conversation_id: string;
      session_id: string;
      turn_id: string;
      data: EventData[];
    }
  | {
      type: "turn_write_artifact";
      agent_id: string;
      conversation_id: string;
      session_id: string;
      turn_id: string;
      request: { path: string; contents: number[] };
    }
  | {
      type: "turn_finish";
      agent_id: string;
      conversation_id: string;
      session_id: string;
      turn_id: string;
    };

type RawExoResponse =
  | { type: "agents"; agents: RawAgentRecord[] }
  | { type: "agent"; agent: RawAgentRecord | null }
  | { type: "bool"; value: boolean }
  | { type: "conversations"; conversations: RawConversationHandleInfo[] }
  | { type: "conversation"; conversation: RawConversationHandleInfo | null }
  | { type: "events"; result: RawGetEventsResult }
  | { type: "event"; event: RawEvent | null }
  | { type: "add_events"; result: RawAddEventsResult }
  | { type: "session_id"; session_id: string }
  | { type: "artifact_versions"; artifacts: RawArtifactVersion[] }
  | { type: "artifact"; artifact: RawArtifact | null }
  | { type: "artifact_version"; artifact: RawArtifactVersion }
  | { type: "bindings"; bindings: RawBindingRecord[] }
  | { type: "binding"; binding: RawBinding | null }
  | { type: "secrets"; secrets: RawSecretMetadata[] }
  | { type: "secret"; secret: RawSecret | null }
  | { type: "logout_oauth"; result: RawLogoutOauthResult }
  | { type: "turn"; turn: RawTurnHandleInfo }
  | { type: "event_id"; event_id: string }
  | { type: "unit" };

type HostToGuestMessage =
  | { kind: "init"; payload: RawTypeScriptInitPayload }
  | { kind: "shutdown" }
  | {
      kind: "runtime_response";
      id: number;
      ok: boolean;
      payload?: RawRuntimeResponsePayload | null;
      error?: string | null;
    }
  | {
      kind: "exo_response";
      id: number;
      ok: boolean;
      response?: RawExoResponse | null;
      error?: string | null;
    }
  | { kind: "runtime_event"; event: RawRuntimeEvent };

type GuestToHostMessage =
  | { kind: "runtime_request"; id: number; request: RawRuntimeRequest }
  | { kind: "exo_request"; id: number; request: RawExoRequest }
  | { kind: "stream_event"; event: RawTypeScriptStreamEvent }
  | { kind: "done" }
  | { kind: "error"; message: string; stack?: string | null };

type RawTypeScriptStreamEvent =
  | { type: "first_chunk"; ttft_ms: number }
  | { type: "text_delta"; text: string }
  | {
      type: "tool_call";
      tool_call_id: string;
      tool_name: string;
      arguments: JsonObject;
    }
  | { type: "tool_result"; tool_call_id: string; result: ToolResult };

class ProtocolClient {
  private nextRequestId = 1;
  private readonly pending = new Map<
    number,
    {
      resolve: (payload: unknown) => void;
      reject: (error: Error) => void;
    }
  >();
  private readonly initQueue: RawTypeScriptInitPayload[] = [];
  private readonly initWaiters: Array<{
    resolve: (payload: RawTypeScriptInitPayload | null) => void;
  }> = [];
  private readonly sandboxProcesses = new Map<number, SandboxProcessHandle>();
  private closed = false;

  constructor() {
    const rl = readline.createInterface({
      input: process.stdin,
      crlfDelay: Number.POSITIVE_INFINITY,
    });

    rl.on("line", (line) => {
      void this.handleLine(line).catch((error) => {
        void this.fail(error);
      });
    });

    rl.on("close", () => {
      const error = new Error(
        "typescript harness host pipe closed unexpectedly",
      );
      this.closeInitQueue();
      for (const pending of this.pending.values()) {
        pending.reject(error);
      }
      this.pending.clear();
    });
  }

  nextInit(): Promise<RawTypeScriptInitPayload | null> {
    const queued = this.initQueue.shift();
    if (queued) {
      return Promise.resolve(queued);
    }
    if (this.closed) {
      return Promise.resolve(null);
    }
    return new Promise<RawTypeScriptInitPayload | null>((resolve) => {
      this.initWaiters.push({ resolve });
    });
  }

  async requestRuntime(
    request: RawRuntimeRequest,
  ): Promise<RawRuntimeResponsePayload> {
    const id = this.nextRequestId;
    this.nextRequestId += 1;
    const response = new Promise<RawRuntimeResponsePayload>(
      (resolve, reject) => {
        this.pending.set(id, {
          resolve: (payload: unknown) =>
            resolve(payload as RawRuntimeResponsePayload),
          reject,
        });
      },
    );
    await this.send({
      kind: "runtime_request",
      id,
      request,
    });
    return response;
  }

  async requestExo(request: RawExoRequest): Promise<RawExoResponse> {
    const id = this.nextRequestId;
    this.nextRequestId += 1;
    const response = new Promise<RawExoResponse>((resolve, reject) => {
      this.pending.set(id, {
        resolve: (payload: unknown) => resolve(payload as RawExoResponse),
        reject,
      });
    });
    await this.send({
      kind: "exo_request",
      id,
      request,
    });
    try {
      return await response;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      throw new Error(`exoharness request ${request.type} failed: ${message}`);
    }
  }

  async emitStream(event: RawTypeScriptStreamEvent): Promise<void> {
    await this.send({
      kind: "stream_event",
      event,
    });
  }

  async startSandboxProcess(
    request: SandboxProcessStartRequest,
  ): Promise<SandboxProcess> {
    const payload = await this.requestRuntime({
      type: "start_sandbox_process",
      command: request.command,
      env: request.env ?? {},
      reuse_key: request.reuseKey ?? null,
    });
    if (payload.type !== "sandbox_process_started") {
      throw new Error(
        `expected sandbox_process_started payload, got ${payload.type}`,
      );
    }
    const process = new SandboxProcessHandle(
      this,
      payload.process_id,
      payload.sandbox_id ?? undefined,
      payload.sandbox_process_id ?? undefined,
      payload.reused === true,
    );
    this.sandboxProcesses.set(payload.process_id, process);
    return process;
  }

  async writeSandboxProcessStdin(
    processId: number,
    data: string,
  ): Promise<void> {
    const payload = await this.requestRuntime({
      type: "write_sandbox_process_stdin",
      process_id: processId,
      data,
    });
    if (payload.type !== "unit") {
      throw new Error(`expected unit payload, got ${payload.type}`);
    }
  }

  async closeSandboxProcessStdin(processId: number): Promise<void> {
    const payload = await this.requestRuntime({
      type: "close_sandbox_process_stdin",
      process_id: processId,
    });
    if (payload.type !== "unit") {
      throw new Error(`expected unit payload, got ${payload.type}`);
    }
  }

  async closeSandboxProcess(processId: number): Promise<void> {
    const payload = await this.requestRuntime({
      type: "close_sandbox_process",
      process_id: processId,
    });
    if (payload.type !== "unit") {
      throw new Error(`expected unit payload, got ${payload.type}`);
    }
  }

  async done(): Promise<void> {
    await this.send({ kind: "done" });
  }

  async fail(error: unknown): Promise<void> {
    const message = error instanceof Error ? error.message : String(error);
    const stack = error instanceof Error ? (error.stack ?? null) : null;
    await this.send({
      kind: "error",
      message,
      stack,
    });
  }

  private async handleLine(line: string): Promise<void> {
    const message = JSON.parse(line) as HostToGuestMessage;
    switch (message.kind) {
      case "init":
        this.enqueueInit(message.payload);
        return;
      case "shutdown":
        this.closeInitQueue();
        return;
      case "runtime_response": {
        const pending = this.pending.get(message.id);
        if (!pending) {
          throw new Error(`unexpected runtime response id ${message.id}`);
        }
        this.pending.delete(message.id);
        if (!message.ok) {
          pending.reject(
            new Error(
              message.error ?? "typescript harness runtime request failed",
            ),
          );
          return;
        }
        if (!message.payload) {
          pending.reject(
            new Error(
              `missing runtime response payload for request ${message.id}`,
            ),
          );
          return;
        }
        pending.resolve(message.payload);
        return;
      }
      case "exo_response": {
        const pending = this.pending.get(message.id);
        if (!pending) {
          throw new Error(`unexpected exoharness response id ${message.id}`);
        }
        this.pending.delete(message.id);
        if (!message.ok) {
          pending.reject(
            new Error(message.error ?? "exoharness request failed"),
          );
          return;
        }
        if (!message.response) {
          pending.reject(
            new Error(
              `missing exoharness response payload for request ${message.id}`,
            ),
          );
          return;
        }
        pending.resolve(message.response);
        return;
      }
      case "runtime_event":
        this.handleRuntimeEvent(message.event);
        return;
    }
  }

  private async send(message: GuestToHostMessage): Promise<void> {
    process.stdout.write(`${JSON.stringify(message)}\n`);
  }

  private enqueueInit(payload: RawTypeScriptInitPayload): void {
    const waiter = this.initWaiters.shift();
    if (waiter) {
      waiter.resolve(payload);
      return;
    }
    this.initQueue.push(payload);
  }

  private closeInitQueue(): void {
    this.closed = true;
    while (this.initWaiters.length > 0) {
      const waiter = this.initWaiters.shift();
      waiter?.resolve(null);
    }
  }

  private handleRuntimeEvent(event: RawRuntimeEvent): void {
    const process = this.sandboxProcesses.get(event.process_id);
    if (!process) {
      return;
    }
    process.handleEvent(event);
    if (
      event.type === "sandbox_process_exit" ||
      event.type === "sandbox_process_error"
    ) {
      this.sandboxProcesses.delete(event.process_id);
    }
  }
}

class SandboxProcessHandle implements SandboxProcess {
  readonly reused: boolean;
  readonly stdout: ReadableStream<string>;
  readonly stderr: ReadableStream<string>;
  private stdoutController: ReadableStreamDefaultController<string> | null =
    null;
  private stderrController: ReadableStreamDefaultController<string> | null =
    null;
  private finished = false;
  private readonly waitPromise: Promise<number | null>;
  private resolveWait!: (exitCode: number | null) => void;
  private rejectWait!: (error: Error) => void;

  constructor(
    private readonly client: ProtocolClient,
    private readonly processId: number,
    readonly sandboxId?: string,
    readonly sandboxProcessId?: string,
    reused = false,
  ) {
    this.reused = reused;
    this.stdout = new ReadableStream<string>({
      start: (controller) => {
        this.stdoutController = controller;
      },
    });
    this.stderr = new ReadableStream<string>({
      start: (controller) => {
        this.stderrController = controller;
      },
    });
    this.waitPromise = new Promise<number | null>((resolve, reject) => {
      this.resolveWait = resolve;
      this.rejectWait = reject;
    });
  }

  async writeStdin(data: string): Promise<void> {
    await this.client.writeSandboxProcessStdin(this.processId, data);
  }

  async closeStdin(): Promise<void> {
    await this.client.closeSandboxProcessStdin(this.processId);
  }

  async close(): Promise<void> {
    if (this.finished) {
      return;
    }
    await this.client.closeSandboxProcess(this.processId);
  }

  wait(): Promise<number | null> {
    return this.waitPromise;
  }

  handleEvent(event: RawRuntimeEvent): void {
    switch (event.type) {
      case "sandbox_process_output":
        this.enqueue(event.stream, event.data);
        return;
      case "sandbox_process_exit":
        this.finish(event.exit_code ?? null);
        return;
      case "sandbox_process_error":
        this.fail(new Error(event.message));
        return;
    }
  }

  private enqueue(stream: RawSandboxProcessStream, data: string): void {
    const controller =
      stream === "stdout" ? this.stdoutController : this.stderrController;
    controller?.enqueue(data);
  }

  private finish(exitCode: number | null): void {
    if (this.finished) {
      return;
    }
    this.finished = true;
    this.stdoutController?.close();
    this.stderrController?.close();
    this.resolveWait(exitCode);
  }

  private fail(error: Error): void {
    if (this.finished) {
      return;
    }
    this.finished = true;
    this.stdoutController?.error(error);
    this.stderrController?.error(error);
    this.rejectWait(error);
  }
}

function toAgentConfig(raw: RawAgentConfig): AgentConfig {
  return {
    instructions: raw.instructions,
    harness: raw.harness === "type_script" ? "typescript" : raw.harness,
    typescript: raw.typescript
      ? {
          modulePath: raw.typescript.module_path,
          toolModulePaths: raw.typescript.tool_module_paths ?? [],
        }
      : null,
    enableAgentToolCreation: raw.enable_agent_tool_creation ?? true,
    sandbox: {
      image: raw.sandbox.image ?? null,
      provider: raw.sandbox.provider,
      mounts: (raw.sandbox.mounts ?? []).map(toFileSystemMount),
      enableNetworking: raw.sandbox.enable_networking,
      scope: raw.sandbox.scope,
    },
    model: raw.model,
    maxOutputTokens: raw.max_output_tokens ?? null,
    maxToolRoundTrips: raw.max_tool_round_trips ?? null,
    braintrust: raw.braintrust,
  };
}

function toConversationConfig(raw: RawConversationConfig): ConversationConfig {
  return {
    sandboxImage: raw.sandbox_image ?? null,
    sandboxProvider: raw.sandbox_provider ?? null,
    shellProgram: raw.shell_program ?? null,
    sandboxScope: raw.sandbox_scope ?? null,
    mounts: raw.mounts.map(toFileSystemMount),
  };
}

function toFileSystemMount(
  raw: RawConversationConfig["mounts"][number],
): FileSystemMount {
  return {
    hostPath: raw.host_path,
    mountPath: raw.mount_path,
    mode: raw.mode,
    internal: raw.internal ?? null,
  };
}

function toSendRequest(raw: RawSendRequest): SendRequest {
  return {
    input: raw.input,
    sessionId: raw.session_id ?? null,
  };
}

function toRawToolRequest(request: ToolRequest): RawToolRequest {
  return {
    function_name: request.functionName,
    arguments: request.arguments,
  };
}

function toAgentRecord(raw: RawAgentRecord): AgentRecord {
  return {
    id: raw.id,
    slug: raw.slug,
    name: raw.name,
  };
}

function toConversationRecord(raw: RawConversationRecord): ConversationRecord {
  return {
    id: raw.id,
    slug: raw.slug,
    name: raw.name,
    latestEventId: raw.latest_event_id ?? null,
  };
}

function toTurnRecord(raw: RawTurnRecord): TurnRecord {
  return {
    id: raw.id,
    sessionId: raw.session_id,
  };
}

function toArtifactVersion(raw: RawArtifactVersion): ArtifactVersion {
  return {
    artifactId: raw.artifact_id,
    path: raw.path,
    version: raw.version,
    createdAt: raw.created_at,
    sizeBytes: raw.size_bytes,
  };
}

function toArtifact(raw: RawArtifact): Artifact {
  return {
    ...toArtifactVersion(raw),
    contents: Uint8Array.from(raw.contents),
  };
}

function toBindingRecord(raw: RawBindingRecord): BindingRecord {
  return {
    id: raw.id,
    type: raw.type,
    name: raw.name,
    createdAt: raw.created_at,
    binding: toBinding(raw.binding),
  };
}

function toBinding(raw: RawBinding): Binding {
  if (raw.type === "env") {
    return {
      type: "env",
      name: raw.name,
      envVar: raw.env_var,
      secretId: raw.secret_id,
    };
  }
  if (raw.type === "mcp") {
    return {
      type: "mcp",
      name: raw.name,
      serverUrl: raw.server_url,
      secretId: raw.secret_id ?? null,
    };
  }
  return {
    type: "llm",
    name: raw.name,
    model: raw.model,
    provider: raw.provider ?? null,
    baseUrl: raw.base_url ?? null,
    secretId: raw.secret_id ?? null,
  };
}

function toSecretMetadata(raw: RawSecretMetadata): SecretMetadata {
  return {
    id: raw.id,
    type: raw.type,
    name: raw.name,
    createdAt: raw.created_at,
  };
}

function toSecret(raw: RawSecret): Secret {
  if (raw.type === "key") {
    return {
      type: "key",
      value: raw.value,
    };
  }
  return {
    type: "oauth",
    provider: raw.provider ?? null,
    accountId: raw.account_id ?? null,
    accessToken: raw.access_token ?? null,
    refreshToken: raw.refresh_token ?? null,
    expiresAt: raw.expires_at ?? null,
  };
}

function toLogoutOauthResult(
  raw: RawLogoutOauthResult,
): import("./index.js").LogoutOauthResult {
  return {
    wasLoggedIn: raw.was_logged_in,
    remoteRevocationConfirmed: raw.remote_revocation_confirmed,
  };
}

function decodeArtifactText(artifact: Artifact | null): string | null {
  if (!artifact) {
    return null;
  }
  return new TextDecoder().decode(artifact.contents);
}

function decodeArtifactJson<T>(artifact: Artifact | null): T | null {
  const text = decodeArtifactText(artifact);
  if (text === null) {
    return null;
  }
  return JSON.parse(text) as T;
}

function toEvent(raw: RawEvent): Event {
  return {
    id: raw.id,
    conversationId: raw.conversation_id,
    sessionId: raw.session_id ?? null,
    turnId: raw.turn_id ?? null,
    createdAt: raw.created_at,
    data: raw.data,
  };
}

function toGetEventsResult(raw: RawGetEventsResult): GetEventsResult {
  return {
    events: raw.events.map(toEvent),
    cursor: raw.cursor ?? null,
  };
}

function toAddEventsResult(raw: RawAddEventsResult): AddEventsResult {
  return {
    eventIds: raw.event_ids,
    latestEventId: raw.latest_event_id,
  };
}

type RawEventQuery = {
  cursor?: string | null;
  direction?: "asc" | "desc" | null;
  limit?: number | null;
  session_id?: string | null;
  turn_id?: string | null;
  types?: string[] | null;
};

function toRawEventQuery(query?: EventQuery): RawEventQuery | null {
  if (!query) {
    return null;
  }
  return {
    cursor: query.cursor ?? null,
    direction: query.direction ?? null,
    limit: query.limit ?? null,
    session_id: query.sessionId ?? null,
    turn_id: query.turnId ?? null,
    types: query.types ?? null,
  };
}

function toRawAddEventsRequest(request: AddEventsRequest): {
  session_id?: string | null;
  turn_id?: string | null;
  data: EventData[];
} {
  return {
    session_id: request.sessionId ?? null,
    turn_id: request.turnId ?? null,
    data: request.data,
  };
}

function toRawNewConversationRequest(request?: NewConversationRequest): {
  slug?: string | null;
  name?: string | null;
} {
  return {
    slug: request?.slug ?? null,
    name: request?.name ?? null,
  };
}

function toRawForkConversationRequest(request?: ForkConversationRequest): {
  up_to_inclusive?: string | null;
  slug?: string | null;
  name?: string | null;
} {
  return {
    up_to_inclusive: request?.upToInclusive ?? null,
    slug: request?.slug ?? null,
    name: request?.name ?? null,
  };
}

function createAgent(client: ProtocolClient, raw: RawAgentRecord): Agent {
  const record = toAgentRecord(raw);
  const agent: Agent = {
    record,

    async listConversations(): Promise<Conversation[]> {
      const payload = await client.requestExo({
        type: "list_conversations",
        agent_id: record.id,
      });
      if (payload.type !== "conversations") {
        throw new Error(`expected conversations payload, got ${payload.type}`);
      }
      return payload.conversations.map((conversation) =>
        createConversation(client, conversation),
      );
    },

    async getConversation(id: string): Promise<Conversation | null> {
      const payload = await client.requestExo({
        type: "get_conversation",
        agent_id: record.id,
        conversation_id: id,
      });
      if (payload.type !== "conversation") {
        throw new Error(`expected conversation payload, got ${payload.type}`);
      }
      return payload.conversation
        ? createConversation(client, payload.conversation)
        : null;
    },

    async newConversation(
      request?: NewConversationRequest,
    ): Promise<Conversation> {
      const payload = await client.requestExo({
        type: "new_conversation",
        agent_id: record.id,
        request: toRawNewConversationRequest(request),
      });
      if (payload.type !== "conversation" || !payload.conversation) {
        throw new Error(`expected conversation payload, got ${payload.type}`);
      }
      return createConversation(client, payload.conversation);
    },

    async deleteConversation(id: string): Promise<boolean> {
      const payload = await client.requestExo({
        type: "delete_conversation",
        agent_id: record.id,
        conversation_id: id,
      });
      if (payload.type !== "bool") {
        throw new Error(`expected bool payload, got ${payload.type}`);
      }
      return payload.value;
    },

    async listArtifacts(): Promise<ArtifactVersion[]> {
      const payload = await client.requestExo({
        type: "agent_list_artifacts",
        agent_id: record.id,
      });
      if (payload.type !== "artifact_versions") {
        throw new Error(
          `expected artifact_versions payload, got ${payload.type}`,
        );
      }
      return payload.artifacts.map(toArtifactVersion);
    },

    async readArtifact(args): Promise<Artifact | null> {
      const payload = await client.requestExo({
        type: "agent_read_artifact",
        agent_id: record.id,
        request: {
          artifact_id: args.artifactId,
          version: args.version,
        },
      });
      if (payload.type !== "artifact") {
        throw new Error(`expected artifact payload, got ${payload.type}`);
      }
      return payload.artifact ? toArtifact(payload.artifact) : null;
    },

    async readArtifactText(args): Promise<string | null> {
      return decodeArtifactText(await agent.readArtifact(args));
    },

    async readArtifactJson<T>(args: {
      artifactId: string;
      version?: number;
    }): Promise<T | null> {
      return decodeArtifactJson<T>(await agent.readArtifact(args));
    },

    async writeArtifact(args): Promise<ArtifactVersion> {
      const payload = await client.requestExo({
        type: "agent_write_artifact",
        agent_id: record.id,
        request: {
          path: args.path,
          contents: Array.from(asBytes(args.contents)),
        },
      });
      if (payload.type !== "artifact_version") {
        throw new Error(
          `expected artifact_version payload, got ${payload.type}`,
        );
      }
      return toArtifactVersion(payload.artifact);
    },

    async writeArtifactText(args): Promise<ArtifactVersion> {
      return agent.writeArtifact({
        path: args.path,
        contents: args.text,
      });
    },

    async writeArtifactJson(args): Promise<ArtifactVersion> {
      return agent.writeArtifact({
        path: args.path,
        contents: JSON.stringify(args.value, null, 2),
      });
    },

    async listBindings(): Promise<BindingRecord[]> {
      const payload = await client.requestExo({
        type: "agent_list_bindings",
        agent_id: record.id,
      });
      if (payload.type !== "bindings") {
        throw new Error(`expected bindings payload, got ${payload.type}`);
      }
      return payload.bindings.map(toBindingRecord);
    },

    async getBinding(id: string): Promise<Binding | null> {
      const payload = await client.requestExo({
        type: "agent_get_binding",
        agent_id: record.id,
        binding_id: id,
      });
      if (payload.type !== "binding") {
        throw new Error(`expected binding payload, got ${payload.type}`);
      }
      return payload.binding ? toBinding(payload.binding) : null;
    },

    async listSecrets(): Promise<SecretMetadata[]> {
      const payload = await client.requestExo({
        type: "agent_list_secrets",
        agent_id: record.id,
      });
      if (payload.type !== "secrets") {
        throw new Error(`expected secrets payload, got ${payload.type}`);
      }
      return payload.secrets.map(toSecretMetadata);
    },

    async getSecret(id: string): Promise<Secret | null> {
      const payload = await client.requestExo({
        type: "agent_get_secret",
        agent_id: record.id,
        secret_id: id,
      });
      if (payload.type !== "secret") {
        throw new Error(`expected secret payload, got ${payload.type}`);
      }
      return payload.secret ? toSecret(payload.secret) : null;
    },
  };
  return agent;
}

function createExoHarness(
  client: ProtocolClient,
  current: ExoHarnessCurrent,
): ExoHarness {
  return {
    current,

    async preflightSecretStorage(): Promise<void> {
      const payload = await client.requestExo({
        type: "preflight_secret_storage",
      });
      if (payload.type !== "unit") {
        throw new Error(`expected unit payload, got ${payload.type}`);
      }
    },

    async listAgents(): Promise<Agent[]> {
      const payload = await client.requestExo({ type: "list_agents" });
      if (payload.type !== "agents") {
        throw new Error(`expected agents payload, got ${payload.type}`);
      }
      return payload.agents.map((agent) => createAgent(client, agent));
    },

    async getAgent(id: string): Promise<Agent | null> {
      const payload = await client.requestExo({
        type: "get_agent",
        agent_id: id,
      });
      if (payload.type !== "agent") {
        throw new Error(`expected agent payload, got ${payload.type}`);
      }
      return payload.agent ? createAgent(client, payload.agent) : null;
    },

    async newAgent(request): Promise<Agent> {
      const payload = await client.requestExo({
        type: "new_agent",
        request,
      });
      if (payload.type !== "agent" || !payload.agent) {
        throw new Error(`expected agent payload, got ${payload.type}`);
      }
      return createAgent(client, payload.agent);
    },

    async deleteAgent(id: string): Promise<boolean> {
      const payload = await client.requestExo({
        type: "delete_agent",
        agent_id: id,
      });
      if (payload.type !== "bool") {
        throw new Error(`expected bool payload, got ${payload.type}`);
      }
      return payload.value;
    },

    async listBindings(): Promise<BindingRecord[]> {
      const payload = await client.requestExo({ type: "list_bindings" });
      if (payload.type !== "bindings") {
        throw new Error(`expected bindings payload, got ${payload.type}`);
      }
      return payload.bindings.map(toBindingRecord);
    },

    async getBinding(id: string): Promise<Binding | null> {
      const payload = await client.requestExo({
        type: "get_binding",
        binding_id: id,
      });
      if (payload.type !== "binding") {
        throw new Error(`expected binding payload, got ${payload.type}`);
      }
      return payload.binding ? toBinding(payload.binding) : null;
    },

    async listSecrets(): Promise<SecretMetadata[]> {
      const payload = await client.requestExo({ type: "list_secrets" });
      if (payload.type !== "secrets") {
        throw new Error(`expected secrets payload, got ${payload.type}`);
      }
      return payload.secrets.map(toSecretMetadata);
    },

    async getSecret(id: string): Promise<Secret | null> {
      const payload = await client.requestExo({
        type: "get_secret",
        secret_id: id,
      });
      if (payload.type !== "secret") {
        throw new Error(`expected secret payload, got ${payload.type}`);
      }
      return payload.secret ? toSecret(payload.secret) : null;
    },

    async logoutOauthSecret(id) {
      const payload = await client.requestExo({
        type: "logout_oauth_secret",
        secret_id: id,
      });
      if (payload.type !== "logout_oauth") {
        throw new Error(`expected logout_oauth payload, got ${payload.type}`);
      }
      return toLogoutOauthResult(payload.result);
    },
  };
}

function createConversation(
  client: ProtocolClient,
  raw: RawConversationHandleInfo,
): Conversation {
  const record = toConversationRecord(raw.record);
  const conversation: Conversation = {
    agentId: raw.agent_id,
    record,

    async startSession(): Promise<string> {
      const payload = await client.requestExo({
        type: "conversation_start_session",
        agent_id: raw.agent_id,
        conversation_id: record.id,
      });
      if (payload.type !== "session_id") {
        throw new Error(`expected session_id payload, got ${payload.type}`);
      }
      return payload.session_id;
    },

    async endSession(id: string): Promise<void> {
      const payload = await client.requestExo({
        type: "conversation_end_session",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        session_id: id,
      });
      if (payload.type !== "unit") {
        throw new Error(`expected unit payload, got ${payload.type}`);
      }
    },

    async getEvents(query?: EventQuery): Promise<GetEventsResult> {
      const payload = await client.requestExo({
        type: "conversation_get_events",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        query: toRawEventQuery(query),
      });
      if (payload.type !== "events") {
        throw new Error(`expected events payload, got ${payload.type}`);
      }
      return toGetEventsResult(payload.result);
    },

    async getEvent(id: string): Promise<Event | null> {
      const payload = await client.requestExo({
        type: "conversation_get_event",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        event_id: id,
      });
      if (payload.type !== "event") {
        throw new Error(`expected event payload, got ${payload.type}`);
      }
      return payload.event ? toEvent(payload.event) : null;
    },

    async addEvents(request: AddEventsRequest): Promise<AddEventsResult> {
      const payload = await client.requestExo({
        type: "conversation_add_events",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        request: toRawAddEventsRequest(request),
      });
      if (payload.type !== "add_events") {
        throw new Error(`expected add_events payload, got ${payload.type}`);
      }
      return toAddEventsResult(payload.result);
    },

    async fork(request?: ForkConversationRequest): Promise<Conversation> {
      const payload = await client.requestExo({
        type: "conversation_fork",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        request: toRawForkConversationRequest(request),
      });
      if (payload.type !== "conversation" || !payload.conversation) {
        throw new Error(`expected conversation payload, got ${payload.type}`);
      }
      return createConversation(client, payload.conversation);
    },

    async listArtifacts(): Promise<ArtifactVersion[]> {
      const payload = await client.requestExo({
        type: "conversation_list_artifacts",
        agent_id: raw.agent_id,
        conversation_id: record.id,
      });
      if (payload.type !== "artifact_versions") {
        throw new Error(
          `expected artifact_versions payload, got ${payload.type}`,
        );
      }
      return payload.artifacts.map(toArtifactVersion);
    },

    async readArtifact(args): Promise<Artifact | null> {
      const payload = await client.requestExo({
        type: "conversation_read_artifact",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        request: {
          artifact_id: args.artifactId,
          version: args.version,
        },
      });
      if (payload.type !== "artifact") {
        throw new Error(`expected artifact payload, got ${payload.type}`);
      }
      return payload.artifact ? toArtifact(payload.artifact) : null;
    },

    async readArtifactText(args): Promise<string | null> {
      return decodeArtifactText(await conversation.readArtifact(args));
    },

    async readArtifactJson<T>(args: {
      artifactId: string;
      version?: number;
    }): Promise<T | null> {
      return decodeArtifactJson<T>(await conversation.readArtifact(args));
    },

    async writeArtifact(args): Promise<ArtifactVersion> {
      const payload = await client.requestExo({
        type: "conversation_write_artifact",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        request: {
          path: args.path,
          contents: Array.from(asBytes(args.contents)),
        },
      });
      if (payload.type !== "artifact_version") {
        throw new Error(
          `expected artifact_version payload, got ${payload.type}`,
        );
      }
      return toArtifactVersion(payload.artifact);
    },

    async writeArtifactText(args): Promise<ArtifactVersion> {
      return conversation.writeArtifact({
        path: args.path,
        contents: args.text,
      });
    },

    async writeArtifactJson(args): Promise<ArtifactVersion> {
      return conversation.writeArtifact({
        path: args.path,
        contents: JSON.stringify(args.value, null, 2),
      });
    },

    async listBindings(): Promise<BindingRecord[]> {
      const payload = await client.requestExo({
        type: "conversation_list_bindings",
        agent_id: raw.agent_id,
        conversation_id: record.id,
      });
      if (payload.type !== "bindings") {
        throw new Error(`expected bindings payload, got ${payload.type}`);
      }
      return payload.bindings.map(toBindingRecord);
    },

    async getBinding(id: string): Promise<Binding | null> {
      const payload = await client.requestExo({
        type: "conversation_get_binding",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        binding_id: id,
      });
      if (payload.type !== "binding") {
        throw new Error(`expected binding payload, got ${payload.type}`);
      }
      return payload.binding ? toBinding(payload.binding) : null;
    },

    async listSecrets(): Promise<SecretMetadata[]> {
      const payload = await client.requestExo({
        type: "conversation_list_secrets",
        agent_id: raw.agent_id,
        conversation_id: record.id,
      });
      if (payload.type !== "secrets") {
        throw new Error(`expected secrets payload, got ${payload.type}`);
      }
      return payload.secrets.map(toSecretMetadata);
    },

    async getSecret(id: string): Promise<Secret | null> {
      const payload = await client.requestExo({
        type: "conversation_get_secret",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        secret_id: id,
      });
      if (payload.type !== "secret") {
        throw new Error(`expected secret payload, got ${payload.type}`);
      }
      return payload.secret ? toSecret(payload.secret) : null;
    },
  };
  return conversation;
}

function createTurn(
  client: ProtocolClient,
  raw: RawTurnHandleInfo,
  conversation: Conversation,
): Turn {
  const record = toTurnRecord(raw.record);
  const turn: Turn = {
    agentId: raw.conversation.agent_id,
    conversationId: raw.conversation.record.id,
    sessionId: record.sessionId,
    turnId: record.id,
    conversation,
    record,

    async addEvents(data): Promise<AddEventsResult> {
      const payload = await client.requestExo({
        type: "turn_add_events",
        agent_id: raw.conversation.agent_id,
        conversation_id: raw.conversation.record.id,
        session_id: record.sessionId,
        turn_id: record.id,
        data,
      });
      if (payload.type !== "add_events") {
        throw new Error(`expected add_events payload, got ${payload.type}`);
      }
      return toAddEventsResult(payload.result);
    },

    async writeArtifact(args): Promise<ArtifactVersion> {
      const payload = await client.requestExo({
        type: "turn_write_artifact",
        agent_id: raw.conversation.agent_id,
        conversation_id: raw.conversation.record.id,
        session_id: record.sessionId,
        turn_id: record.id,
        request: {
          path: args.path,
          contents: Array.from(asBytes(args.contents)),
        },
      });
      if (payload.type !== "artifact_version") {
        throw new Error(
          `expected artifact_version payload, got ${payload.type}`,
        );
      }
      return toArtifactVersion(payload.artifact);
    },

    async writeArtifactText(args): Promise<ArtifactVersion> {
      return turn.writeArtifact({
        path: args.path,
        contents: args.text,
      });
    },

    async writeArtifactJson(args): Promise<ArtifactVersion> {
      return turn.writeArtifact({
        path: args.path,
        contents: JSON.stringify(args.value, null, 2),
      });
    },
  };
  return turn;
}

function createTurnContext(
  client: ProtocolClient,
  init: RawTypeScriptInitPayload,
): TurnContext {
  const agentConfig = toAgentConfig(init.agent_config);
  const conversationConfig = toConversationConfig(init.conversation_config);
  const request = toSendRequest(init.request);
  const streaming = init.streaming;
  const agent = createAgent(client, init.agent);
  const conversation = createConversation(client, init.conversation);
  const turn = createTurn(client, init.turn, conversation);
  const exoharness = createExoHarness(client, {
    agent,
    conversation,
    turn,
  });

  const context: TurnContext = {
    agentConfig,
    conversationConfig,
    request,
    streaming,
    braintrustParent: init.braintrust_parent ?? null,
    exoharness,
    async executeTool(request): Promise<ToolResult> {
      const payload = await client.requestRuntime({
        type: "execute_tool",
        request: toRawToolRequest(request),
      });
      if (payload.type !== "tool_result") {
        throw new Error(`expected tool_result payload, got ${payload.type}`);
      }
      return payload.result;
    },

    async startSandboxProcess(request): Promise<SandboxProcess> {
      return client.startSandboxProcess(request);
    },

    async executePendingTools(
      toolCalls: PendingToolCall[],
    ): Promise<EventData[]> {
      const events: EventData[] = [];
      for (const toolCall of toolCalls) {
        if (streaming) {
          await context.stream.toolCall({
            toolCallId: toolCall.toolCallId,
            toolName: toolCall.request.functionName,
            arguments: toolCall.request.arguments,
          });
        }
        let result: ToolResult;
        try {
          result = await context.executeTool(toolCall.request);
        } catch (error) {
          result = {
            ok: false,
            error: runnerErrorMessage(error),
          };
        }
        if (streaming) {
          await context.stream.toolResult({
            toolCallId: toolCall.toolCallId,
            result,
          });
        }
        events.push(toolResultEvent(toolCall.toolCallId, result));
      }
      return events;
    },

    stream: {
      async firstChunk(ttftMs): Promise<void> {
        if (!streaming) {
          return;
        }
        await client.emitStream({
          type: "first_chunk",
          ttft_ms: ttftMs,
        });
      },

      async text(text): Promise<void> {
        if (!streaming) {
          return;
        }
        await client.emitStream({
          type: "text_delta",
          text,
        });
      },

      async toolCall(args): Promise<void> {
        if (!streaming) {
          return;
        }
        await client.emitStream({
          type: "tool_call",
          tool_call_id: args.toolCallId,
          tool_name: args.toolName,
          arguments: args.arguments,
        });
      },

      async toolResult(args): Promise<void> {
        if (!streaming) {
          return;
        }
        await client.emitStream({
          type: "tool_result",
          tool_call_id: args.toolCallId,
          result: args.result,
        });
      },
    },
  };
  return context;
}

function resolveHarnessModule(
  moduleExports: Record<string, unknown>,
): TypeScriptHarness {
  const candidate = moduleExports.default ?? moduleExports.harness;
  if (!candidate || typeof candidate !== "object") {
    throw new Error(
      "typescript harness module must export a default harness or a named `harness` export",
    );
  }
  if (!("runTurn" in candidate) || typeof candidate.runTurn !== "function") {
    throw new Error(
      "typescript harness export must have an async runTurn(context) method",
    );
  }
  return candidate as TypeScriptHarness;
}

function runnerErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function main(): Promise<void> {
  const client = new ProtocolClient();
  const modulePath = process.argv[2];
  if (!modulePath) {
    throw new Error("missing harness module path");
  }
  let harness: TypeScriptHarness;
  try {
    const moduleExports = (await import(
      pathToFileURL(modulePath).href
    )) as Record<string, unknown>;
    harness = resolveHarnessModule(moduleExports);
  } catch (error) {
    await client.fail(error);
    throw error;
  }

  for (;;) {
    const init = await client.nextInit();
    if (!init) {
      return;
    }
    const context = createTurnContext(client, init);
    try {
      await harness.runTurn(context);
      await client.done();
    } catch (error) {
      await client.fail(error);
      throw error;
    }
  }
}

void main().catch(() => {
  process.exitCode = 1;
});
