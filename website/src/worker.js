const SESSION_PATH_PATTERN = /^\/chat\/(?:s|agent)\/([A-Za-z0-9_-]{8,128})\/?$/;
const WS_PATH_PATTERN = /^\/chat\/ws\/([A-Za-z0-9_-]{8,128})\/?$/;

const SECURITY_HEADERS = {
  "Referrer-Policy": "no-referrer",
  "X-Content-Type-Options": "nosniff",
  "Content-Security-Policy":
    "default-src 'self'; connect-src 'self' wss:; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'",
};

const SESSION_TTL_MS = 30 * 60 * 1000;
const MAX_RELAY_MESSAGE_BYTES = 10 * 1024 * 1024;

// A sender that includes an envelope-level `id` gets replay protection: the
// relay remembers recently accepted ids and answers a repeat with an ack
// instead of a second broadcast. The window is a bounded cache, not a ledger —
// losing it (expiry, session teardown) degrades to at-least-once delivery.
const REPLAY_CACHE_KEY = "replay-cache";
const REPLAY_CACHE_LIMIT = 64;
const REPLAY_CACHE_TTL_MS = 10 * 60 * 1000;

export class RendezvousSession {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.replayLimit = Number(env?.EXOCHAT_REPLAY_LIMIT) || REPLAY_CACHE_LIMIT;
    this.replayTtlMs =
      Number(env?.EXOCHAT_REPLAY_TTL_MS) || REPLAY_CACHE_TTL_MS;
  }

  async fetch(request) {
    const url = new URL(request.url);
    const role = parseRole(url.searchParams.get("role"));

    if (request.headers.get("Upgrade") !== "websocket") {
      return new Response("Expected WebSocket upgrade\n", {
        status: 426,
        headers: SECURITY_HEADERS,
      });
    }

    if (!role) {
      return new Response("Missing or invalid role\n", {
        status: 400,
        headers: SECURITY_HEADERS,
      });
    }

    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);

    for (const socket of this.ctx.getWebSockets()) {
      const attachment = socket.deserializeAttachment();
      if (attachment?.role === role) {
        socket.close(1000, "Replaced by a newer connection");
      }
    }

    this.ctx.acceptWebSocket(server);
    server.serializeAttachment({
      role,
      connectedAt: Date.now(),
    });
    await this.ctx.storage.setAlarm(Date.now() + SESSION_TTL_MS);

    this.broadcastPresence();

    return new Response(null, {
      status: 101,
      webSocket: client,
    });
  }

  async webSocketMessage(socket, message) {
    if (typeof message !== "string") {
      socket.close(1003, "Only text relay messages are supported");
      return;
    }

    if (message.length > MAX_RELAY_MESSAGE_BYTES) {
      socket.close(1009, "Relay message too large");
      return;
    }

    const sender = socket.deserializeAttachment();
    if (!sender?.role) {
      socket.close(1008, "Socket has no role");
      return;
    }

    const sendId = relaySendId(message);
    if (sendId !== null) {
      const cache = await this.replayCache();
      if (cache.has(sendId)) {
        socket.send(sendAck(sendId, true));
        return;
      }
    }

    for (const peer of this.ctx.getWebSockets()) {
      if (peer === socket) {
        continue;
      }

      const recipient = peer.deserializeAttachment();
      if (
        recipient?.role &&
        recipient.role !== sender.role &&
        peer.readyState === WebSocket.OPEN
      ) {
        peer.send(message);
      }
    }

    // Record after the broadcast and ack after the record, so a crash anywhere
    // in between resolves as a retry-and-duplicate, never a silent loss.
    if (sendId !== null) {
      await this.recordSend(sendId);
      socket.send(sendAck(sendId, false));
    }
  }

  webSocketClose() {
    this.broadcastPresence();
  }

  webSocketError() {
    this.broadcastPresence();
  }

  async alarm() {
    for (const socket of this.ctx.getWebSockets()) {
      socket.close(1000, "Session expired");
    }
    await this.ctx.storage.deleteAll();
    this.replaySends = undefined;
  }

  // The in-memory map dies with every hibernation, so accepted ids are written
  // through to durable object storage and rehydrated on the next wake. Expired
  // entries are pruned on read; the storage row dies with the session alarm.
  async replayCache() {
    if (!this.replaySends) {
      const stored = await this.ctx.storage.get(REPLAY_CACHE_KEY);
      this.replaySends = new Map(stored ?? []);
    }
    const cutoff = Date.now() - this.replayTtlMs;
    for (const [id, at] of this.replaySends) {
      if (at < cutoff) {
        this.replaySends.delete(id);
      }
    }
    return this.replaySends;
  }

  async recordSend(id) {
    const cache = await this.replayCache();
    cache.set(id, Date.now());
    while (cache.size > this.replayLimit) {
      cache.delete(cache.keys().next().value);
    }
    await this.ctx.storage.put(REPLAY_CACHE_KEY, [...cache]);
  }

  broadcastPresence() {
    const roles = new Set();
    for (const socket of this.ctx.getWebSockets()) {
      const attachment = socket.deserializeAttachment();
      if (attachment?.role) {
        roles.add(attachment.role);
      }
    }

    const message = JSON.stringify({
      channel: "rendezvous",
      type: "presence",
      roles: [...roles].sort(),
      at: Date.now(),
    });

    for (const socket of this.ctx.getWebSockets()) {
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(message);
      }
    }
  }
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (
      url.pathname === "/wrangler.jsonc" ||
      url.pathname.startsWith("/src/")
    ) {
      return new Response("Not found\n", {
        status: 404,
        headers: SECURITY_HEADERS,
      });
    }

    const wsMatch = url.pathname.match(WS_PATH_PATTERN);
    if (wsMatch) {
      const id = env.RENDEZVOUS.idFromName(wsMatch[1]);
      const stub = env.RENDEZVOUS.get(id);
      return stub.fetch(request);
    }

    if (isSessionPage(url)) {
      const chatUrl = new URL("/chat", url);
      const response = await env.ASSETS.fetch(new Request(chatUrl, request));
      return withSecurityHeaders(response);
    }

    if (url.pathname === "/chat" || url.pathname === "/chat/") {
      return new Response(
        "Create a session with exo/scripts/rendezvous-demo.mjs\n",
        {
          status: 200,
          headers: {
            ...SECURITY_HEADERS,
            "Content-Type": "text/plain; charset=utf-8",
          },
        },
      );
    }

    const response = await env.ASSETS.fetch(request);
    return withSecurityHeaders(response);
  },
};

