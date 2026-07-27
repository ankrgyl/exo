export type WorkerOutboundCommand = {
  type: "send_message";
  id: string;
  target?: string | null;
  text: string;
  attachments: AdapterAttachment[];
};

export type AdapterAttachment = {
  kind: "image" | "video" | "audio" | "document";
  path?: string | null;
  url?: string | null;
  data?: string | null;
  mimeType?: string | null;
  fileName?: string | null;
};

export type WorkerInboundEvent =
  | {
      type: "connected";
      subject?: string | null;
      metadata?: JsonObject;
    }
  | {
      type: "message";
      target: string;
      sender?: string | null;
      text: string;
      message_id?: string | null;
      metadata?: JsonObject;
      attachments?: AdapterAttachment[];
    }
  | {
      type: "lifecycle";
      name: string;
      metadata?: JsonObject;
    }
  | {
      type: "error";
      message: string;
    }
  | {
      type: "command_ack";
      command_id: string;
    }
  | {
      type: "command_nack";
      command_id: string;
      message: string;
    }
  | {
      type: "disconnected";
      reason?: string | null;
    };

type JsonObject = Record<string, unknown>;
let stdoutErrorHandlerInstalled = false;

export function adapterConfig(): JsonObject {
  const raw = process.env.EXO_ADAPTER_CONFIG;
  if (!raw) {
    return {};
  }
  const parsed = JSON.parse(raw) as unknown;
  if (!isRecord(parsed)) {
    throw new Error("EXO_ADAPTER_CONFIG must contain a JSON object");
  }
  return parsed;
}

export function parseWorkerCommand(value: unknown): WorkerOutboundCommand {
  if (!isRecord(value) || value.type !== "send_message") {
    throw new Error("worker command must be a send_message object");
  }
  if (typeof value.id !== "string" || value.id.length === 0) {
    throw new Error("send_message id must be a non-empty string");
  }
  if (
    value.target !== undefined &&
    value.target !== null &&
    (typeof value.target !== "string" || value.target.length === 0)
  ) {
    throw new Error("send_message target must be null or a non-empty string");
  }
  if (typeof value.text !== "string" || value.text.length === 0) {
    throw new Error("send_message text must be a non-empty string");
  }
  return {
    type: "send_message",
    id: value.id,
    target: value.target ?? null,
    text: value.text,
    attachments: parseAttachments(value.attachments),
  };
}

function parseAttachments(value: unknown): AdapterAttachment[] {
  if (value === undefined || value === null) {
    return [];
  }
  if (!Array.isArray(value)) {
    throw new Error("send_message attachments must be null or an array");
  }
  return value.map((item) => {
    if (!isRecord(item)) {
      throw new Error("send_message attachment must be an object");
    }
    if (
      item.kind !== "image" &&
      item.kind !== "video" &&
      item.kind !== "audio" &&
      item.kind !== "document"
    ) {
      throw new Error(
        "send_message attachment kind must be image, video, audio, or document",
      );
    }
    const path = nullableStringValue(item.path, "attachment path");
    const url = nullableStringValue(item.url, "attachment url");
    const data = nullableStringValue(item.data, "attachment data");
    const sourceCount = [path, url, data].filter(
      (source) => source !== null,
    ).length;
    if (sourceCount !== 1) {
      throw new Error(
        "send_message attachment must specify exactly one of path, url, or data",
      );
    }
    if (url !== null && !url.startsWith("https://")) {
      throw new Error("send_message attachment url must use https");
    }
    return {
      kind: item.kind,
      path,
      url,
      data,
      mimeType: nullableStringValue(item.mimeType, "attachment mimeType"),
      fileName: nullableStringValue(item.fileName, "attachment fileName"),
    };
  });
}

function nullableStringValue(value: unknown, name: string): string | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${name} must be null or a non-empty string`);
  }
  return value;
}

export function stringField(config: JsonObject, name: string): string {
  const value = config[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`adapter config ${name} must be a non-empty string`);
  }
  return value;
}

export function optionalStringField(
  config: JsonObject,
  name: string,
): string | null {
  const value = config[name];
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(
      `adapter config ${name} must be null or a non-empty string`,
    );
  }
  return value;
}

export function booleanField(config: JsonObject, name: string): boolean {
  const value = config[name];
  if (typeof value !== "boolean") {
    throw new Error(`adapter config ${name} must be a boolean`);
  }
  return value;
}

export function numberField(config: JsonObject, name: string): number {
  const value = config[name];
  if (typeof value !== "number") {
    throw new Error(`adapter config ${name} must be a number`);
  }
  return value;
}

export function writeWorkerEvent(event: WorkerInboundEvent): void {
  ensureStdoutErrorHandler();
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

export function isRecord(value: unknown): value is JsonObject {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function ensureStdoutErrorHandler(): void {
  if (stdoutErrorHandlerInstalled) {
    return;
  }
  stdoutErrorHandlerInstalled = true;
  process.stdout.on("error", (error: NodeJS.ErrnoException) => {
    if (error.code === "EPIPE") {
      process.exit(0);
    }
    throw error;
  });
}
