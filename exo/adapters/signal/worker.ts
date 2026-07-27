import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import fs from "node:fs/promises";
import readline from "node:readline/promises";

import qrcodeTerminal from "qrcode-terminal";

import {
  type AdapterAttachment,
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";

const config = adapterConfig();
const signalCliCommand = stringArrayOrDefault(
  config.signalCliCommand,
  ["signal-cli"],
  "signalCliCommand",
);
const configDir =
  optionalStringField(config, "configDir") ??
  (process.env.EXO_ADAPTER_STATE_DIR === undefined
    ? `.exo/adapters/signal/${process.env.EXO_ADAPTER_ID ?? "default"}/signal-cli`
    : `${process.env.EXO_ADAPTER_STATE_DIR}/signal-cli`);
const configuredAccount = optionalStringField(config, "account");
const deviceName = optionalStringField(config, "deviceName") ?? "Exo";
const trigger = optionalStringField(config, "trigger") ?? "all_messages";
const allowedContacts = stringArrayOrNull(config.allowedContacts);
if (trigger !== "all_messages" && trigger !== "contacts_only") {
  throw new Error("Signal trigger must be all_messages or contacts_only");
}

await fs.mkdir(configDir, { recursive: true });
const account = configuredAccount ?? (await discoverOrLinkAccount());
const signal = spawnSignalCli([
  "-a",
  account,
  "jsonRpc",
  "--receive-mode=on-start",
]);
installChildCleanup(signal);

writeWorkerEvent({
  type: "connected",
  subject: account,
  metadata: { account },
});

const pending = new Map<
  string,
  {
    resolve: (value: unknown) => void;
    reject: (error: Error) => void;
  }
>();
let nextRequestId = 1;

signal.stderr.on("data", (chunk) => {
  process.stderr.write(`[signal-adapter] ${chunk.toString()}`);
});

signal.on("exit", (code, signalName) => {
  writeWorkerEvent({
    type: "disconnected",
    reason: signalName ?? (code === null ? null : String(code)),
  });
  if (code !== 0) {
    process.exit(code ?? 1);
  }
});

const signalOutput = readline.createInterface({
  input: signal.stdout,
  crlfDelay: Number.POSITIVE_INFINITY,
});

void (async () => {
  for await (const line of signalOutput) {
    if (line.trim().length === 0) {
      continue;
    }
    try {
      handleJsonRpcMessage(JSON.parse(line) as unknown);
    } catch (error) {
      writeWorkerEvent({
        type: "error",
        message: error instanceof Error ? error.message : String(error),
      });
    }
  }
})();

const input = readline.createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

for await (const line of input) {
  if (line.trim().length === 0) {
    continue;
  }
  let commandId: string | null = null;
  try {
    const command = parseWorkerCommand(JSON.parse(line));
    commandId = command.id;
    if (!command.target) {
      throw new Error(
        "Signal send_message requires a target username, uuid, phone number, or group id",
      );
    }
    await sendSignalMessage(command.target, command.text, command.attachments);
    writeWorkerEvent({ type: "command_ack", command_id: command.id });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    writeWorkerEvent({
      type: "error",
      message,
    });
    if (commandId !== null) {
      writeWorkerEvent({
        type: "command_nack",
        command_id: commandId,
        message,
      });
    }
  }
}

function handleJsonRpcMessage(message: unknown): void {
  if (!isRecord(message)) {
    throw new Error("signal-cli JSON-RPC message must be an object");
  }
  if (typeof message.id === "string" || typeof message.id === "number") {
    const id = String(message.id);
    const callback = pending.get(id);
    if (callback !== undefined) {
      pending.delete(id);
      if (isRecord(message.error)) {
        callback.reject(new Error(JSON.stringify(message.error)));
      } else {
        callback.resolve(message.result);
      }
    }
    return;
  }
  if (message.method !== "receive" || !isRecord(message.params)) {
    return;
  }
  const inbound = inboundMessage(message.params);
  if (inbound === null || !shouldTrigger(inbound.sender ?? inbound.target)) {
    return;
  }
  writeWorkerEvent(inbound);
}

function inboundMessage(params: Record<string, unknown>) {
  const envelope = params.envelope;
  if (!isRecord(envelope)) {
    return null;
  }
  const dataMessage = envelope.dataMessage;
  if (!isRecord(dataMessage)) {
    return null;
  }
  const message = dataMessage.message;
  if (typeof message !== "string" || message.length === 0) {
    return null;
  }
  const groupInfo = dataMessage.groupInfo;
  const groupId = isRecord(groupInfo) ? stringOrNull(groupInfo.groupId) : null;
  const source =
    stringOrNull(envelope.source) ??
    stringOrNull(envelope.sourceNumber) ??
    stringOrNull(envelope.sourceUuid) ??
    stringOrNull(envelope.sourceName);
  const timestamp =
    numberOrStringOrNull(dataMessage.timestamp) ??
    numberOrStringOrNull(envelope.timestamp);
  return {
    type: "message" as const,
    target: groupId ?? source ?? account,
    sender: source,
    text: message,
    message_id: timestamp,
    metadata: {
      sourceName: stringOrNull(envelope.sourceName),
      sourceUuid: stringOrNull(envelope.sourceUuid),
      groupId,
    },
  };
}

async function sendSignalMessage(
  target: string,
  text: string,
  attachments: AdapterAttachment[],
): Promise<void> {
  const attachmentParams = signalAttachments(attachments);
  const params = looksLikeGroupId(target)
    ? { groupId: target, message: text, ...attachmentParams }
    : {
        recipient: [normalizeRecipient(target)],
        message: text,
        ...attachmentParams,
      };
  writeWorkerEvent({
    type: "lifecycle",
    name: "send_starting",
    metadata: { target, params },
  });
  const result = await jsonRpcRequest("send", params, 30_000);
  writeWorkerEvent({
    type: "lifecycle",
    name: "send_result",
    metadata: { target, params, result },
  });
}

function signalAttachments(attachments: AdapterAttachment[]): {
  attachments?: string[];
} {
  if (attachments.length === 0) {
    return {};
  }
  return {
    attachments: attachments.map(signalAttachmentSource),
  };
}

function signalAttachmentSource(attachment: AdapterAttachment): string {
  if (attachment.path) {
    return attachment.path;
  }
  if (attachment.data) {
    return signalDataUri(attachment);
  }
  if (attachment.url) {
    throw new Error(
      "Signal attachment URL should have been staged by the host tool before reaching the worker",
    );
  }
  throw new Error("Signal attachment requires staged path or data");
}

function signalDataUri(attachment: AdapterAttachment): string {
  if (attachment.data?.startsWith("data:")) {
    return attachment.data;
  }
  const mimeType = attachment.mimeType ?? "application/octet-stream";
  const fileName = attachment.fileName
    ? `;filename=${encodeURIComponent(attachment.fileName)}`
    : "";
  return `data:${mimeType}${fileName};base64,${attachment.data}`;
}

function jsonRpcRequest(
  method: string,
  params: Record<string, unknown>,
  timeoutMs = 30_000,
): Promise<unknown> {
  const id = String(nextRequestId++);
  const request = { jsonrpc: "2.0", method, params, id };
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      killSignalCliProcess(signal);
      reject(
        new Error(
          `signal-cli JSON-RPC ${method} timed out after ${timeoutMs}ms`,
        ),
      );
    }, timeoutMs);
    pending.set(id, {
      resolve: (value) => {
        clearTimeout(timeout);
        resolve(value);
      },
      reject: (error) => {
        clearTimeout(timeout);
        reject(error);
      },
    });
    signal.stdin.write(`${JSON.stringify(request)}\n`, (error) => {
      if (error) {
        clearTimeout(timeout);
        pending.delete(id);
        reject(error);
      }
    });
  });
}

