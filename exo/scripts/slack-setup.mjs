#!/usr/bin/env node

const DEFAULT_PORT = 3939;
const DEFAULT_PATH = "/slack/events";

const PROFILE_DEFINITIONS = {
  mentions: {
    label: "Mentions only",
    summary:
      "Least privilege. Exo only wakes when mentioned in a channel; Slack DMs are disabled.",
    scopes: ["app_mentions:read", "chat:write"],
    events: ["app_mention"],
    trigger: "mentions_only",
    appHomeMessages: false,
  },
  dm: {
    label: "Mentions + DMs",
    summary:
      "Recommended default. Exo wakes on @mentions and direct messages; untagged thread replies are ignored.",
    scopes: ["app_mentions:read", "chat:write", "im:history", "im:write"],
    events: ["app_mention", "message.im"],
    trigger: "mentions_only",
    appHomeMessages: true,
  },
  "public-threads": {
    label: "Public active threads",
    summary:
      "Adds untagged follow-ups in public channel threads after Exo has participated.",
    scopes: [
      "app_mentions:read",
      "channels:history",
      "chat:write",
      "im:history",
      "im:write",
    ],
    events: ["app_mention", "message.channels", "message.im"],
    trigger: "mentions_only",
    appHomeMessages: true,
  },
  "private-threads": {
    label: "Public + private active threads",
    summary:
      "Adds untagged follow-ups in public and private channel threads after Exo has participated.",
    scopes: [
      "app_mentions:read",
      "channels:history",
      "chat:write",
      "groups:history",
      "im:history",
      "im:write",
    ],
    events: ["app_mention", "message.channels", "message.groups", "message.im"],
    trigger: "mentions_only",
    appHomeMessages: true,
  },
  all: {
    label: "All subscribed channel messages",
    summary:
      "Advanced/noisy. Exo wakes on every subscribed public/private channel message; use allowedChannels.",
    scopes: [
      "app_mentions:read",
      "channels:history",
      "chat:write",
      "groups:history",
      "im:history",
      "im:write",
    ],
    events: ["app_mention", "message.channels", "message.groups", "message.im"],
    trigger: "all_messages",
    appHomeMessages: true,
  },
};

const { explicitUrl, profileName, appName, botName } = parseArgs(
  process.argv.slice(2),
);
const profile = PROFILE_DEFINITIONS[profileName];
if (!profile) {
  console.error(`Unknown Slack profile: ${profileName}`);
  console.error(`Use one of: ${Object.keys(PROFILE_DEFINITIONS).join(", ")}`);
  process.exit(1);
}
const publicUrl = explicitUrl ?? (await ngrokPublicUrl());

console.log(`Slack coverage profile: ${profile.label}`);
console.log(profile.summary);
console.log(`Adapter trigger: ${profile.trigger}`);
console.log(`Slack app name: ${appName}`);
console.log(`Slack bot display name: ${botName}`);
console.log("Slack app manifest:\n");
console.log(slackManifest(profile, appName, botName));

if (publicUrl) {
  const normalized = publicUrl.replace(/\/+$/, "");
  console.log("\nSlack Event Subscriptions Request URL:\n");
  console.log(`${normalized}${DEFAULT_PATH}`);
  console.log("\nSubscribe to these bot events:\n");
  for (const event of profile.events) {
    console.log(event);
  }
} else {
  console.log("\nNo ngrok URL detected.");
  console.log(`Run: ngrok http ${DEFAULT_PORT}`);
  console.log(
    `Then rerun: pnpm slack:setup --profile ${profileName} --app-name ${quoteShell(appName)} --bot-name ${quoteShell(botName)} https://YOUR-NGROK-HOST.ngrok-free.app`,
  );
}

console.log("\nAvailable profiles:\n");
for (const [name, definition] of Object.entries(PROFILE_DEFINITIONS)) {
  console.log(`${name}: ${definition.label} - ${definition.summary}`);
}

console.log("\nExo secrets:\n");
console.log("exo secret set slack-signing-secret --value '<signing-secret>'");
console.log("exo secret set slack-bot-token --value 'xoxb-...'");

function parseArgs(args) {
  let profileName = "dm";
  let appName = "Exo Local";
  let botName = "Exo";
  let explicitUrl = null;
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (arg === "--profile") {
      profileName = args[index + 1] ?? "";
      index += 1;
      continue;
    }
    if (arg.startsWith("--profile=")) {
      profileName = arg.slice("--profile=".length);
      continue;
    }
    if (arg === "--app-name") {
      appName = args[index + 1] ?? "";
      index += 1;
      continue;
    }
    if (arg.startsWith("--app-name=")) {
      appName = arg.slice("--app-name=".length);
      continue;
    }
    if (arg === "--bot-name") {
      botName = args[index + 1] ?? "";
      index += 1;
      continue;
    }
    if (arg.startsWith("--bot-name=")) {
      botName = arg.slice("--bot-name=".length);
      continue;
    }
    if (explicitUrl === null) {
      explicitUrl = arg;
      continue;
    }
    throw new Error(`Unexpected argument: ${arg}`);
  }
  if (appName.trim().length === 0) {
    throw new Error("--app-name must not be empty");
  }
  if (botName.trim().length === 0) {
    throw new Error("--bot-name must not be empty");
  }
  return {
    explicitUrl,
    profileName,
    appName: appName.trim(),
    botName: botName.trim(),
  };
}

function quoteShell(value) {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

async function ngrokPublicUrl() {
  try {
    const response = await fetch("http://127.0.0.1:4040/api/tunnels");
    if (!response.ok) {
      return null;
    }
    const payload = await response.json();
    if (!payload || !Array.isArray(payload.tunnels)) {
      return null;
    }
    const tunnel = payload.tunnels.find(
      (candidate) =>
        typeof candidate.public_url === "string" &&
        candidate.public_url.startsWith("https://"),
    );
    return tunnel?.public_url ?? null;
  } catch {
    return null;
  }
}

function slackManifest(profile, appName, botName) {
  const appHome = profile.appHomeMessages
    ? `  app_home:
    home_tab_enabled: false
    messages_tab_enabled: true
    messages_tab_read_only_enabled: false
`
    : "";
  return `display_information:
  name: ${appName}
features:
${appHome}  bot_user:
    display_name: ${botName}
    always_online: false
oauth_config:
  scopes:
    bot:
${profile.scopes.map((scope) => `      - ${scope}`).join("\n")}
settings:
  org_deploy_enabled: false
  socket_mode_enabled: false
  token_rotation_enabled: false`;
}
