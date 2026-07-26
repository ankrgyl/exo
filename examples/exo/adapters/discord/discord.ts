import type { AdapterAttachment, WorkerInboundEvent } from "../protocol";

export type DiscordAttachmentLike = {
  url: string;
  contentType?: string | null;
  name?: string | null;
};

const DISCORD_CONTENT_LIMIT = 2_000;

export function splitDiscordContent(
  text: string,
  limit = DISCORD_CONTENT_LIMIT,
): string[] {
  if (text.length <= limit) {
    return [text];
  }

  const chunks: string[] = [];
  let remaining = text;
  while (remaining.length > limit) {
    const window = remaining.slice(0, limit);
    const newline = window.lastIndexOf("\n");
    const space = window.lastIndexOf(" ");
    const splitAt = newline > 0 ? newline + 1 : space > 0 ? space + 1 : limit;
    chunks.push(remaining.slice(0, splitAt));
    remaining = remaining.slice(splitAt);
  }
  if (remaining.length > 0) {
    chunks.push(remaining);
  }
  return chunks;
}

export function attachmentKindForContentType(
  contentType: string | null | undefined,
): AdapterAttachment["kind"] {
  const mime = (contentType ?? "").toLowerCase();
  if (mime.startsWith("image/")) {
    return "image";
  }
  if (mime.startsWith("video/")) {
    return "video";
  }
  if (mime.startsWith("audio/")) {
    return "audio";
  }
  return "document";
}

/// Maps inbound Discord attachments to protocol attachments. Discord CDN URLs
/// carry expiring signatures, so the host downloads them promptly.
export function inboundAttachments(
  attachments: Iterable<DiscordAttachmentLike>,
): AdapterAttachment[] {
  const mapped: AdapterAttachment[] = [];
  for (const attachment of attachments) {
    if (!attachment.url) {
      continue;
    }
    mapped.push({
      kind: attachmentKindForContentType(attachment.contentType),
      path: null,
      url: attachment.url,
      data: null,
      mimeType: attachment.contentType ?? null,
      fileName: attachment.name ?? null,
    });
  }
  return mapped;
}

export function errorMessage(value: unknown): string {
  if (value instanceof Error) {
    return value.message;
  }
  try {
    return String(value);
  } catch {
    return "unknown error";
  }
}

export function isTlsAccessDeniedError(error: unknown): boolean {
  if (!(error instanceof Error)) {
    return false;
  }
  const code = (error as { code?: unknown }).code;
  return (
    code === "EPROTO" && error.message.includes("tlsv1 alert access denied")
  );
}

// Gateway close codes discord.js will not reconnect from. Each is a
// configuration problem that a worker restart cannot fix. discord.js emits the
// Client "shardDisconnect" event only for these codes; every other close is
// retried internally and surfaces as "shardReconnecting" instead.
// https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-close-event-codes
const UNRECOVERABLE_CLOSE_CODES = new Map<number, string>([
  [4004, "authentication failed: the bot token is invalid"],
  [4010, "invalid shard"],
  [4011, "sharding required"],
  [4012, "invalid gateway API version"],
  [4013, "invalid gateway intents"],
  [
    4014,
    "disallowed gateway intents: enable the privileged intents in the Discord developer portal",
  ],
]);

export function describeCloseCode(code: number): string {
  const detail = UNRECOVERABLE_CLOSE_CODES.get(code);
  return detail ? `${detail} (code ${code})` : `gateway closed (code ${code})`;
}

export type ResilienceDeps = {
  emit: (event: WorkerInboundEvent) => void;
  exit: (code: number) => void;
};

export type ResilienceHandlers = {
  onUnhandledRejection: (reason: unknown) => void;
  onUncaughtException: (error: unknown) => void;
  onShardDisconnect: (code: number) => void;
  onShardError: (error: unknown) => void;
  onLoginFailure: (error: unknown) => void;
};

// The worker is a child process the adapter runner restarts on exit with
// exponential backoff (5s doubling to 5min). Exiting is therefore the safe
// default for any state a fresh start could improve, and the backoff bounds
// the reconnect churn even for failures a restart cannot fix. The emit/exit
// side effects are injected so the decisions can be unit-tested without a
// real process or gateway.
export function createResilienceHandlers(
  deps: ResilienceDeps,
): ResilienceHandlers {
  return {
    onUnhandledRejection(reason) {
      // A rejection that escapes discord.js's own gateway recovery leaves the
      // worker in an unknown state, so surface it and exit for a clean restart.
      deps.emit({
        type: "error",
        message: `unhandled rejection: ${errorMessage(reason)}`,
      });
      deps.exit(1);
    },
    onUncaughtException(error) {
      if (isTlsAccessDeniedError(error)) {
        deps.emit({
          type: "error",
          message: `Discord TLS stream error: ${errorMessage(error)}`,
        });
        return;
      }
      deps.emit({
        type: "error",
        message: `uncaught exception: ${errorMessage(error)}`,
      });
      deps.exit(1);
    },
    onShardDisconnect(code) {
      // discord.js emits shardDisconnect only for close codes it will not
      // reconnect from. Staying up here leaves a zombie worker that looks
      // alive to the runner but will never receive a message again, so exit
      // and let the runner's backoff bound the retry rate while the cause
      // (usually configuration) is fixed.
      deps.emit({ type: "disconnected", reason: describeCloseCode(code) });
      deps.exit(1);
    },
    onShardError(error) {
      // Shard errors are transient; discord.js keeps reconnecting, so report
      // the error without tearing the worker down.
      deps.emit({
        type: "error",
        message: `shard error: ${errorMessage(error)}`,
      });
    },
    onLoginFailure(error) {
      // Login can fail transiently, in which case a restart may succeed, so
      // surface the error and exit. A persistently bad token reports this on
      // each retry.
      deps.emit({
        type: "error",
        message: `discord login failed: ${errorMessage(error)}`,
      });
      deps.exit(1);
    },
  };
}

export type ConnectionWatchdogDeps = {
  isReady: () => boolean;
  emit: (event: WorkerInboundEvent) => void;
  exit: (code: number) => void;
  intervalMs?: number;
  timeoutMs?: number;
};

// Some failures leave the gateway dead without any event firing (e.g. DNS
// breaks mid-session and discord.js wedges while retrying). The watchdog
// covers that gap: if the client has not been ready for timeoutMs, the worker
// exits so the runner restarts it on a fresh connection. Returns a stop
// function.
export function startConnectionWatchdog(
  deps: ConnectionWatchdogDeps,
): () => void {
  const intervalMs = deps.intervalMs ?? 30_000;
  const timeoutMs = deps.timeoutMs ?? 5 * 60_000;
  let lastReadyAtMs = Date.now();
  const timer = setInterval(() => {
    if (deps.isReady()) {
      lastReadyAtMs = Date.now();
      return;
    }
    const staleMs = Date.now() - lastReadyAtMs;
    if (staleMs >= timeoutMs) {
      deps.emit({
        type: "error",
        message: `discord gateway not ready for ${Math.round(staleMs / 1000)}s; exiting so the runner restarts the worker`,
      });
      deps.exit(1);
    }
  }, intervalMs);
  timer.unref?.();
  return () => clearInterval(timer);
}
