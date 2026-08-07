// HTTP client for the PyBoy sidecar (emulator/server.py), customized for
// Pokemon Red/Blue. The sidecar must already be running.

export interface PartyMon {
  species: string;
  level: number;
  hp: number;
  max_hp: number;
}

export interface GameState {
  map_id: number;
  map_name: string;
  x: number;
  y: number;
  facing: string;
  in_battle: "none" | "wild" | "trainer" | "lost";
  badges: string[];
  badge_count: number;
  money: number;
  party: PartyMon[];
  pokedex_owned: number;
}

export interface FramePayload {
  screenshot_b64: string;
  state: GameState;
  screen_hash: string;
  frame_count: number;
  status: string;
}

export class EmulatorClient {
  private readonly baseUrl: string;

  constructor(baseUrl: string) {
    this.baseUrl = baseUrl;
  }

  async health(): Promise<{ ok: boolean; rom: string }> {
    return await this.request("GET", "/health");
  }

  async frame(): Promise<FramePayload> {
    return await this.request("GET", "/frame");
  }

  async press(
    buttons: string[],
    holdFrames?: number | null,
    waitFrames?: number | null,
  ): Promise<FramePayload> {
    return await this.request("POST", "/press", {
      buttons,
      hold_frames: holdFrames ?? undefined,
      wait_frames: waitFrames ?? undefined,
    });
  }

  async tick(frames: number): Promise<FramePayload> {
    return await this.request("POST", "/tick", { frames });
  }

  async saveCheckpoint(name: string): Promise<FramePayload> {
    return await this.request("POST", "/checkpoint/save", { name });
  }

  async loadCheckpoint(name: string): Promise<FramePayload> {
    return await this.request("POST", "/checkpoint/load", { name });
  }

  // Sets the status line shown by the sidecar's /view page. Cosmetic only.
  async setStatus(text: string): Promise<void> {
    await this.request("POST", "/status", { text });
  }

  async listCheckpoints(): Promise<string[]> {
    const payload = await this.request<{ checkpoints: string[] }>(
      "GET",
      "/checkpoints",
    );
    return payload.checkpoints;
  }

  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<T> {
    const response = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers: body === undefined ? {} : { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(60_000),
    });
    const payload = (await response.json()) as T & { error?: string };
    if (!response.ok) {
      throw new Error(
        `emulator ${method} ${path} failed (${response.status}): ${payload?.error ?? "unknown error"}`,
      );
    }
    return payload;
  }
}

export function describeState(state: GameState): string {
  const party =
    state.party.length === 0
      ? "no pokemon"
      : state.party
          .map(
            (mon) => `${mon.species} L${mon.level} ${mon.hp}/${mon.max_hp}hp`,
          )
          .join(", ");
  return [
    `location: ${state.map_name} (map 0x${state.map_id.toString(16)}) at (${state.x},${state.y}) facing ${state.facing}`,
    `battle: ${state.in_battle}`,
    `party: ${party}`,
    `badges: ${state.badge_count} [${state.badges.join(", ")}]`,
    `money: $${state.money}  pokedex owned: ${state.pokedex_owned}`,
  ].join("\n");
}
