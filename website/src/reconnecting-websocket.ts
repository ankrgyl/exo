const OPEN_READY_STATE = 1;
const INITIAL_RECONNECT_DELAY_MS = 1_000;
const MAX_RECONNECT_DELAY_MS = 30_000;
const REPLACED_CONNECTION_REASON = "Replaced by a newer connection";

type ConnectionAttempt = {
  attempt: number;
};

type OpenedConnection = {
  reconnected: boolean;
};

type ScheduledReconnect = {
  attempt: number;
  delayMs: number;
  event: CloseEvent;
};

type ReconnectingWebSocketOptions = {
  createSocket: () => WebSocket;
  onConnecting: (connection: ConnectionAttempt) => void;
  onError: (event: Event) => void;
  onMessage: (event: MessageEvent) => void;
  onOpen: (connection: OpenedConnection) => void;
  onReconnectScheduled: (reconnect: ScheduledReconnect) => void;
  onTerminalClose: (event: CloseEvent) => void;
};

export type ReconnectingWebSocket = {
  send: (data: string) => boolean;
  start: () => void;
  stop: () => void;
};

export function createReconnectingWebSocket({
  createSocket,
  onConnecting,
  onError,
  onMessage,
  onOpen,
  onReconnectScheduled,
  onTerminalClose,
}: ReconnectingWebSocketOptions): ReconnectingWebSocket {
  let reconnectAttempt = 0;
  let reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let socket: WebSocket | null = null;
  let stopped = true;

  function connect() {
    if (stopped || socket || reconnectTimer) {
      return;
    }

    onConnecting({ attempt: reconnectAttempt });
    const nextSocket = createSocket();
    socket = nextSocket;

    nextSocket.addEventListener("open", () => {
      if (stopped || socket !== nextSocket) {
        return;
      }

      const reconnected = reconnectAttempt > 0;
      reconnectAttempt = 0;
      reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;
      onOpen({ reconnected });
    });

    nextSocket.addEventListener("message", (event) => {
      if (!stopped && socket === nextSocket) {
        onMessage(event);
      }
    });

    nextSocket.addEventListener("error", (event) => {
      if (!stopped && socket === nextSocket) {
        onError(event);
      }
    });

    nextSocket.addEventListener("close", (event) => {
      if (stopped || socket !== nextSocket) {
        return;
      }

      socket = null;
      if (!shouldReconnect(event)) {
        onTerminalClose(event);
        return;
      }

      const delayMs = reconnectDelayMs;
      reconnectDelayMs = Math.min(reconnectDelayMs * 2, MAX_RECONNECT_DELAY_MS);
      reconnectAttempt += 1;
      onReconnectScheduled({
        attempt: reconnectAttempt,
        delayMs,
        event,
      });
      reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        connect();
      }, delayMs);
    });
  }

  return {
    send(data) {
      if (!socket || socket.readyState !== OPEN_READY_STATE) {
        return false;
      }
      socket.send(data);
      return true;
    },
    start() {
      if (!stopped) {
        return;
      }
      stopped = false;
      connect();
    },
    stop() {
      if (stopped) {
        return;
      }
      stopped = true;
      if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      const activeSocket = socket;
      socket = null;
      activeSocket?.close();
      reconnectAttempt = 0;
      reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;
    },
  };
}

function shouldReconnect(event: CloseEvent) {
  return !(event.code === 1000 && event.reason === REPLACED_CONNECTION_REASON);
}