async function discoverOrLinkAccount(): Promise<string> {
  const localAccounts = await localAccountsFromConfig();
  if (localAccounts.length > 0) {
    const existing = localAccounts[0];
    writeWorkerEvent({
      type: "lifecycle",
      name: "account_discovered",
      metadata: { account: existing, source: "config" },
    });
    return existing;
  }
  const existingAccounts = await listAccountsOrEmpty(
    "account_discovery_failed",
  );
  if (existingAccounts.length > 0) {
    const existing = existingAccounts[0];
    writeWorkerEvent({
      type: "lifecycle",
      name: "account_discovered",
      metadata: { account: existing },
    });
    return existing;
  }
  return linkAndDiscoverAccount();
}

async function linkAndDiscoverAccount(): Promise<string> {
  writeWorkerEvent({
    type: "lifecycle",
    name: "link_starting",
    metadata: { deviceName },
  });
  const link = spawnSignalCli(["link", "-n", deviceName]);
  const linkOutput = readline.createInterface({
    input: link.stdout,
    crlfDelay: Number.POSITIVE_INFINITY,
  });
  let linkUri: string | null = null;
  const outputTask = (async () => {
    for await (const line of linkOutput) {
      process.stderr.write(`[signal-adapter] ${line}\n`);
      const uri = line.match(/(?:sgnl|tsdevice):\/\/\S+/)?.[0] ?? null;
      if (uri !== null && linkUri === null) {
        linkUri = uri;
        qrcodeTerminal.generate(uri, { small: true }, (qr) => {
          process.stderr.write(
            `\n[signal-adapter] Scan this QR with Signal:\n${qr}\n`,
          );
        });
        writeWorkerEvent({
          type: "lifecycle",
          name: "link_qr",
          metadata: { uri },
        });
      }
    }
  })();
  link.stderr.on("data", (chunk) => {
    process.stderr.write(`[signal-adapter] ${chunk.toString()}`);
  });
  const exitCode = await waitForExit(link);
  await outputTask;
  if (exitCode !== 0) {
    throw new Error(`signal-cli link failed with exit code ${exitCode}`);
  }
  const accounts = await localAccountsFromConfig();
  if (accounts.length === 0) {
    const signalCliAccounts = await listAccountsOrEmpty(
      "post_link_account_discovery_failed",
    );
    if (signalCliAccounts.length > 0) {
      const discovered = signalCliAccounts[0];
      writeWorkerEvent({
        type: "lifecycle",
        name: "linked",
        metadata: { account: discovered, source: "signal-cli" },
      });
      return discovered;
    }
    throw new Error(
      "signal-cli link completed, but no local accounts were found",
    );
  }
  const discovered = accounts[0];
  writeWorkerEvent({
    type: "lifecycle",
    name: "linked",
    metadata: { account: discovered, source: "config" },
  });
  return discovered;
}

