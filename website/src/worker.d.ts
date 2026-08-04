// Hand-written surface for worker.js (the repo compiles with allowJs off) so
// the relay class can be exercised from TypeScript tests.
export type RelaySocket = {
  readyState: number;
  send(message: string): void;
  close(code?: number, reason?: string): void;
  serializeAttachment(value: unknown): void;
  deserializeAttachment(): { role?: string } | undefined;
};

export type RelayContext = {
  getWebSockets(): RelaySocket[];
  acceptWebSocket(socket: RelaySocket): void;
  storage: {
    get(key: string): Promise<unknown>;
    put(key: string, value: unknown): Promise<void>;
    setAlarm(at: number): Promise<void>;
    deleteAll(): Promise<void>;
  };
};

export type RelayEnv = {
  EXOCHAT_REPLAY_LIMIT?: string;
  EXOCHAT_REPLAY_TTL_MS?: string;
};

export class RendezvousSession {
  constructor(ctx: RelayContext, env?: RelayEnv);
  fetch(request: Request): Promise<Response>;
  webSocketMessage(socket: RelaySocket, message: unknown): Promise<void>;
  webSocketClose(): void;
  webSocketError(): void;
  alarm(): Promise<void>;
  broadcastPresence(): void;
}
