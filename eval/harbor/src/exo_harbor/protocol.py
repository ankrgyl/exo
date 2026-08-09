"""Typed protocol for submitting a containerized trial to Exo.

Harbor opens a Unix socket and sends one newline-delimited ``trial_run``
request containing the task container, instructions, request id, and stable
trial target. The adapter keeps that connection open and sends two messages:

1. A ``trial_started`` event after Exo creates the conversation and attaches
   the container. Its conversation id lets Harbor export a partial trajectory
   even if the trial later times out.
2. A final ``trial_complete`` response after Exo explicitly declares the work
   finished with ``send_adapter_message``.

If Exo restarts or the socket disconnects, Harbor reconnects and resends the
same request. The request id makes that retry idempotent, and the adapter
replays any durable ``trial_started`` or ``trial_complete`` message.
"""

from __future__ import annotations

import asyncio
import json
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Callable, Literal


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


@dataclass(frozen=True)
class TrialStarted:
    request_id: str
    target: str
    conversation_id: str


class TrialAdapterError(RuntimeError):
    """The trial adapter rejected a request or returned an invalid response."""


class _Disconnected(RuntimeError):
    """The adapter socket disappeared while a trial was running."""


async def send_trial_run(
    socket_path: Path,
    request: TrialRun,
    *,
    timeout_sec: float | None,
    on_started: Callable[[TrialStarted], None] | None = None,
) -> TrialComplete:
    """Wait for explicit completion, reconnecting across Exo restarts."""

    started: TrialStarted | None = None

    def receive_started(event: dict[str, Any]) -> None:
        nonlocal started
        received = _parse_started(event, request)
        if started is not None and started != received:
            raise TrialAdapterError(
                "trial_started changed while reconnecting: "
                f"{started.conversation_id!r} became {received.conversation_id!r}"
            )
        if started is None:
            started = received
            if on_started is not None:
                on_started(received)

    async def wait_for_completion() -> TrialComplete:
        while True:
            try:
                event = await _exchange(
                    socket_path, request.payload(), receive_started
                )
                completion = _parse_completion(event, request)
                if (
                    started is not None
                    and completion.conversation_id != started.conversation_id
                ):
                    raise TrialAdapterError(
                        "trial_complete conversation_id does not match trial_started"
                    )
                return completion
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


async def _exchange(
    socket_path: Path,
    payload: dict[str, Any],
    on_started: Callable[[dict[str, Any]], None],
) -> dict[str, Any]:
    reader, writer = await asyncio.open_unix_connection(str(socket_path))
    try:
        writer.write(f"{json.dumps(payload)}\n".encode())
        await writer.drain()
        while True:
            envelope, event = await _read_event(reader)
            if envelope == "event":
                on_started(event)
                continue
            return event
    finally:
        writer.close()
        await writer.wait_closed()


async def _read_event(
    reader: asyncio.StreamReader,
) -> tuple[Literal["event", "response"], dict[str, Any]]:
    line = await reader.readline()
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
    envelope = message.get("type")
    if envelope not in {"event", "response"} or not isinstance(
        message.get("event"), dict
    ):
        raise TrialAdapterError(f"unexpected trial adapter reply: {message!r}")
    return envelope, message["event"]


def _parse_started(event: dict[str, Any], request: TrialRun) -> TrialStarted:
    _validate_routing_fields(event, request, "trial_started")
    return TrialStarted(
        request_id=request.request_id,
        target=request.target,
        conversation_id=_conversation_id(event, "trial_started"),
    )


def _parse_completion(
    event: dict[str, Any], request: TrialRun
) -> TrialComplete:
    _validate_routing_fields(event, request, "trial_complete")
    conversation_id = _conversation_id(event, "trial_complete")
    summary = event.get("summary")
    if summary is not None and not isinstance(summary, str):
        raise TrialAdapterError("trial completion summary must be a string or null")
    return TrialComplete(
        request_id=request.request_id,
        target=request.target,
        conversation_id=conversation_id,
        summary=summary,
    )


def _validate_routing_fields(
    event: dict[str, Any], request: TrialRun, event_type: str
) -> None:
    expected = {
        "type": event_type,
        "request_id": request.request_id,
        "target": request.target,
    }
    for field, value in expected.items():
        if event.get(field) != value:
            raise TrialAdapterError(
                f"{event_type} {field} is {event.get(field)!r}, expected {value!r}"
            )


def _conversation_id(event: dict[str, Any], event_type: str) -> str:
    conversation_id = event.get("conversation_id")
    if not isinstance(conversation_id, str) or not conversation_id:
        raise TrialAdapterError(f"{event_type} has no conversation_id")
    return conversation_id
