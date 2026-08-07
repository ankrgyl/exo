import crypto from "node:crypto";
import http, { type IncomingMessage, type ServerResponse } from "node:http";
import readline from "node:readline/promises";

import {
  adapterConfig,
  isRecord,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";

const MAX_REQUEST_BYTES = 1024 * 1024;
const SEND_TIMEOUT_MS = 60_000;
const PROGRESS_START_TIMEOUT_MS = 5_000;
const MAX_ACTIVE_STREAM_AGE_MS = 10 * 60_000;

const config = adapterConfig();
const botTokenEnv =
  optionalStringField(config, "botTokenEnv") ?? "EXO_SLACK_BOT_TOKEN";
const signingSecretEnv =
  optionalStringField(config, "signingSecretEnv") ?? "EXO_SLACK_SIGNING_SECRET";
const botToken = requiredEnv(botTokenEnv, "Slack bot token");
const signingSecret = requiredEnv(signingSecretEnv, "Slack signing secret");

const port = optionalPort(config.port);
const requestPath = optionalStringField(config, "path") ?? "/slack/events";
if (!requestPath.startsWith("/")) {
  throw new Error("Slack request path must start with /");
}
const defaultChannelId = optionalStringField(config, "defaultChannelId");
const trigger = optionalStringField(config, "trigger") ?? "mentions_only";
const allowedChannels = stringArrayOrNull(config.allowedChannels);
const allowBots = config.allowBots === true;
const threadReplies = config.threadReplies !== false;
const progressMode = slackProgressMode(config.progressMode);
if (trigger !== "all_messages" && trigger !== "mentions_only") {
  throw new Error("Slack trigger must be all_messages or mentions_only");
}

process.on("unhandledRejection", (reason) => {
  reportWorkerError(
    `Slack adapter unhandled rejection: ${
      reason instanceof Error ? reason.message : String(reason)
    }`,
  );
});

process.on("uncaughtException", (error) => {
  reportWorkerError(`Slack adapter uncaught exception: ${error.message}`);
  process.exit(1);
});

const auth = await slackAuthTest();
const botUserId = optionalApiString(auth.user_id);
const botId = optionalApiString(auth.bot_id);
const activeThreads = new Set<string>();
const activeProgress = new Map<string, SlackProgressState>();

const server = http.createServer((request, response) => {
  void handleRequest(request, response).catch((error) => {
    const message = error instanceof Error ? error.message : String(error);
    reportWorkerError(message);
    if (!response.headersSent) {
      sendJson(response, 500, { ok: false });
    } else {
      response.end();
    }
  });
});

await listen(server, port);
writeWorkerEvent({
  type: "connected",
  subject: botUserId,
  metadata: {
    team: optionalApiString(auth.team),
    teamId: optionalApiString(auth.team_id),
    user: optionalApiString(auth.user),
    userId: botUserId,
    botId,
    port,
    path: requestPath,
    progressMode,
  },
});

void readCommands().catch((error) => {
  const message = error instanceof Error ? error.message : String(error);
  reportWorkerError(`Slack adapter command stream error: ${message}`);
  process.exit(1);
});

process.on("SIGTERM", () => {
  writeWorkerEvent({ type: "disconnected", reason: "sigterm" });
  server.close(() => process.exit(0));
});

process.on("SIGINT", () => {
  writeWorkerEvent({ type: "disconnected", reason: "sigint" });
  server.close(() => process.exit(0));
});

async function handleRequest(
  request: IncomingMessage,
  response: ServerResponse,
): Promise<void> {
  const url = new URL(request.url ?? "/", "http://localhost");
  if (url.pathname !== requestPath) {
    sendJson(response, 404, { ok: false, error: "not_found" });
    return;
  }
  if (request.method !== "POST") {
    sendJson(response, 405, { ok: false, error: "method_not_allowed" });
    return;
  }

  const body = await readBody(request);
  if (!verifySlackSignature(request, body)) {
    sendJson(response, 401, { ok: false, error: "invalid_signature" });
    return;
  }

  const payload = parseJsonBody(body);
  if (payload.type === "url_verification") {
    const challenge = payload.challenge;
    if (typeof challenge !== "string") {
      sendJson(response, 400, { ok: false, error: "missing_challenge" });
      return;
    }
    sendJson(response, 200, { challenge });
    return;
  }

  if (payload.type !== "event_callback") {
    sendJson(response, 200, { ok: true });
    return;
  }

  const message = inboundMessageFromPayload(payload);
  if (message !== null) {
    sendJson(response, 200, { ok: true });
    void emitInboundMessage(message).catch((error) => {
      const errorMessage =
        error instanceof Error ? error.message : String(error);
      reportWorkerError(errorMessage);
    });
    return;
  }
  sendJson(response, 200, { ok: true });
}

async function emitInboundMessage(message: SlackInboundMessage): Promise<void> {
  await startSlackProgress(message);
  writeWorkerEvent({
    type: "message",
    target: message.target,
    sender: message.sender,
    text: message.text,
    message_id: message.messageId,
    metadata: message.metadata,
    attachments: [],
  });
}

async function readCommands(): Promise<void> {
  const input = readline.createInterface({
    input: process.stdin,
    crlfDelay: Number.POSITIVE_INFINITY,
  });

  input.on("error", (error) => {
    reportWorkerError(`Slack adapter command stream error: ${error.message}`);
  });

  for await (const line of input) {
    if (line.trim().length === 0) {
      continue;
    }
    let commandId: string | null = null;
    try {
      const command = parseWorkerCommand(JSON.parse(line));
      commandId = command.id;
      if (command.attachments.length > 0) {
        throw new Error("Slack adapter supports text-only messages for now");
      }
      const target = command.target ?? defaultChannelId;
      if (!target) {
        throw new Error(
          "Slack send_message requires a target or configured defaultChannelId",
        );
      }
      const destination = await slackDestination(target);
      writeWorkerEvent({
        type: "lifecycle",
        name: "send_starting",
        metadata: {
          target,
          channel: destination.channel,
          threadTs: destination.threadTs,
          dmUserId: destination.dmUserId,
          progressPending: activeProgress.has(target),
        },
      });
      let sendMode: "message" | "stream" | "update" = "message";
      let result: SlackPostMessageResult | null = null;
      try {
        const progressResult = await finishSlackProgress(target, command.text);
        if (progressResult !== null) {
          sendMode = progressResult.mode;
          result = progressResult.result;
        }
      } catch (error) {
        const progressError =
          error instanceof Error ? error.message : String(error);
        writeWorkerEvent({
          type: "lifecycle",
          name: "send_progress_fallback",
          metadata: {
            target,
            channel: destination.channel,
            threadTs: destination.threadTs,
            error: progressError,
          },
        });
      }
      if (result === null) {
        result = await postSlackMessage(destination, command.text);
      }
      if (destination.threadTs !== null) {
        activeThreads.add(
          slackThreadKey(destination.channel, destination.threadTs),
        );
      }
      writeWorkerEvent({
        type: "lifecycle",
        name: "send_result",
        metadata: {
          target,
          channel: destination.channel,
          threadTs: destination.threadTs,
          dmUserId: destination.dmUserId,
          sendMode,
          slackChannel: result.channel,
          slackTs: result.ts,
          slackMessageTs: result.messageTs,
          slackText: result.text,
          slackWarning: result.warning,
        },
      });
      writeWorkerEvent({ type: "command_ack", command_id: command.id });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      reportWorkerError(message);
      if (commandId !== null) {
        writeWorkerEvent({
          type: "command_nack",
          command_id: commandId,
          message,
        });
      }
    }
  }
}

type SlackAuthTestResponse = {
  ok: true;
  team?: unknown;
  team_id?: unknown;
  user?: unknown;
  user_id?: unknown;
  bot_id?: unknown;
};

async function slackAuthTest(): Promise<SlackAuthTestResponse> {
  const payload = await slackApiGet("auth.test");
  return {
    ok: true,
    team: payload.team,
    team_id: payload.team_id,
    user: payload.user,
    user_id: payload.user_id,
    bot_id: payload.bot_id,
  };
}

type SlackProgressState = {
  mode: "stream" | "update";
  target: string;
  channel: string;
  threadTs: string | null;
  messageTs: string;
  startedAtMs: number;
};

type SlackProgressRequest = {
  mode: "stream" | "update";
  channel: string;
  threadTs: string | null;
  recipientUserId: string | null;
  recipientTeamId: string | null;
  isDm: boolean;
};

type SlackProgressResult = {
  mode: "stream" | "update";
  result: SlackPostMessageResult;
};

async function startSlackProgress(message: SlackInboundMessage): Promise<void> {
  const progress = message.progress;
  if (progress === null) {
    return;
  }
  pruneExpiredSlackProgress();
  if (activeProgress.has(message.target)) {
    return;
  }
  if (progress.mode === "update") {
    await startSlackUpdateProgress(message, progress);
    return;
  }
  await startSlackStreamProgress(message, progress);
}

async function startSlackUpdateProgress(
  message: SlackInboundMessage,
  progress: SlackProgressRequest,
): Promise<void> {
  try {
    const result = await postSlackMessage(
      {
        channel: progress.channel,
        threadTs: progress.threadTs,
        dmUserId: null,
      },
      "Exo is working...",
      PROGRESS_START_TIMEOUT_MS,
      slackProgressBlocks(),
    );
    const messageTs = result.messageTs ?? result.ts;
    if (messageTs === null) {
      throw new Error("Slack chat.postMessage returned no ts");
    }
    const channel = result.channel ?? progress.channel;
    activeProgress.set(message.target, {
      mode: "update",
      target: message.target,
      channel,
      threadTs: progress.threadTs,
      messageTs,
      startedAtMs: Date.now(),
    });
    writeWorkerEvent({
      type: "lifecycle",
      name: "progress_update_started",
      metadata: {
        target: message.target,
        channel,
        threadTs: progress.threadTs,
        messageTs,
      },
    });
  } catch (error) {
    const messageText = error instanceof Error ? error.message : String(error);
    writeWorkerEvent({
      type: "lifecycle",
      name: "progress_update_failed",
      metadata: {
        target: message.target,
        channel: progress.channel,
        threadTs: progress.threadTs,
        error: messageText,
      },
    });
  }
}

async function startSlackStreamProgress(
  message: SlackInboundMessage,
  progress: SlackProgressRequest,
): Promise<void> {
  if (progress.threadTs === null) {
    return;
  }
  try {
    const body: Record<string, unknown> = {
      channel: progress.channel,
      thread_ts: progress.threadTs,
      task_display_mode: "plan",
      chunks: [
        {
          type: "plan_update",
          title: "Exo is working",
        },
        {
          type: "task_update",
          id: "exo_response",
          title: "Prepare response",
          status: "in_progress",
          details: "Reading the request",
        },
      ],
    };
    if (!progress.isDm) {
      if (progress.recipientUserId !== null) {
        body.recipient_user_id = progress.recipientUserId;
      }
      if (progress.recipientTeamId !== null) {
        body.recipient_team_id = progress.recipientTeamId;
      }
    }
    const payload = await slackApiPost(
      "chat.startStream",
      body,
      PROGRESS_START_TIMEOUT_MS,
    );
    const streamTs = optionalApiString(payload.ts);
    if (streamTs === null) {
      throw new Error("Slack chat.startStream returned no ts");
    }
    const channel = optionalApiString(payload.channel) ?? progress.channel;
    activeProgress.set(message.target, {
      mode: "stream",
      target: message.target,
      channel,
      threadTs: progress.threadTs,
      messageTs: streamTs,
      startedAtMs: Date.now(),
    });
    writeWorkerEvent({
      type: "lifecycle",
      name: "progress_stream_started",
      metadata: {
        target: message.target,
        channel,
        threadTs: progress.threadTs,
        streamTs,
      },
    });
  } catch (error) {
    const messageText = error instanceof Error ? error.message : String(error);
    writeWorkerEvent({
      type: "lifecycle",
      name: "progress_stream_failed",
      metadata: {
        target: message.target,
        channel: progress.channel,
        threadTs: progress.threadTs,
        error: messageText,
      },
    });
  }
}

async function finishSlackProgress(
  target: string,
  text: string,
): Promise<SlackProgressResult | null> {
  pruneExpiredSlackProgress();
  const progress = activeProgress.get(target);
  if (progress === undefined) {
    return null;
  }
  if (progress.mode === "update") {
    return {
      mode: "update",
      result: await updateSlackProgressMessage(progress, text),
    };
  }
  return {
    mode: "stream",
    result: await stopSlackStreamProgress(progress, text),
  };
}

async function updateSlackProgressMessage(
  progress: SlackProgressState,
  text: string,
): Promise<SlackPostMessageResult> {
  const payload = await slackApiPost("chat.update", {
    channel: progress.channel,
    ts: progress.messageTs,
    text,
    blocks: [],
  });
  activeProgress.delete(progress.target);
  const message = payload.message;
  return {
    channel: optionalApiString(payload.channel) ?? progress.channel,
    ts: optionalApiString(payload.ts) ?? progress.messageTs,
    messageTs: isRecord(message)
      ? optionalApiString(message.ts)
      : optionalApiString(payload.ts),
    text: isRecord(message) ? optionalApiString(message.text) : null,
    warning: optionalApiString(payload.warning),
  };
}

async function stopSlackStreamProgress(
  progress: SlackProgressState,
  text: string,
): Promise<SlackPostMessageResult> {
  const payload = await slackApiPost("chat.stopStream", {
    channel: progress.channel,
    ts: progress.messageTs,
    markdown_text: text,
  });
  activeProgress.delete(progress.target);
  const message = payload.message;
  return {
    channel: optionalApiString(payload.channel),
    ts: optionalApiString(payload.ts),
    messageTs: isRecord(message) ? optionalApiString(message.ts) : null,
    text: isRecord(message) ? optionalApiString(message.text) : null,
    warning: optionalApiString(payload.warning),
  };
}

function pruneExpiredSlackProgress(): void {
  const cutoff = Date.now() - MAX_ACTIVE_STREAM_AGE_MS;
  for (const [target, progress] of activeProgress.entries()) {
    if (progress.startedAtMs < cutoff) {
      activeProgress.delete(target);
    }
  }
}

async function postSlackMessage(
  destination: SlackDestination,
  text: string,
  timeoutMs = SEND_TIMEOUT_MS,
  blocks?: SlackBlock[],
): Promise<SlackPostMessageResult> {
  const payload = await slackApiPost(
    "chat.postMessage",
    {
      channel: destination.channel,
      text,
      ...(blocks ? { blocks } : {}),
      ...(destination.threadTs ? { thread_ts: destination.threadTs } : {}),
    },
    timeoutMs,
  );
  const message = payload.message;
  return {
    channel: optionalApiString(payload.channel),
    ts: optionalApiString(payload.ts),
    messageTs: isRecord(message) ? optionalApiString(message.ts) : null,
    text: isRecord(message) ? optionalApiString(message.text) : null,
    warning: optionalApiString(payload.warning),
  };
}

type SlackPostMessageResult = {
  channel: string | null;
  ts: string | null;
  messageTs: string | null;
  text: string | null;
  warning: string | null;
};

type SlackBlock = Record<string, unknown>;

function slackProgressBlocks(): SlackBlock[] {
  return [
    {
      type: "task_card",
      task_id: "exo_response",
      title: "Exo is working",
      status: "in_progress",
      details: {
        type: "rich_text",
        elements: [
          {
            type: "rich_text_section",
            elements: [
              {
                type: "text",
                text: "Preparing a response",
              },
            ],
          },
        ],
      },
    },
  ];
}

async function openSlackDm(userId: string): Promise<string> {
  const payload = await slackApiPost("conversations.open", {
    users: userId,
  });
  const channel = payload.channel;
  if (!isRecord(channel)) {
    throw new Error("Slack conversations.open returned no channel");
  }
  const channelId = optionalApiString(channel.id);
  if (channelId === null) {
    throw new Error("Slack conversations.open returned no channel id");
  }
  return channelId;
}

async function slackApiGet(method: string): Promise<Record<string, unknown>> {
  const response = await fetch(`https://slack.com/api/${method}`, {
    headers: {
      Authorization: `Bearer ${botToken}`,
    },
  });
  return parseSlackResponse(method, response);
}

async function slackApiPost(
  method: string,
  body: Record<string, unknown>,
  timeoutMs = SEND_TIMEOUT_MS,
): Promise<Record<string, unknown>> {
  let timeout: NodeJS.Timeout | null = null;
  const controller = new AbortController();
  try {
    timeout = setTimeout(() => {
      controller.abort(new Error(`Slack ${method} timed out`));
    }, timeoutMs);
    const response = await fetch(`https://slack.com/api/${method}`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${botToken}`,
        "Content-Type": "application/json; charset=utf-8",
      },
      body: JSON.stringify(body),
      signal: controller.signal,
    });
    return parseSlackResponse(method, response);
  } finally {
    if (timeout !== null) {
      clearTimeout(timeout);
    }
  }
}

async function parseSlackResponse(
  method: string,
  response: Response,
): Promise<Record<string, unknown>> {
  const text = await response.text();
  let payload: unknown;
  try {
    payload = text.length > 0 ? JSON.parse(text) : {};
  } catch {
    throw new Error(`Slack ${method} returned invalid JSON`);
  }
  if (!isRecord(payload)) {
    throw new Error(`Slack ${method} returned a non-object response`);
  }
  if (!response.ok || payload.ok !== true) {
    const error = optionalApiString(payload.error) ?? response.statusText;
    throw new Error(`Slack ${method} failed: ${error}`);
  }
  return payload;
}

type SlackInboundMessage = {
  target: string;
  sender: string | null;
  text: string;
  messageId: string | null;
  metadata: Record<string, unknown>;
  progress: SlackProgressRequest | null;
};

function inboundMessageFromPayload(
  payload: Record<string, unknown>,
): SlackInboundMessage | null {
  const event = payload.event;
  if (!isRecord(event)) {
    return null;
  }
  const eventType = optionalApiString(event.type);
  if (eventType !== "app_mention" && eventType !== "message") {
    return null;
  }
  if (eventType === "message" && event.subtype !== undefined) {
    return null;
  }

  const channel = optionalApiString(event.channel);
  const ts = optionalApiString(event.ts);
  const channelType = optionalApiString(event.channel_type);
  const isDm = channelType === "im";
  if (channel === null || ts === null) {
    return null;
  }
  if (allowedChannels !== null && !allowedChannels.includes(channel)) {
    return null;
  }
  const eventThreadTs = optionalApiString(event.thread_ts);
  const isActiveThread =
    eventType === "message" &&
    eventThreadTs !== null &&
    activeThreads.has(slackThreadKey(channel, eventThreadTs));
  if (
    trigger === "mentions_only" &&
    eventType !== "app_mention" &&
    !isDm &&
    !isActiveThread
  ) {
    return null;
  }

  const eventBotId = optionalApiString(event.bot_id);
  const userId = optionalApiString(event.user);
  const sender = userId ?? eventBotId;
  if (sender !== null && sender === botUserId) {
    return null;
  }
  if (eventBotId !== null) {
    if (eventBotId === botId) {
      return null;
    }
    if (!allowBots) {
      return null;
    }
  }

  const rawText = typeof event.text === "string" ? event.text : "";
  const text = stripOwnMention(rawText);
  const threadTs = threadReplies ? (eventThreadTs ?? ts) : null;
  if (eventType === "app_mention" && threadTs !== null) {
    activeThreads.add(slackThreadKey(channel, threadTs));
  }
  const dmTarget = userId === null ? null : `dm:${userId}`;
  const dmThreadTarget =
    dmTarget !== null && eventThreadTs !== null
      ? `${dmTarget}:${eventThreadTs}`
      : null;
  const target =
    isDm && dmTarget !== null
      ? (dmThreadTarget ?? dmTarget)
      : threadTs === null
        ? channel
        : `${channel}:${threadTs}`;
  const eventId = optionalApiString(payload.event_id);
  const teamId =
    optionalApiString(payload.team_id) ??
    optionalApiString(event.team) ??
    optionalApiString(event.user_team);
  const progress = slackProgressRequest({
    channel,
    threadTs: isDm ? eventThreadTs : threadTs,
    userId,
    teamId,
    eventType,
    isDm,
  });

  return {
    target,
    sender,
    text: text.length > 0 ? text : rawText,
    messageId: `${channel}:${ts}`,
    progress,
    metadata: {
      channel,
      threadTs,
      ts,
      eventId,
      eventType,
      channelType,
      isDm,
      isActiveThread,
      teamId,
      user: userId,
      botId: eventBotId,
      dmTarget,
      dmThreadTarget,
      progressMode,
      progressEligible: progress !== null,
    },
  };
}

function slackProgressRequest(input: {
  channel: string;
  threadTs: string | null;
  userId: string | null;
  teamId: string | null;
  eventType: string | null;
  isDm: boolean;
}): SlackProgressRequest | null {
  if (progressMode === "off" || input.userId === null) {
    return null;
  }
  if (input.eventType !== "app_mention" && !input.isDm) {
    return null;
  }
  if (progressMode === "stream" && input.threadTs === null) {
    return null;
  }
  return {
    mode: progressMode,
    channel: input.channel,
    threadTs: input.threadTs,
    recipientUserId: input.userId,
    recipientTeamId: input.teamId,
    isDm: input.isDm,
  };
}

function stripOwnMention(text: string): string {
  if (botUserId === null) {
    return text;
  }
  return text.replace(new RegExp(`^<@${escapeRegExp(botUserId)}>\\s*`), "");
}

type SlackDestination = {
  channel: string;
  threadTs: string | null;
  dmUserId: string | null;
};

async function slackDestination(target: string): Promise<SlackDestination> {
  if (target.startsWith("dm:")) {
    const dmTarget = target.slice("dm:".length);
    const separator = dmTarget.indexOf(":");
    const dmUserId = separator === -1 ? dmTarget : dmTarget.slice(0, separator);
    const threadTs = separator === -1 ? null : dmTarget.slice(separator + 1);
    if (dmUserId.length === 0) {
      throw new Error(
        "Slack DM target must be dm:USER_ID or dm:USER_ID:THREAD_TS",
      );
    }
    if (threadTs !== null && threadTs.length === 0) {
      throw new Error("Slack DM thread target must include THREAD_TS");
    }
    return {
      channel: await openSlackDm(dmUserId),
      threadTs,
      dmUserId,
    };
  }
  const separator = target.indexOf(":");
  if (separator === -1) {
    return { channel: target, threadTs: null, dmUserId: null };
  }
  const channel = target.slice(0, separator);
  const threadTs = target.slice(separator + 1);
  if (channel.length === 0 || threadTs.length === 0) {
    throw new Error(
      "Slack target must be CHANNEL_ID, CHANNEL_ID:THREAD_TS, dm:USER_ID, or dm:USER_ID:THREAD_TS",
    );
  }
  return { channel, threadTs, dmUserId: null };
}

function slackThreadKey(channel: string, threadTs: string): string {
  return `${channel}:${threadTs}`;
}

function verifySlackSignature(request: IncomingMessage, body: Buffer): boolean {
  const timestamp = headerValue(request.headers["x-slack-request-timestamp"]);
  const signature = headerValue(request.headers["x-slack-signature"]);
  if (timestamp === null || signature === null) {
    return false;
  }
  const timestampSeconds = Number(timestamp);
  if (!Number.isFinite(timestampSeconds)) {
    return false;
  }
  const ageSeconds = Math.abs(Date.now() / 1000 - timestampSeconds);
  if (ageSeconds > 60 * 5) {
    return false;
  }
  const base = `v0:${timestamp}:${body.toString("utf8")}`;
  const expected = `v0=${crypto
    .createHmac("sha256", signingSecret)
    .update(base)
    .digest("hex")}`;
  return timingSafeEqual(signature, expected);
}

function timingSafeEqual(left: string, right: string): boolean {
  const leftBuffer = Buffer.from(left);
  const rightBuffer = Buffer.from(right);
  if (leftBuffer.length !== rightBuffer.length) {
    return false;
  }
  return crypto.timingSafeEqual(leftBuffer, rightBuffer);
}

function parseJsonBody(body: Buffer): Record<string, unknown> {
  let payload: unknown;
  try {
    payload = JSON.parse(body.toString("utf8"));
  } catch {
    throw new Error("Slack request body is not valid JSON");
  }
  if (!isRecord(payload)) {
    throw new Error("Slack request body must be a JSON object");
  }
  return payload;
}

function readBody(request: IncomingMessage): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let byteLength = 0;
    request.on("data", (chunk: Buffer) => {
      byteLength += chunk.byteLength;
      if (byteLength > MAX_REQUEST_BYTES) {
        reject(new Error("Slack request body is too large"));
        request.destroy();
        return;
      }
      chunks.push(chunk);
    });
    request.on("end", () => resolve(Buffer.concat(chunks)));
    request.on("error", reject);
  });
}

function listen(server: http.Server, port: number): Promise<void> {
  return new Promise((resolve, reject) => {
    const onError = (error: Error): void => {
      server.off("listening", onListening);
      reject(error);
    };
    const onListening = (): void => {
      server.off("error", onError);
      resolve();
    };
    server.once("error", onError);
    server.once("listening", onListening);
    server.listen(port, "0.0.0.0");
  });
}

function sendJson(
  response: ServerResponse,
  status: number,
  body: Record<string, unknown>,
): void {
  response.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
  });
  response.end(JSON.stringify(body));
}

function headerValue(value: string | string[] | undefined): string | null {
  if (Array.isArray(value)) {
    return value[0] ?? null;
  }
  return value ?? null;
}

function optionalApiString(value: unknown): string | null {
  if (typeof value !== "string" || value.length === 0) {
    return null;
  }
  return value;
}

function optionalPort(value: unknown): number {
  if (value === undefined || value === null) {
    return 3939;
  }
  if (
    typeof value !== "number" ||
    !Number.isInteger(value) ||
    value <= 0 ||
    value > 65535
  ) {
    throw new Error("Slack port must be an integer from 1 to 65535");
  }
  return value;
}

type SlackProgressMode = "update" | "stream" | "off";

function slackProgressMode(value: unknown): SlackProgressMode {
  if (value === undefined || value === null) {
    return "update";
  }
  if (value === "update" || value === "stream" || value === "off") {
    return value;
  }
  throw new Error("Slack progressMode must be update, stream, or off");
}

function requiredEnv(name: string, label: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${label} missing from ${name}`);
  }
  return value;
}

function stringArrayOrNull(value: unknown): string[] | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (
    Array.isArray(value) &&
    value.every((item): item is string => typeof item === "string")
  ) {
    return value;
  }
  throw new Error("Slack allowedChannels must be null or an array of strings");
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function reportWorkerError(message: string): void {
  writeWorkerEvent({ type: "error", message });
}
