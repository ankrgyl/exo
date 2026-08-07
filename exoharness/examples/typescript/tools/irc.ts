// Demo library tool for the TypeScript harness tool system. This example shows
// how a networked service integration can expose typed tool configuration from
// a TypeScript module. It is not exposed by any example harness by default.

import net from "node:net";
import tls from "node:tls";

import type {
  JsonObject,
  Tool,
  ToolModule,
  ToolResult,
  TurnContext,
} from "@exo/harness/tool";

interface IrcConfig {
  server: string;
  port: number;
  nick: string;
  username: string;
  realname: string;
  tls: boolean;
  dryRun: boolean;
  passwordSecretId: string | null;
}

export const ircTool = {
  definition: {
    name: "irc_send_message",
    description: "Send a message to an IRC channel.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        channel: {
          type: "string",
          description: "IRC channel name, for example #exo.",
        },
        text: {
          type: "string",
          description: "Message text to send.",
        },
      },
      required: ["channel", "text"],
    },
    outputSchema: {
      type: "object",
      additionalProperties: false,
      properties: {
        ok: { type: "boolean" },
        dryRun: { type: "boolean" },
        registered: { type: "boolean" },
        joined: { type: "boolean" },
        server: { type: "string" },
        channel: { type: "string" },
      },
      required: ["ok", "dryRun", "registered", "joined", "server", "channel"],
    },
  },
  initializationParameters: {
    type: "object",
    additionalProperties: false,
    properties: {
      server: {
        type: "string",
        description: "IRC server hostname.",
      },
      port: {
        type: "number",
        description: "IRC server port.",
      },
      nick: {
        type: "string",
        description: "Nickname to use for the IRC connection.",
      },
      username: {
        type: "string",
        description: "Username to send in the IRC USER command.",
      },
      realname: {
        type: "string",
        description: "Real name to send in the IRC USER command.",
      },
      tls: {
        type: "boolean",
        description: "Whether to connect with TLS.",
      },
      dryRun: {
        type: "boolean",
        description: "If true, build IRC commands without opening a socket.",
      },
      passwordSecretId: {
        type: ["string", "null"],
        description: "Optional secret id containing an IRC server password.",
      },
    },
    required: ["server", "port", "nick", "username", "realname", "tls"],
  },
  initialize(args) {
    const config = parseConfig(args);
    return {
      async execute(args, execution): Promise<ToolResult> {
        return sendIrcMessage(execution.context, config, args);
      },
    };
  },
} satisfies Tool;

export default {
  tools: [
    {
      tool: ircTool,
      initialization: {
        server: "irc.libera.chat",
        port: 6697,
        nick: "exo-demo",
        username: "exo-demo",
        realname: "Exo Demo",
        tls: true,
        dryRun: false,
        passwordSecretId: null,
      },
    },
  ],
} satisfies ToolModule;

async function sendIrcMessage(
  context: TurnContext,
  config: IrcConfig,
  args: JsonObject,
): Promise<ToolResult> {
  const channel = stringArgument(args, "channel");
  const text = stringArgument(args, "text");
  const password = await resolvePassword(context, config.passwordSecretId);
  let registered = false;
  let joined = false;

  if (!config.dryRun) {
    await withIrcConnection(config, async (socket) => {
      const session = await registerJoinAndSend(
        socket,
        config,
        channel,
        text,
        password,
      );
      registered = session.registered;
      joined = session.joined;
    });
  }

  return {
    ok: true,
    dryRun: config.dryRun,
    registered,
    joined,
    server: config.server,
    channel,
  };
}

interface IrcSessionResult {
  registered: boolean;
  joined: boolean;
}

async function registerJoinAndSend(
  socket: net.Socket,
  config: IrcConfig,
  channel: string,
  text: string,
  password: string | null,
): Promise<IrcSessionResult> {
  const reader = new IrcLineReader(socket);
  if (password) {
    writeIrcCommand(socket, `PASS ${password}`);
  }
  writeIrcCommand(socket, `NICK ${config.nick}`);
  writeIrcCommand(socket, `USER ${config.username} 0 * :${config.realname}`);

  await reader.waitFor(
    (line) => line.includes(` 001 ${config.nick} `),
    "IRC registration welcome",
  );
  writeIrcCommand(socket, `JOIN ${channel}`);
  await reader.waitFor(
    (line) =>
      line.includes(` JOIN ${channel}`) ||
      line.includes(` JOIN :${channel}`) ||
      line.includes(` 366 ${config.nick} ${channel} `),
    `JOIN confirmation for ${channel}`,
  );
  writeIrcCommand(socket, `PRIVMSG ${channel} :${text}`);
  await sleep(500);
  writeIrcCommand(socket, "QUIT");
  return { registered: true, joined: true };
}

