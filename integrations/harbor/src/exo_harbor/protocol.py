from __future__ import annotations

import asyncio
import json
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Literal
from uuid import uuid4


@dataclass(frozen=True)
class TaskStarted:
    trial_id: str
    task_name: str
    instruction: str
    conversation_id: str
    sandbox_id: str
    deadline_at: str | None = None
    type: Literal["task_started"] = "task_started"
    message_id: str = ""

    def payload(self) -> dict[str, Any]:
        payload = asdict(self)
        payload["message_id"] = self.message_id or str(uuid4())
        return payload


@dataclass(frozen=True)
class TaskComplete:
    trial_id: str
    summary: str | None
    type: Literal["task_complete"] = "task_complete"


class HarborAdapterError(RuntimeError):
    pass


async def send_task_started(
    socket_path: Path,
    request: TaskStarted,
    *,
    timeout_sec: float,
) -> TaskComplete:
    response = await _request(socket_path, request.payload(), timeout_sec=timeout_sec)
    if response.get("type") != "response":
        raise HarborAdapterError(_string(response.get("message"), "message"))
    event = _object(response.get("event"), "event")
    if event.get("type") != "task_complete":
        raise HarborAdapterError("adapter response must be task_complete")
    trial_id = _string(event.get("trial_id"), "event.trial_id")
    if trial_id != request.trial_id:
        raise HarborAdapterError(
            f"response trial_id {trial_id} does not match {request.trial_id}"
        )
    summary = event.get("summary")
    if summary is not None and not isinstance(summary, str):
        raise HarborAdapterError("event.summary must be a string or null")
    return TaskComplete(trial_id=trial_id, summary=summary)


async def probe(socket_path: Path) -> bool:
    try:
        _reader, writer = await asyncio.open_unix_connection(socket_path)
    except (FileNotFoundError, ConnectionRefusedError, OSError):
        return False
    writer.close()
    await writer.wait_closed()
    return True


async def _request(
    socket_path: Path, payload: dict[str, Any], *, timeout_sec: float
) -> dict[str, Any]:
    async def exchange() -> dict[str, Any]:
        reader, writer = await asyncio.open_unix_connection(socket_path)
        try:
            writer.write(json.dumps(payload, separators=(",", ":")).encode() + b"\n")
            await writer.drain()
            line = await reader.readline()
            if not line:
                raise HarborAdapterError("adapter closed the socket without a response")
            decoded = json.loads(line)
            return _object(decoded, "adapter response")
        finally:
            writer.close()
            await writer.wait_closed()

    try:
        return await asyncio.wait_for(exchange(), timeout=timeout_sec)
    except TimeoutError as error:
        raise HarborAdapterError(
            f"timed out waiting {timeout_sec:g}s for Exo"
        ) from error


def _object(value: Any, name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise HarborAdapterError(f"{name} must be an object")
    return value


def _string(value: Any, name: str) -> str:
    if not isinstance(value, str) or not value:
        raise HarborAdapterError(f"{name} must be a non-empty string")
    return value