async function listAccountsOrEmpty(errorEvent: string): Promise<string[]> {
  try {
    return await listAccounts();
  } catch (error) {
    writeWorkerEvent({
      type: "lifecycle",
      name: errorEvent,
      metadata: {
        message: error instanceof Error ? error.message : String(error),
      },
    });
    return [];
  }
}

async function localAccountsFromConfig(): Promise<string[]> {
  const accountsPath = `${configDir}/data/accounts.json`;
  let contents: string;
  try {
    contents = await fs.readFile(accountsPath, "utf8");
  } catch (error) {
    if (isNodeErrorWithCode(error, "ENOENT")) {
      return [];
    }
    throw error;
  }

  const parsed = JSON.parse(contents) as unknown;
  if (!isRecord(parsed) || !Array.isArray(parsed.accounts)) {
    return [];
  }
  return parsed.accounts
    .map((account) => {
      if (!isRecord(account)) {
        return null;
      }
      return stringOrNull(account.number) ?? stringOrNull(account.uuid);
    })
    .filter((account) => account !== null);
}

async function listAccounts(): Promise<string[]> {
  const output = await runSignalCli(["listAccounts"]);
  return output
    .split(/\r?\n/)
    .map((line) => line.trim())
    .map(parseSignalAccountLine)
    .filter((account) => account !== null);
}

function parseSignalAccountLine(line: string): string | null {
  const number = line.match(/(?:Number|Account):\s*(\+\d+)/)?.[1];
  if (number !== undefined) {
    return number;
  }
  if (
    line.startsWith("+") ||
    line.startsWith("ACI:") ||
    line.startsWith("PNI:") ||
    /^[0-9a-fA-F-]{32,36}$/.test(line)
  ) {
    return line;
  }
  return null;
}

