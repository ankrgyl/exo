#!/usr/bin/env node

import fs from "node:fs/promises";
import path from "node:path";
import process from "node:process";

const POLL_INTERVAL_MS = 300;
const DEFAULT_HISTORY = 20;
const MAX_DETAIL_CHARS = 1_200;

const options = parseArguments(process.argv.slice(2));
if (options.help) {
  printHelp();
  process.exit(0);
}

const eventsDirectory = await resolveEventsDirectory(
  process.cwd(),
  options.agentSlug,
  options.conversationSlug,
);

console.log(
  `Following ${options.agentSlug}/${options.conversationSlug} events in ${path.relative(process.cwd(), eventsDirectory)}`,
);
console.log("Press Ctrl-C to stop.\n");

let cursor = "";
const initialFiles = await eventFiles(eventsDirectory);
for (const fileName of initialFiles.slice(-options.history)) {
  await printEvent(path.join(eventsDirectory, fileName));
}
if (initialFiles.length > 0) {
  cursor = initialFiles.at(-1);
}

while (true) {
  await delay(POLL_INTERVAL_MS);
  const files = await eventFiles(eventsDirectory);
  for (const fileName of files) {
    if (fileName <= cursor) {
      continue;
    }
    await printEvent(path.join(eventsDirectory, fileName));
    cursor = fileName;
  }
}

function parseArguments(args) {
  const positionals = [];
  let history = DEFAULT_HISTORY;
  let help = false;

  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--help" || argument === "-h") {
      help = true;
    } else if (argument === "--history") {
      const value = args[index + 1];
      if (value === undefined) {
        throw new Error("--history requires a non-negative integer");
      }
      history = parseHistory(value);
      index += 1;
    } else if (argument.startsWith("--history=")) {
      history = parseHistory(argument.slice("--history=".length));
    } else if (argument.startsWith("-")) {
      throw new Error(`unknown option: ${argument}`);
    } else {
      positionals.push(argument);
    }
  }

  if (positionals.length > 2) {
    throw new Error("expected at most an agent slug and conversation slug");
  }

  return {
    agentSlug: positionals[0] ?? "exo-agent",
    conversationSlug: positionals[1] ?? "dev",
    history,
    help,
  };
}

function parseHistory(value) {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 0) {
    throw new Error("--history requires a non-negative integer");
  }
  return parsed;
}

function printHelp() {
  console.log(`Usage:
  node exo/scripts/tail-agent-events.mjs [agent-slug] [conversation-slug] [--history N]

Defaults:
  agent-slug       exo-agent
  conversation-slug dev
  history          ${DEFAULT_HISTORY}

Examples:
  pnpm events:tail
  pnpm events:tail exo-agent dev --history 50
  pnpm events:tail exo-agent dev --history 0`);
}

async function resolveEventsDirectory(workspace, agentSlug, conversationSlug) {
  const agentsDirectory = path.join(workspace, ".exo", "exoharness", "agents");
  const slugPath = path.join(agentsDirectory, "by-slug", `${agentSlug}.slug`);
  const agentId = await readJson(slugPath);
  if (typeof agentId !== "string" || agentId.length === 0) {
    throw new Error(`invalid agent slug mapping: ${slugPath}`);
  }

  const conversationsDirectory = path.join(
    agentsDirectory,
    agentId,
    "conversations",
  );
  const entries = await fs.readdir(conversationsDirectory, {
    withFileTypes: true,
  });
  for (const entry of entries) {
    if (!entry.isDirectory()) {
      continue;
    }
    const conversationDirectory = path.join(conversationsDirectory, entry.name);
    const recordPath = path.join(conversationDirectory, "record.json");
    let record;
    try {
      record = await readJson(recordPath);
    } catch (error) {
      if (isNotFoundError(error)) {
        continue;
      }
      throw error;
    }
    if (
      record !== null &&
      typeof record === "object" &&
      record.slug === conversationSlug
    ) {
      return path.join(conversationDirectory, "events");
    }
  }

  throw new Error(
    `conversation ${conversationSlug} was not found for agent ${agentSlug}`,
  );
}

async function eventFiles(directory) {
  return (await fs.readdir(directory))
    .filter((name) => name.endsWith(".json"))
    .sort();
}

async function printEvent(filePath) {
  let event;
  try {
    event = await readJson(filePath);
  } catch (error) {
    console.error(`[event read error] ${path.basename(filePath)}: ${error}`);
    return;
  }

  const timestamp =
    typeof event.created_at === "string"
      ? event.created_at.slice(11, 23)
      : "--:--:--";
  const data = event.data;
  if (data === null || typeof data !== "object") {
    console.log(`${timestamp} [unknown] ${compact(event)}`);
    return;
  }

  switch (data.type) {
    case "messages":
      printMessages(timestamp, data.messages);
      break;
    case "tool_requested":
      console.log(
        `${timestamp} [tool →] ${data.request?.function_name ?? "unknown"} ${compact(data.request?.arguments ?? {})}`,
      );
      break;
    case "tool_result": {
      const result = data.result ?? {};
      const status = result.ok === false ? "error" : "ok";
      const name = result.toolName ?? "unknown";
      const detail = result.preview ?? result.value ?? result;
      console.log(`${timestamp} [tool ← ${status}] ${name} ${compact(detail)}`);
      break;
    }
    case "turn_started":
      console.log(`${timestamp} [turn started]`);
      break;
    case "turn_ended":
      console.log(`${timestamp} [turn ended]\n`);
      break;
    case "session_started":
    case "session_ended":
      console.log(`${timestamp} [${data.type.replaceAll("_", " ")}]`);
      break;
    case "artifact_written":
      break;
    default:
      console.log(`${timestamp} [${data.type ?? "unknown"}]`);
  }
}

function printMessages(timestamp, messages) {
  if (!Array.isArray(messages)) {
    return;
  }
  for (const message of messages) {
    const role = message?.role ?? "unknown";
    if (typeof message?.content === "string") {
      console.log(`${timestamp} [${role}] ${compact(message.content)}`);
      continue;
    }
    if (!Array.isArray(message?.content)) {
      continue;
    }
    for (const part of message.content) {
      if (part?.type === "text") {
        console.log(`${timestamp} [${role}] ${compact(part.text ?? "")}`);
      } else if (part?.type === "tool_call") {
        const args = part.arguments?.value ?? part.arguments ?? {};
        console.log(
          `${timestamp} [${role} plans tool] ${part.tool_name ?? "unknown"} ${compact(args)}`,
        );
      }
    }
  }
}

function compact(value) {
  const text =
    typeof value === "string"
      ? value.replaceAll(/\s+/g, " ").trim()
      : JSON.stringify(value);
  if (text.length <= MAX_DETAIL_CHARS) {
    return text;
  }
  return `${text.slice(0, MAX_DETAIL_CHARS)}…`;
}

async function readJson(filePath) {
  return JSON.parse(await fs.readFile(filePath, "utf8"));
}

function isNotFoundError(error) {
  return (
    error !== null &&
    typeof error === "object" &&
    "code" in error &&
    error.code === "ENOENT"
  );
}

async function delay(milliseconds) {
  await new Promise((resolve) => setTimeout(resolve, milliseconds));
}
