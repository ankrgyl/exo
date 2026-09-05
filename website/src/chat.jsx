import * as React from "react";
import { createRoot } from "react-dom/client";
import {
  ActivityIcon,
  CircleIcon,
  CopyIcon,
  HammerIcon,
  LockIcon,
  PlusIcon,
  SendHorizontalIcon,
  WifiIcon,
} from "lucide-react";

import {
  Attachment,
  AttachmentContent,
  AttachmentDescription,
  AttachmentTitle,
} from "@/components/ui/attachment";
import { Avatar, AvatarFallback, AvatarImage } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Bubble, BubbleContent } from "@/components/ui/bubble";
import { Button } from "@/components/ui/button";
import { Marker, MarkerContent, MarkerIcon } from "@/components/ui/marker";
import {
  Message,
  MessageAvatar,
  MessageContent,
  MessageFooter,
  MessageHeader,
} from "@/components/ui/message";
import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerItem,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from "@/components/ui/message-scroller";
import { Textarea } from "@/components/ui/textarea";
import { createReconnectingWebSocket } from "@/reconnecting-websocket";
import "./chat.css";

function ChatApp() {
  const session = React.useMemo(() => readSession(), []);
  const [events, setEvents] = React.useState(() =>
    session.demo ? demoEvents(session.role) : [],
  );
  const [input, setInput] = React.useState("");
  const [composer, setComposer] = React.useState({
    enabled: session.demo,
    label: session.demo ? "Preview" : "Waiting",
  });
  const [status, setStatus] = React.useState({
    data: {
      label: "Crypto",
      tone: session.demo ? "success" : "idle",
      value: session.demo ? "preview" : "locked",
    },
    peer: {
      label: "Peer",
      tone: session.demo ? "success" : "warning",
      value: session.demo ? "demo" : "waiting",
    },
    role: { label: "Role", tone: "success", value: session.role },
    signal: {
      label: "Socket",
      tone: session.demo ? "success" : "warning",
      value: session.demo ? "local" : "connecting",
    },
  });

  const relayKeyRef = React.useRef(null);
  const remoteSeenRef = React.useRef(false);
  const seqRef = React.useRef(0);
  const textareaRef = React.useRef(null);
  const connectionRef = React.useRef(null);

  const addEvent = React.useCallback((event) => {
    setEvents((current) => [
      ...current,
      {
        id: randomId(),
        createdAt: Date.now(),
        ...event,
      },
    ]);
  }, []);

  const addNotice = React.useCallback(
    (text, tone = "neutral") => {
      addEvent({ kind: "notice", text, tone });
    },
    [addEvent],
  );

  const setStatusPart = React.useCallback((key, next) => {
    setStatus((current) => ({
      ...current,
      [key]: {
        ...current[key],
        ...next,
      },
    }));
  }, []);

  React.useEffect(() => {
    const el = textareaRef.current;
    if (!el) {
      return;
    }
    el.style.height = "auto";
    el.style.height = `${Math.min(180, el.scrollHeight)}px`;
  }, [input]);

  React.useEffect(() => {
    if (session.demo) {
      return;
    }

    if (!session.channelId || !session.secret) {
      addNotice(
        "Missing channel id or secret. Start from the URL printed by the demo script.",
        "danger",
      );
      setComposer({ enabled: false, label: "Missing session" });
      return;
    }

    let disposed = false;

    start().catch((error) => {
      addNotice(errorMessage(error), "danger");
      setStatusPart("signal", { tone: "danger", value: "error" });
      setComposer({ enabled: false, label: "Connection error" });
    });

    return () => {
      disposed = true;
      connectionRef.current?.stop();
      connectionRef.current = null;
    };

    async function start() {
      relayKeyRef.current = await deriveRelayKey(
        session.secret,
        session.channelId,
      );
      if (disposed) {
        return;
      }

      setStatusPart("data", { tone: "success", value: "ready" });
      const connection = connectRelay();
      connectionRef.current = connection;
      connection.start();
    }

    function connectRelay() {
      return createReconnectingWebSocket({
        createSocket() {
          const wsUrl = new URL(`/chat/ws/${session.channelId}`, location.href);
          wsUrl.protocol = location.protocol === "https:" ? "wss:" : "ws:";
          wsUrl.searchParams.set("role", session.role);
          return new WebSocket(wsUrl);
        },
        onConnecting({ attempt }) {
          if (attempt === 0) {
            return;
          }
          setStatusPart("signal", {
            tone: "warning",
            value: "reconnecting",
          });
          setStatusPart("peer", { tone: "warning", value: "waiting" });
          setComposer({ enabled: false, label: "Reconnecting" });
        },
        onOpen({ reconnected }) {
          setStatusPart("signal", { tone: "success", value: "open" });
          addNotice(
            reconnected
              ? "Connection restored. Waiting for the other peer..."
              : session.role === "user"
                ? "Waiting for the desktop peer..."
                : "Waiting for the phone peer...",
            reconnected ? "success" : "neutral",
          );
        },
        onReconnectScheduled({ attempt, delayMs }) {
          setStatusPart("signal", {
            tone: "warning",
            value: "reconnecting",
          });
          setStatusPart("peer", { tone: "warning", value: "waiting" });
          setComposer({ enabled: false, label: "Reconnecting" });
          if (attempt === 1) {
            addNotice(
              `Connection lost. Reconnecting in ${formatDelay(delayMs)}...`,
            );
          }
        },
        onTerminalClose() {
          setStatusPart("signal", { tone: "danger", value: "replaced" });
          setStatusPart("peer", { tone: "warning", value: "waiting" });
          setComposer({ enabled: false, label: "Opened elsewhere" });
          addNotice(
            "This session was opened in a newer tab. Continue there instead.",
            "danger",
          );
        },
        onError() {
          console.warn("ExoChat WebSocket error");
        },
        onMessage(event) {
          void handleRelayMessage(event);
        },
      });
    }

    async function handleRelayMessage(event) {
      let message;
      try {
        message = JSON.parse(event.data);
      } catch (error) {
        console.warn("Unable to parse relay message", error);
        addNotice("Rejected a malformed relay message.", "danger");
        return;
      }

      if (!isRecord(message)) {
        addNotice("Rejected a relay message that was not an object.", "danger");
        return;
      }

      if (message.channel === "rendezvous" && message.type === "presence") {
        handlePresence(Array.isArray(message.roles) ? message.roles : []);
        return;
      }

      const frame = await decryptRelayFrame(
        message,
        relayKeyRef.current,
        session.channelId,
      );
      if (!frame) {
        addNotice(
          "Rejected a relay message that could not be decrypted.",
          "danger",
        );
        return;
      }

      renderFrame(frame, false);
    }

    function handlePresence(roles) {
      const remoteRole = session.role === "agent" ? "user" : "agent";
      if (roles.includes(remoteRole)) {
        setStatusPart("peer", { tone: "success", value: "online" });
        setComposer({ enabled: true, label: "Ready" });

        if (!remoteSeenRef.current) {
          addNotice(
            session.role === "agent"
              ? "Phone peer joined. Relay is ready."
              : "Desktop peer joined. Relay is ready.",
            "success",
          );
        }
        remoteSeenRef.current = true;
        return;
      }

      setStatusPart("peer", { tone: "warning", value: "waiting" });
      setComposer({ enabled: false, label: "Waiting" });
      if (remoteSeenRef.current) {
        addNotice("Peer left. Messages are not queued by the relay.");
      }
    }

    function renderFrame(frame, self) {
      if (frame.type === "chat") {
        addEvent({
          frame,
          kind: "message",
          self,
          text: frame.text,
          from: frame.from ?? (self ? session.role : "peer"),
        });
        return;
      }

      if (frame.type === "status" || frame.type === "error") {
        addNotice(
          frame.message ?? frame.text ?? JSON.stringify(frame),
          frame.type === "error" ? "danger" : "neutral",
        );
        return;
      }

      addEvent({
        frame,
        kind: "tool",
        self,
      });
    }
  }, [addEvent, addNotice, session, setStatusPart]);

  const sendChat = React.useCallback(async () => {
    const text = input.trim();
    if (session.demo) {
      if (!text) {
        return;
      }

      const frame = {
        type: "chat",
        id: randomId(),
        from: session.role,
        text,
        createdAt: Date.now(),
      };
      addEvent({
        frame,
        from: session.role,
        kind: "message",
        self: true,
        text,
      });
      setInput("");
      window.setTimeout(() => {
        addEvent({
          frame: {
            type: "chat",
            id: randomId(),
            from: session.role === "agent" ? "user" : "agent",
            text: "This is local UI preview mode. The real rendezvous path sends encrypted frames through the hibernatable websocket relay.",
            createdAt: Date.now(),
          },
          from: session.role === "agent" ? "user" : "agent",
          kind: "message",
          self: false,
          text: "This is local UI preview mode. The real rendezvous path sends encrypted frames through the hibernatable websocket relay.",
        });
      }, 240);
      return;
    }

    const connection = connectionRef.current;
    if (!text || !connection) {
      return;
    }

    const frame = {
      type: "chat",
      id: randomId(),
      from: session.role,
      text,
      createdAt: Date.now(),
    };
    try {
      const envelope = await encryptRelayFrame(
        frame,
        relayKeyRef.current,
        session,
        ++seqRef.current,
      );
      if (!connection.send(JSON.stringify(envelope))) {
        addNotice(
          "The connection changed before this message could be sent. Please try again.",
          "danger",
        );
        return;
      }
    } catch (error) {
      console.warn("Unable to encrypt relay message", error);
      addNotice("Could not encrypt the message for the relay.", "danger");
      return;
    }

    addEvent({
      frame,
      from: session.role,
      kind: "message",
      self: true,
      text,
    });
    setInput("");
  }, [addEvent, addNotice, input, session]);

  const submitComposer = React.useCallback(
    (event) => {
      event.preventDefault();
      void sendChat();
    },
    [sendChat],
  );

  const title =
    session.role === "agent"
      ? "exo chat - desktop peer"
      : "exo chat - phone peer";

  return (
    <div className="chat-shell">
      <header className="topbar">
        <div className="identity">
          <Avatar className="brand-avatar">
            <AvatarImage src="/exo-badge-dark.svg" alt="exo" />
            <AvatarFallback>ex</AvatarFallback>
          </Avatar>
          <div className="identity-copy">
            <h1>{title}</h1>
            <p>session {session.channelId?.slice(0, 8) || "missing"}</p>
          </div>
        </div>

        <div className="status-rail" aria-label="Connection status">
          <StatusBadge icon={CircleIcon} status={status.role} />
          <StatusBadge icon={WifiIcon} status={status.signal} />
          <StatusBadge icon={ActivityIcon} status={status.peer} />
          <StatusBadge icon={LockIcon} status={status.data} />
        </div>
      </header>

      <MessageScrollerProvider autoScroll defaultScrollPosition="end">
        <MessageScroller>
          <MessageScrollerViewport>
            <MessageScrollerContent className="conversation-inner">
              {events.length === 0 ? <EmptyState role={session.role} /> : null}
              {events.map((event) => (
                <MessageScrollerItem
                  key={event.id}
                  messageId={event.id}
                  scrollAnchor={event.kind !== "notice"}
                >
                  <ConversationEvent event={event} />
                </MessageScrollerItem>
              ))}
            </MessageScrollerContent>
          </MessageScrollerViewport>
          <MessageScrollerButton
            aria-label="Scroll to latest"
            direction="end"
          />
        </MessageScroller>
      </MessageScrollerProvider>

      <form className="composer" onSubmit={submitComposer}>
        <div className="composer-surface">
          <Textarea
            ref={textareaRef}
            autoComplete="off"
            className="composer-input"
            disabled={!composer.enabled}
            onChange={(event) => setInput(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                void sendChat();
              }
            }}
            placeholder={composer.enabled ? "Message exo..." : composer.label}
            rows={1}
            value={input}
          />
          <div className="composer-bar">
            <div className="composer-tools">
              <Button
                aria-label="Add attachment or tool"
                className="composer-tool-button"
                disabled={!composer.enabled}
                size="icon"
                title={composer.label}
                type="button"
                variant="outline"
              >
                <PlusIcon />
              </Button>
            </div>
            <Button
              aria-label="Send message"
              className="composer-send-button"
              disabled={!composer.enabled || !input.trim()}
              size="icon"
              type="submit"
            >
              <SendHorizontalIcon />
            </Button>
          </div>
        </div>
      </form>
    </div>
  );
}