function isSessionPage(url) {
  if (SESSION_PATH_PATTERN.test(url.pathname)) {
    return true;
  }

  if (url.pathname !== "/chat" && url.pathname !== "/chat/") {
    return false;
  }

  return Boolean(
    url.searchParams.get("c") && parseRole(url.searchParams.get("role")),
  );
}

// Only an `exo.chat` envelope that opted in with a string `id` is inspected;
// everything else stays an opaque string and follows the legacy path
// byte-for-byte. The ciphertext is never touched.
function relaySendId(message) {
  let value;
  try {
    value = JSON.parse(message);
  } catch {
    return null;
  }
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return null;
  }
  if (value.channel !== "exo.chat" || typeof value.id !== "string") {
    return null;
  }
  if (value.id.length < 8 || value.id.length > 128) {
    return null;
  }
  return value.id;
}

function sendAck(id, duplicate) {
  return JSON.stringify({
    channel: "exo.chat.ack",
    id,
    duplicate,
    at: Date.now(),
  });
}

function parseRole(role) {
  if (role === "agent" || role === "user") {
    return role;
  }
  return null;
}

function withSecurityHeaders(response) {
  const headers = new Headers(response.headers);
  for (const [name, value] of Object.entries(SECURITY_HEADERS)) {
    headers.set(name, value);
  }
  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}