function writeIrcCommand(socket: net.Socket, command: string): void {
  socket.write(`${command}\r\n`);
}

class IrcLineReader {
  private buffer = "";
  private readonly lines: string[] = [];
  private waiter: IrcLineWaiter | null = null;

  constructor(private readonly socket: net.Socket) {
    socket.on("data", (chunk) => this.handleData(chunk.toString()));
  }

  waitFor(
    predicate: (line: string) => boolean,
    description: string,
    timeoutMs = 10_000,
  ): Promise<string> {
    const existingIndex = this.lines.findIndex(predicate);
    if (existingIndex >= 0) {
      const [line] = this.lines.splice(existingIndex, 1);
      if (line === undefined) {
        throw new Error("IRC line reader invariant violated");
      }
      return Promise.resolve(line);
    }
    if (this.waiter) {
      return Promise.reject(new Error("IRC line reader already has a waiter"));
    }

    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.waiter = null;
        reject(new Error(`Timed out waiting for ${description}`));
      }, timeoutMs);
      this.waiter = { predicate, resolve, reject, timeout };
    });
  }

  private handleData(data: string): void {
    this.buffer += data;
    for (;;) {
      const newlineIndex = this.buffer.indexOf("\n");
      if (newlineIndex === -1) {
        return;
      }
      const line = this.buffer.slice(0, newlineIndex).replace(/\r$/, "");
      this.buffer = this.buffer.slice(newlineIndex + 1);
      this.handleLine(line);
    }
  }

  private handleLine(line: string): void {
    const pingToken = line.match(/^PING :(.+)$/)?.[1];
    if (pingToken) {
      writeIrcCommand(this.socket, `PONG :${pingToken}`);
    }

    if (!this.waiter) {
      this.lines.push(line);
      return;
    }
    if (!this.waiter.predicate(line)) {
      this.lines.push(line);
      return;
    }

    const waiter = this.waiter;
    this.waiter = null;
    clearTimeout(waiter.timeout);
    waiter.resolve(line);
  }
}

interface IrcLineWaiter {
  predicate: (line: string) => boolean;
  resolve: (line: string) => void;
  reject: (error: Error) => void;
  timeout: ReturnType<typeof setTimeout>;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function resolvePassword(
  context: TurnContext,
  secretId: string | null,
): Promise<string | null> {
  if (!secretId) {
    return null;
  }
  const secret =
    await context.exoharness.current.conversation.getSecret(secretId);
  if (!secret) {
    throw new Error(`IRC password secret does not exist: ${secretId}`);
  }
  if (secret.type !== "key") {
    throw new Error("IRC password secret must be a key secret");
  }
  return secret.value;
}

async function withIrcConnection(
  config: IrcConfig,
  run: (socket: net.Socket) => Promise<void> | void,
): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const socket = config.tls
      ? tls.connect(config.port, config.server)
      : net.connect(config.port, config.server);
    socket.setEncoding("utf8");
    socket.setTimeout(10_000);
    socket.once("connect", async () => {
      try {
        await run(socket);
        socket.end(resolve);
      } catch (error) {
        socket.destroy();
        reject(error);
      }
    });
    socket.once("error", reject);
    socket.once("timeout", () => {
      socket.destroy(new Error("IRC connection timed out"));
    });
  });
}

function parseConfig(args: JsonObject): IrcConfig {
  return {
    server: stringArgument(args, "server"),
    port: numberArgument(args, "port"),
    nick: stringArgument(args, "nick"),
    username: stringArgument(args, "username"),
    realname: stringArgument(args, "realname"),
    tls: booleanArgument(args, "tls"),
    dryRun: optionalBooleanArgument(args, "dryRun") ?? false,
    passwordSecretId: optionalStringArgument(args, "passwordSecretId"),
  };
}

function stringArgument(args: JsonObject, name: string): string {
  const value = args[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`IRC tool argument ${name} must be a non-empty string`);
  }
  return value;
}

function optionalStringArgument(args: JsonObject, name: string): string | null {
  const value = args[name];
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`IRC tool argument ${name} must be a non-empty string`);
  }
  return value;
}

function numberArgument(args: JsonObject, name: string): number {
  const value = args[name];
  if (typeof value !== "number") {
    throw new Error(`IRC tool argument ${name} must be a number`);
  }
  return value;
}

function booleanArgument(args: JsonObject, name: string): boolean {
  const value = args[name];
  if (typeof value !== "boolean") {
    throw new Error(`IRC tool argument ${name} must be a boolean`);
  }
  return value;
}

function optionalBooleanArgument(
  args: JsonObject,
  name: string,
): boolean | null {
  const value = args[name];
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "boolean") {
    throw new Error(`IRC tool argument ${name} must be a boolean`);
  }
  return value;
}