function StatusBadge({ icon: Icon, status }) {
  return (
    <Badge
      className={`status-badge status-badge-${status.tone}`}
      variant={status.tone === "idle" ? "secondary" : "outline"}
    >
      <Icon />
      <span>{status.label}</span>
      <strong>{status.value}</strong>
    </Badge>
  );
}

function EmptyState({ role }) {
  return (
    <div className="empty-state">
      <div className="empty-icon">
        <ActivityIcon />
      </div>
      <h2>{role === "agent" ? "Waiting for phone" : "Waiting for desktop"}</h2>
    </div>
  );
}

function ConversationEvent({ event }) {
  if (event.kind === "notice") {
    return (
      <Marker
        className={`notice-marker notice-marker-${event.tone ?? "neutral"}`}
        role={event.tone === "danger" ? "alert" : "status"}
        variant={event.tone === "neutral" ? "separator" : "default"}
      >
        <MarkerIcon>
          <ActivityIcon />
        </MarkerIcon>
        <MarkerContent>{event.text}</MarkerContent>
      </Marker>
    );
  }

  if (event.kind === "tool") {
    return <ToolMessage event={event} />;
  }

  return <ChatMessage event={event} />;
}

function ChatMessage({ event }) {
  const align = event.self ? "end" : "start";
  const from = event.from ?? (event.self ? "user" : "agent");

  return (
    <Message align={align}>
      <MessageAvatar>
        <Avatar>
          <AvatarFallback>{initials(from)}</AvatarFallback>
        </Avatar>
      </MessageAvatar>
      <MessageContent>
        <MessageHeader className="message-meta">
          <span>{labelForRole(from)}</span>
          <time>{formatTime(event.createdAt)}</time>
        </MessageHeader>
        <Bubble align={align} variant={event.self ? "default" : "secondary"}>
          <BubbleContent>
            <RichText text={event.text} />
          </BubbleContent>
        </Bubble>
        <MessageFooter className="message-meta">
          <Button
            aria-label="Copy message"
            onClick={() => copyText(event.text)}
            size="icon-xs"
            type="button"
            variant="ghost"
          >
            <CopyIcon />
          </Button>
          {event.self ? <span className="message-state">sent</span> : null}
        </MessageFooter>
      </MessageContent>
    </Message>
  );
}

