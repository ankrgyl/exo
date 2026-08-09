"""Typed protocol for submitting a containerized trial to Exo."""

from __future__ import annotations

import asyncio
import json
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Literal


@dataclass(frozen=True)
class TrialRun:
    request_id: str
    target: str
    container_id: str
    instructions: str
    deadline_at: str | None = None
    type: Literal["trial_run"] = "trial_run"

    def payload(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class TrialComplete:
    request_id: str
    target: str
    conversation_id: str
    summary: str | None = None


class TrialAdapterError(RuntimeError):
    """The trial adapter rejected a request or returned an invalid response."""


class _Disconnected(RuntimeError):
    """The adapter socket disappeared while a trial was running."""


async def send_trial_run(
    socket_path: Path,
    request: TrialRun,
    *,
    timeout_sec: float | None,
) -> TrialComplete:
    """Wait for explicit completion, reconnecting across Exo restarts."""

    async def wait_for_completion() -> TrialComplete:
        while True:
            try:
                event = await _exchange(socket_path, request.payload())
                return _parse_completion(event, request)
            except (OSError, _Disconnected):
                await asyncio.sleep(0.5)

    return await asyncio.wait_for(wait_for_completion(), timeout=timeout_sec)


async def probe(socket_path: Path, *, timeout_sec: float) -> bool:
    """Return True once the adapter socket accepts a connection."""
    deadline = asyncio.get_running_loop().time() + timeout_sec
    while True:
        try:
            _, writer = await asyncio.open_unix_connection(str(socket_path))
            writer.close()
            await writer.wait_closed()
            return True
        except (OSError, asyncio.TimeoutError):
            if asyncio.get_running_loop().time() >= deadline:
                return False
            await asyncio.sleep(0.5)


async def _exchange(socket_path: Path, payload: dict[str, Any]) -> dict[str, Any]:
    reader, writer = await asyncio.open_unix_connection(str(socket_path))
    try:
        writer.write(f"{json.dumps(payload)}\n".encode())
        await writer.drain()
        line = await reader.readline()
    finally:
        writer.close()
        await writer.wait_closed()

    if not line:
        raise _Disconnected("trial adapter closed without replying")
    try:
        message = json.loads(line)
    except json.JSONDecodeError as error:
        raise TrialAdapterError("trial adapter returned invalid JSON") from error
    if not isinstance(message, dict):
        raise TrialAdapterError("trial adapter reply must be a JSON object")
    if message.get("type") == "error":
        raise TrialAdapterError(str(message.get("message")))
    if message.get("type") != "response" or not isinstance(message.get("event"), dict):
        raise TrialAdapterError(f"unexpected trial adapter reply: {message!r}")
    return message["event"]


def _parse_completion(
    event: dict[str, Any], request: TrialRun
) -> TrialComplete:
    expected = {
        "type": "trial_complete",
        "request_id": request.request_id,
        "target": request.target,
    }
    for field, value in expected.items():
        if event.get(field) != value:
            raise TrialAdapterError(
                f"trial completion {field} is {event.get(field)!r}, expected {value!r}"
            )
    conversation_id = event.get("conversation_id")
    if not isinstance(conversation_id, str) or not conversation_id:
        raise TrialAdapterError("trial completion has no conversation_id")
    summary = event.get("summary")
    if summary is not None and not isinstance(summary, str):
        raise TrialAdapterError("trial completion summary must be a string or null")
    return TrialComplete(
        request_id=request.request_id,
        target=request.target,
        conversation_id=conversation_id,
        summary=summary,
    )
