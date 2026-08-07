# Game Boy agent (Pokemon Red/Blue)

The minimal setup for letting an **exo agent** play a Game Boy game: a
headless emulator behind a tiny HTTP API, a TypeScript client for it, and a
handful of tools that let the model press buttons and see what happened. The
agent runs as an exo TypeScript harness, so exo owns the turn loop,
conversation history, and event log — this example only supplies the game
semantics.

```
┌─────────────┐  tool calls   ┌──────────────────┐  HTTP (localhost)  ┌─────────────────┐
│  model      │ ────────────► │ exo turn loop    │ ─────────────────► │ emulator        │
│  (vision +  │ ◄──────────── │ + harness.ts     │ ◄───────────────── │ server.py       │
│  tools)     │  screen +     │ (game-tools.ts)  │  screenshot + RAM  │ (PyBoy sidecar) │
└─────────────┘  RAM state    └──────────────────┘  state per action  └─────────────────┘
```

Four pieces do all the work:

- **`emulator/server.py`** — runs the game headless with
  [PyBoy](https://github.com/Baekalfen/PyBoy) and exposes it as
  JSON-over-HTTP (`/press`, `/tick`, `/frame`, checkpoints). The game only
  advances when asked, so the world is turn-based and deterministic between
  actions. `emulator/memory_map.py` decodes Pokemon Red/Blue RAM into
  objective state (position, party, badges, money) — the one game-specific
  file.
- **`agent/emulator-client.ts`** — typed TypeScript client for that API.
- **`agent/game-tools.ts`** — the tool definitions the model sees, as exo
  `Tool` objects: `press_buttons`, `wait`, and save/load/list checkpoints.
- **`agent/harness.ts`** — the exo harness. `registerTools` wires in the game
  tools; `instructions` injects the current screenshot + RAM state on **every
  model round**, so the model always
  acts on the current screen and stale frames never pile up in history.

## Run it

**Prerequisites:** python3; the exo CLI built (`cargo build -p exo`); Node +
pnpm (`pnpm install` at the repo root); `OPENAI_API_KEY`; and a Pokemon
Red/Blue ROM. ROMs are copyrighted and gitignored — supply your own dump
under `roms/`.

**1. Start the emulator sidecar** (leave it running):

```bash
cd exoharness/examples/gameboy-agent
mkdir -p roms && cp /path/to/pokemon-red.gb roms/
./run.sh                       # boots PyBoy on http://127.0.0.1:8777
```

**2. Create the agent and play** (from the repo root, in another terminal):

```bash
exo secret set openai --env OPENAI_API_KEY
exo model register gpt-5.5 --secret openai

exo --harness typescript agent create "Gameboy" \
  --module exoharness/examples/gameboy-agent/agent/harness.ts \
  --model gpt-5.5 --max-tool-round-trips 20
exo conversation create gameboy "Play Pokemon"
exo conversation send gameboy play "Play Pokemon Red. Get through the intro and pick a starter."
```

Each `send` runs one exo turn: the harness feeds the model the live screen,
the model calls `press_buttons` / `wait` / checkpoint tools until it stops,
and its closing summary lands in the conversation. Send again to continue —
prior summaries are already in history.

**Watch it play** at <http://127.0.0.1:8777/view> — a page served by the
sidecar itself that shows the live screen, the current model round, and the
RAM state, refreshing once a second.

Point the harness at a sidecar on another host/port with
`GAMEBOY_EMULATOR_URL`.

## Adapting to another game

1. Any Game Boy / Game Boy Color ROM works with `server.py` as-is; only
   `memory_map.read_state` is Pokemon-specific. Replace it with a RAM decode
   for your game (community symbol maps exist for most classics) or return
   `{}` and let the model play from the screenshot alone.
2. Update `GameState`/`describeState` in `agent/emulator-client.ts` to match.
3. Rewrite `SYSTEM_PROMPT` in `agent/harness.ts`.

For other consoles, keep the API shape (`/press`, `/tick`, `/frame`) and swap
PyBoy for another scriptable emulator core.

See [the tutorial](../../website/docs-src/tutorials/game-emulator-integration.md)
for a walkthrough of the design.