function spawnSignalCli(args: string[]): ChildProcessWithoutNullStreams {
  return spawn(
    signalCliCommand[0],
    [...signalCliCommand.slice(1), "--config", configDir, ...args],
    {
      detached: process.platform !== "win32",
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
}

function installChildCleanup(child: ChildProcessWithoutNullStreams): void {
  const stopChild = () => {
    killSignalCliProcess(child);
  };
  process.once("exit", stopChild);
  process.once("SIGINT", () => {
    stopChild();
    process.exit(130);
  });
  process.once("SIGTERM", () => {
    stopChild();
    process.exit(143);
  });
}

function killSignalCliProcess(child: ChildProcessWithoutNullStreams): void {
  if (child.exitCode !== null || child.signalCode !== null) {
    return;
  }
  if (process.platform !== "win32" && child.pid !== undefined) {
    try {
      process.kill(-child.pid, "SIGTERM");
      return;
    } catch {
      // Fall back to killing the direct child below.
    }
  }
  child.kill();
}

async function runSignalCli(args: string[]): Promise<string> {
  const child = spawnSignalCli(args);
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => {
    stdout += chunk.toString();
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk.toString();
  });
  const code = await waitForExitWithTimeout(child, args, 30_000);
  if (code !== 0) {
    throw new Error(`signal-cli ${args.join(" ")} failed: ${stderr.trim()}`);
  }
  return stdout;
}

function waitForExit(
  child: ChildProcessWithoutNullStreams,
): Promise<number | null> {
  return new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("exit", (code) => resolve(code));
  });
}

async function waitForExitWithTimeout(
  child: ChildProcessWithoutNullStreams,
  args: string[],
  timeoutMs: number,
): Promise<number | null> {
  let timeout: NodeJS.Timeout | null = null;
  try {
    return await Promise.race([
      waitForExit(child),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(() => {
          killSignalCliProcess(child);
          reject(
            new Error(
              `signal-cli ${args.join(" ")} timed out after ${timeoutMs}ms`,
            ),
          );
        }, timeoutMs);
      }),
    ]);
  } finally {
    if (timeout !== null) {
      clearTimeout(timeout);
    }
  }
}

function normalizeRecipient(target: string): string {
  if (
    target.startsWith("u:") ||
    target.startsWith("+") ||
    target.startsWith("ACI:") ||
    target.startsWith("PNI:") ||
    /^[0-9a-fA-F-]{32,36}$/.test(target)
  ) {
    return target;
  }
  return `u:${target}`;
}

function looksLikeRecipient(target: string): boolean {
  return (
    target.startsWith("u:") ||
    target.startsWith("+") ||
    target.startsWith("ACI:") ||
    target.startsWith("PNI:") ||
    /^[0-9a-fA-F-]{32,36}$/.test(target)
  );
}

function shouldTrigger(sender: string): boolean {
  if (allowedContacts !== null) {
    return allowedContacts.includes(sender);
  }
  return trigger === "all_messages" || sender.length > 0;
}

function looksLikeGroupId(target: string): boolean {
  return !looksLikeRecipient(target) && /^[A-Za-z0-9+/=_-]{20,}$/.test(target);
}

function stringArrayOrDefault(
  value: unknown,
  fallback: string[],
  name: string,
): string[] {
  if (value === null || value === undefined) {
    return fallback;
  }
  if (Array.isArray(value) && value.every((item) => typeof item === "string")) {
    return value.length === 0 ? fallback : value;
  }
  throw new Error(`Signal ${name} must be null or an array of strings`);
}

function stringArrayOrNull(value: unknown): string[] | null {
  if (value === null || value === undefined) {
    return null;
  }
  if (Array.isArray(value) && value.every((item) => typeof item === "string")) {
    return value;
  }
  throw new Error("Signal allowedContacts must be null or an array of strings");
}

function stringOrNull(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 ? value : null;
}

function numberOrStringOrNull(value: unknown): string | null {
  if (typeof value === "number" || typeof value === "string") {
    return String(value);
  }
  return null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function isNodeErrorWithCode(error: unknown, code: string): boolean {
  return (
    error instanceof Error &&
    "code" in error &&
    (error as { code?: unknown }).code === code
  );
}
