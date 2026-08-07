"""PyBoy sidecar: a Game Boy emulator behind a tiny JSON-over-HTTP API.

Runs the game headless on localhost. The game only advances when the caller
asks it to (press/tick), so the world is effectively turn-based and
deterministic between agent actions.

Endpoints:
  GET  /health                     -> {ok, rom, frame_count}
  GET  /frame                      -> {screenshot_b64, state, screen_hash}
  POST /press {buttons, hold_frames?, wait_frames?}
  POST /tick  {frames}
  POST /checkpoint/save {name}
  POST /checkpoint/load {name}
  GET  /checkpoints
  POST /reset
  POST /status {text}              -> free-text status line shown in /view
  GET  /view                       -> minimal live viewer (browser page)

All mutating endpoints return the same payload as GET /frame so callers see
consequences immediately. stdlib http.server only; no web framework.

The only game-specific piece is memory_map.read_state (Pokemon Red/Blue RAM
decoding); swap that module out to target a different game.

For this to run, you need to provide a Game Boy ROM file (e.g. Pokemon Red/Blue). 
You can find these online by searching for "pokemon red rom" on Google, it will
be small, approximately 370KB.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import json
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from pyboy import PyBoy

import memory_map

VALID_BUTTONS = {"a", "b", "start", "select", "up", "down", "left", "right"}
DEFAULT_HOLD_FRAMES = 10
DEFAULT_WAIT_FRAMES = 45
MAX_BUTTONS_PER_PRESS = 20
MAX_TICK_FRAMES = 60 * 60  # one minute of game time
SCREEN_UPSCALE = 3  # 160x144 -> 480x432 so the model can read sprite text
CHECKPOINT_NAME_RE = re.compile(r"^[A-Za-z0-9_.-]{1,64}$")

# Minimal live viewer: polls /frame once a second. Served at GET /view.
VIEW_HTML = b"""<!doctype html>
<title>gameboy agent</title>
<body style="margin:0;min-height:100vh;display:flex;flex-direction:column;\
align-items:center;justify-content:center;background:#111;color:#9ca3af;\
font:14px/1.6 ui-monospace,monospace">
<img id=s width=480 height=432 style="image-rendering:pixelated;background:#000">
<div id=t style="color:#e5e7eb;margin-top:8px">&nbsp;</div>
<pre id=p style="margin:4px 0 0;text-align:center"></pre>
<script>
setInterval(async () => {
  try {
    const f = await (await fetch("/frame")).json();
    const st = f.state;
    document.getElementById("s").src = "data:image/png;base64," + f.screenshot_b64;
    document.getElementById("t").textContent = f.status || "(waiting for the agent)";
    document.getElementById("p").textContent =
      `${st.map_name} (${st.x},${st.y})  badges: ${st.badge_count}  $${st.money}\\n` +
      (st.party.map(m => `${m.species} L${m.level} ${m.hp}/${m.max_hp}hp`).join(", ") || "no pokemon") +
      `\\nframe ${f.frame_count}`;
  } catch {}
}, 1000);
</script>"""


class Emulator:
    def __init__(self, rom_path: Path, checkpoint_dir: Path) -> None:
        self.rom_path = rom_path
        self.checkpoint_dir = checkpoint_dir
        self.checkpoint_dir.mkdir(parents=True, exist_ok=True)
        self.status = ""  # free-text line set by the agent side, shown in /view
        self.pyboy = self._boot()

    def _boot(self) -> PyBoy:
        pyboy = PyBoy(str(self.rom_path), window="null", sound_emulated=False)
        pyboy.set_emulation_speed(0)  # unbounded; the API paces the game
        pyboy.tick(120, render=True)  # let the boot logo settle
        return pyboy

    def reset(self) -> None:
        self.pyboy.stop(save=False)
        self.pyboy = self._boot()

    def press(self, buttons: list[str], hold: int, wait: int) -> None:
        for button in buttons:
            self.pyboy.button_press(button)
            self.pyboy.tick(hold, render=False)
            self.pyboy.button_release(button)
            # Release must be observed before the next press of the same
            # button registers, and movement/menus need settle time.
            self.pyboy.tick(wait, render=False)
        self.pyboy.tick(1, render=True)

    def tick(self, frames: int) -> None:
        self.pyboy.tick(frames, render=True)

    def frame_payload(self) -> dict:
        image = self.pyboy.screen.image.convert("RGB")
        screen_hash = hashlib.sha256(image.tobytes()).hexdigest()[:16]
        upscaled = image.resize(
            (image.width * SCREEN_UPSCALE, image.height * SCREEN_UPSCALE),
            resample=0,  # nearest neighbor keeps pixel text crisp
        )
        buffer = io.BytesIO()
        upscaled.save(buffer, format="PNG")
        return {
            "screenshot_b64": base64.b64encode(buffer.getvalue()).decode("ascii"),
            "state": memory_map.read_state(self.pyboy.memory),
            "screen_hash": screen_hash,
            "frame_count": self.pyboy.frame_count,
            "status": self.status,
        }

    def checkpoint_path(self, name: str) -> Path:
        if not CHECKPOINT_NAME_RE.match(name):
            raise ValueError(
                "checkpoint name must match [A-Za-z0-9_.-]{1,64}",
            )
        return self.checkpoint_dir / f"{name}.state"

    def save_checkpoint(self, name: str) -> None:
        # Recreate in case the directory was cleaned while running.
        self.checkpoint_dir.mkdir(parents=True, exist_ok=True)
        with open(self.checkpoint_path(name), "wb") as handle:
            self.pyboy.save_state(handle)

    def load_checkpoint(self, name: str) -> None:
        path = self.checkpoint_path(name)
        if not path.exists():
            raise ValueError(f"no checkpoint named {name!r}")
        with open(path, "rb") as handle:
            self.pyboy.load_state(handle)
        self.pyboy.tick(1, render=True)

    def list_checkpoints(self) -> list[str]:
        return sorted(path.stem for path in self.checkpoint_dir.glob("*.state"))


def clamp_frames(value: object, default: int, maximum: int) -> int:
    if value is None:
        return default
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise ValueError("frame counts must be positive integers")
    return min(value, maximum)


def make_handler(emulator: Emulator):
    # The server is threaded (a /view browser tab holds a keep-alive
    # connection open, which would starve a single-threaded server), but
    # PyBoy is not thread-safe — serialize all emulator access.
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, format: str, *args) -> None:
            pass  # keep stdout clean; errors surface in responses

        def _respond(self, status: int, payload: dict) -> None:
            body = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _read_json(self) -> dict:
            length = int(self.headers.get("Content-Length") or 0)
            if length == 0:
                return {}
            payload = json.loads(self.rfile.read(length))
            if not isinstance(payload, dict):
                raise ValueError("request body must be a JSON object")
            return payload

        def do_GET(self) -> None:  # noqa: N802 (stdlib naming)
            if self.path == "/view":  # static; no emulator access, no lock
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(VIEW_HTML)))
                self.end_headers()
                self.wfile.write(VIEW_HTML)
                return
            try:
                with lock:
                    if self.path == "/health":
                        self._respond(
                            200,
                            {
                                "ok": True,
                                "rom": emulator.rom_path.name,
                                "frame_count": emulator.pyboy.frame_count,
                            },
                        )
                    elif self.path == "/frame":
                        self._respond(200, emulator.frame_payload())
                    elif self.path == "/checkpoints":
                        self._respond(
                            200, {"checkpoints": emulator.list_checkpoints()}
                        )
                    else:
                        self._respond(404, {"error": f"unknown path {self.path}"})
            except Exception as error:  # noqa: BLE001 - report to caller
                self._respond(500, {"error": str(error)})

        def do_POST(self) -> None:  # noqa: N802 (stdlib naming)
            try:
                body = self._read_json()
                with lock:
                    if self.path == "/press":
                        buttons = body.get("buttons")
                        if (
                            not isinstance(buttons, list)
                            or len(buttons) == 0
                            or len(buttons) > MAX_BUTTONS_PER_PRESS
                            or any(b not in VALID_BUTTONS for b in buttons)
                        ):
                            raise ValueError(
                                f"buttons must be 1-{MAX_BUTTONS_PER_PRESS} of "
                                f"{sorted(VALID_BUTTONS)}"
                            )
                        hold = clamp_frames(
                            body.get("hold_frames"), DEFAULT_HOLD_FRAMES, 120
                        )
                        wait = clamp_frames(
                            body.get("wait_frames"), DEFAULT_WAIT_FRAMES, 600
                        )
                        emulator.press(buttons, hold, wait)
                    elif self.path == "/tick":
                        emulator.tick(
                            clamp_frames(body.get("frames"), 60, MAX_TICK_FRAMES)
                        )
                    elif self.path == "/checkpoint/save":
                        emulator.save_checkpoint(str(body.get("name") or ""))
                    elif self.path == "/checkpoint/load":
                        emulator.load_checkpoint(str(body.get("name") or ""))
                    elif self.path == "/status":
                        emulator.status = str(body.get("text") or "")[:200]
                    elif self.path == "/reset":
                        emulator.reset()
                    else:
                        self._respond(404, {"error": f"unknown path {self.path}"})
                        return
                    self._respond(200, emulator.frame_payload())
            except ValueError as error:
                self._respond(400, {"error": str(error)})
            except Exception as error:  # noqa: BLE001 - report to caller
                self._respond(500, {"error": str(error)})

    return Handler


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rom", required=True, type=Path)
    parser.add_argument("--port", type=int, default=8777)
    parser.add_argument(
        "--host",
        default="127.0.0.1",
        help="bind address; use 0.0.0.0 to watch /view from another machine",
    )
    parser.add_argument(
        "--checkpoint-dir",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "checkpoints",
    )
    args = parser.parse_args()

    if not args.rom.exists():
        print(f"ROM not found: {args.rom}", file=sys.stderr)
        sys.exit(1)

    emulator = Emulator(args.rom, args.checkpoint_dir)
    server = ThreadingHTTPServer((args.host, args.port), make_handler(emulator))
    print(f"emulator ready on http://{args.host}:{args.port} rom={args.rom.name}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        emulator.pyboy.stop(save=False)


if __name__ == "__main__":
    main()