function ToolMessage({ event }) {
  const frame = event.frame ?? {};
  const name = frame.name ?? frame.tool ?? frame.type ?? "tool";
  const status = frame.status ?? frame.type ?? "event";

  return (
    <Message align={event.self ? "end" : "start"}>
      <MessageAvatar>
        <Avatar>
          <AvatarFallback>
            <HammerIcon />
          </AvatarFallback>
        </Avatar>
      </MessageAvatar>
      <MessageContent>
        <MessageHeader className="message-meta">
          <span>{name}</span>
          <Badge variant={status === "error" ? "destructive" : "secondary"}>
            {status}
          </Badge>
        </MessageHeader>
        <Attachment className="tool-attachment">
          <AttachmentContent>
            <AttachmentTitle>{name}</AttachmentTitle>
            <AttachmentDescription>tool frame</AttachmentDescription>
            <pre>{formatToolBody(frame)}</pre>
          </AttachmentContent>
        </Attachment>
      </MessageContent>
    </Message>
  );
}

function RichText({ text }) {
  const parts = String(text ?? "").split(/```/g);
  return parts.map((part, index) => {
    if (index % 2 === 1) {
      return <pre key={index}>{stripCodeLanguage(part)}</pre>;
    }
    return part ? <p key={index}>{part}</p> : null;
  });
}

async function deriveRelayKey(secret, channelId) {
  const subtle = getSubtleCrypto();
  const baseKey = await subtle.importKey(
    "raw",
    base64urlToBytes(secret),
    "HKDF",
    false,
    ["deriveKey"],
  );
  return subtle.deriveKey(
    {
      name: "HKDF",
      hash: "SHA-256",
      salt: encode(channelId),
      info: encode("exo-chat-relay:aes-gcm:v1"),
    },
    baseKey,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  );
}

async function encryptRelayFrame(frame, key, session, seq) {
  const subtle = getSubtleCrypto();
  const envelope = {
    channel: "exo.chat",
    channelId: session.channelId,
    ciphertext: "",
    from: session.role,
    nonce: bytesToBase64url(crypto.getRandomValues(new Uint8Array(12))),
    seq,
    version: 1,
  };
  const ciphertext = await subtle.encrypt(
    {
      name: "AES-GCM",
      iv: base64urlToBytes(envelope.nonce),
      additionalData: encode(canonicalEnvelope(envelope)),
    },
    key,
    encode(JSON.stringify(frame)),
  );
  envelope.ciphertext = bytesToBase64url(new Uint8Array(ciphertext));
  return envelope;
}

async function decryptRelayFrame(envelope, key, channelId) {
  if (
    !envelope ||
    envelope.channel !== "exo.chat" ||
    envelope.version !== 1 ||
    envelope.channelId !== channelId ||
    (envelope.from !== "agent" && envelope.from !== "user") ||
    !envelope.nonce ||
    !envelope.ciphertext
  ) {
    return null;
  }

  try {
    const plaintext = await getSubtleCrypto().decrypt(
      {
        name: "AES-GCM",
        iv: base64urlToBytes(envelope.nonce),
        additionalData: encode(canonicalEnvelope(envelope)),
      },
      key,
      base64urlToBytes(envelope.ciphertext),
    );
    const frame = JSON.parse(decode(new Uint8Array(plaintext)));
    return {
      ...frame,
      from: envelope.from,
    };
  } catch (error) {
    console.warn("Unable to decrypt relay message", error);
    return null;
  }
}

function canonicalEnvelope(envelope) {
  return JSON.stringify({
    channel: envelope.channel,
    channelId: envelope.channelId,
    from: envelope.from,
    nonce: envelope.nonce,
    seq: envelope.seq,
    version: envelope.version,
  });
}

function readSession() {
  const params = new URLSearchParams(location.search);
  const pathMatch = location.pathname.match(/^\/chat\/(s|agent)\/([^/]+)\/?$/);
  const role =
    params.get("role") ?? (pathMatch?.[1] === "agent" ? "agent" : "user");

  return {
    channelId: params.get("c") ?? pathMatch?.[2] ?? "",
    demo: params.has("demo"),
    role: role === "agent" ? "agent" : "user",
    secret: readSecret(),
  };
}

function demoEvents(role) {
  const now = Date.now();
  const peer = role === "agent" ? "user" : "agent";

  return [
    {
      id: randomId(),
      createdAt: now - 210000,
      kind: "notice",
      text: "Local UI preview mode. The hibernatable websocket relay is skipped.",
      tone: "success",
    },
    {
      id: randomId(),
      createdAt: now - 180000,
      frame: {
        type: "chat",
        from: peer,
      },
      from: peer,
      kind: "message",
      self: false,
      text: "Can you inspect the build logs and tell me why Cloudflare did not create a preview URL?",
    },
    {
      id: randomId(),
      createdAt: now - 120000,
      frame: {
        name: "cloudflare.deployments.lookup",
        status: "completed",
        type: "tool",
        result: {
          worker: "exo",
          deployCommand: "wrangler versions upload",
          durableObject: "RendezvousSession",
        },
      },
      kind: "tool",
      self: true,
    },
    {
      id: randomId(),
      createdAt: now - 60000,
      frame: {
        type: "chat",
        from: role,
      },
      from: role,
      kind: "message",
      self: true,
      text: "The build uploaded a new Worker version, but did not promote it to active traffic.\n\n```sh\nnpx wrangler deploy -c wrangler.staging.jsonc\n```\n\nFor Durable Objects, use a real staging Worker instead of relying on Cloudflare Preview URLs.",
    },
  ];
}

function readSecret() {
  const hash = new URLSearchParams(location.hash.slice(1));
  return hash.get("k") ?? "";
}

function labelForRole(role) {
  if (role === "agent") {
    return "exo";
  }
  if (role === "user") {
    return "you";
  }
  return role;
}

function initials(role) {
  if (role === "agent") {
    return "EX";
  }
  if (role === "user") {
    return "YO";
  }
  return String(role ?? "??")
    .slice(0, 2)
    .toUpperCase();
}

function formatTime(value) {
  return new Intl.DateTimeFormat(undefined, {
    hour: "numeric",
    minute: "2-digit",
  }).format(value);
}

function formatDelay(delayMs) {
  const seconds = delayMs / 1000;
  return `${seconds} second${seconds === 1 ? "" : "s"}`;
}

function stripCodeLanguage(value) {
  return value.replace(/^[a-zA-Z0-9_-]+\n/, "");
}

function formatToolBody(frame) {
  const body =
    frame.output ?? frame.result ?? frame.arguments ?? frame.input ?? frame;
  if (typeof body === "string") {
    return body;
  }
  return JSON.stringify(body, null, 2);
}

function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

async function copyText(value) {
  try {
    await navigator.clipboard.writeText(String(value ?? ""));
  } catch (error) {
    console.warn("Unable to copy message", error);
  }
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}

function getSubtleCrypto() {
  if (globalThis.crypto?.subtle) {
    return globalThis.crypto.subtle;
  }

  throw new Error(
    "Encrypted relay requires HTTPS or localhost. Plain LAN HTTP disables Web Crypto in this browser.",
  );
}

function randomId() {
  if (globalThis.crypto?.randomUUID) {
    return globalThis.crypto.randomUUID();
  }

  return `id-${bytesToBase64url(
    globalThis.crypto.getRandomValues(new Uint8Array(16)),
  )}`;
}

function encode(value) {
  return new TextEncoder().encode(value);
}

function decode(bytes) {
  return new TextDecoder().decode(bytes);
}

function bytesToBase64url(bytes) {
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary)
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");
}

function base64urlToBytes(value) {
  const padded = value
    .replaceAll("-", "+")
    .replaceAll("_", "/")
    .padEnd(Math.ceil(value.length / 4) * 4, "=");
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

const rootElement = document.querySelector("#root");
rootElement.__exoChatRoot ??= createRoot(rootElement);
rootElement.__exoChatRoot.render(<ChatApp />);
