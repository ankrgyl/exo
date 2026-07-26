from __future__ import annotations

import asyncio
import json
import os
import subprocess
from pathlib import Path
from typing import Any

from exo_harbor.protocol import probe


class ExoCommandError(RuntimeError):
    pass


class ExoClient:
    def __init__(
        self,
        *,
        executable: str,
        root: Path,
        repo_root: Path,
        logs_dir: Path,
    ) -> None:
        self.executable = executable
        self.root = root.expanduser().resolve()
        self.repo_root = repo_root.expanduser().resolve()
        self.logs_dir = logs_dir.resolve()

    async def exists(self, *args: str) -> bool:
        result = await self._run(*args, check=False)
        if result.returncode == 0:
            return True
        if "not found" in result.stderr.lower():
            return False
        raise ExoCommandError(self._failure(args, result))

    async def create_agent(self, slug: str, model: str) -> None:
        harness = self.repo_root / "examples/exo/harness.ts"
        await self.run(
            "agent",
            "create",
            slug,
            "--slug",
            slug,
            "--model",
            model,
            "--module",
            str(harness),
            "--sandbox-provider",
            "docker",
            "--sandbox-scope",
            "agent",
        )

    async def create_conversation(self, agent: str, conversation: str) -> None:
        await self.run(
            "conversation",
            "create",
            agent,
            conversation,
            "--slug",
            conversation,
            "--sandbox-scope",
            "agent",
        )

    async def conversation_id(self, agent: str, conversation: str) -> str:
        output = await self.run(
            "conversation",
            "show",
            agent,
            conversation,
        )
        for line in output.splitlines():
            key, separator, value = line.partition(":")
            if separator and key == "id" and value.strip():
                return value.strip()
        raise ExoCommandError("conversation show output did not contain an id")

    async def ensure_harbor_adapter(
        self, agent: str, setup_conversation: str, socket_path: Path
    ) -> None:
        prompt = (
            "Configure the Harbor adapter for this setup conversation. "
            "Call list_adapters with includeDisabled=true. If there is no enabled "
            "adapter named `harbor` with type `harbor` and socketPath exactly "
            f"`{socket_path}`, call create_adapter with name `harbor`, source "
            f'`library`, and config {{"type":"harbor","socketPath":'
            f'"{socket_path}"}}. Do not create a duplicate. If an adapter named '
            "`harbor` exists but is disabled or has different configuration, "
            "report that clearly instead of changing or deleting it."
        )
        await self.run("conversation", "send", agent, setup_conversation, prompt)

    async def attach(
        self,
        agent: str,
        conversation: str,
        *,
        container_id: str,
        default_workdir: str,
    ) -> str:
        output = await self.run(
            "conversation",
            "sandbox",
            "attach",
            agent,
            conversation,
            "--provider",
            "docker",
            "--external-id",
            container_id,
            "--default-workdir",
            default_workdir,
            "--json",
        )
        return _required_string(_json_object(output, "attach output"), "sandbox_id")

    async def detach(self, agent: str, conversation: str, sandbox_id: str) -> None:
        await self.run(
            "conversation",
            "sandbox",
            "detach",
            agent,
            conversation,
            sandbox_id,
            "--json",
        )

    async def ensure_runner(self, socket_path: Path, timeout_sec: float) -> None:
        if await probe(socket_path):
            return
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        log_path = self.logs_dir / "exo-adapters.log"
        log = log_path.open("ab")
        try:
            await asyncio.create_subprocess_exec(
                *self._command(
                    "adapters",
                    "run",
                    "--lock-file",
                    str(self.root / "adapters.lock"),
                ),
                cwd=self.repo_root,
                env=os.environ.copy(),
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        finally:
            log.close()

        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout_sec
        while loop.time() < deadline:
            if await probe(socket_path):
                return
            await asyncio.sleep(0.1)
        raise ExoCommandError(
            f"Harbor adapter did not listen on {socket_path} within {timeout_sec:g}s; "
            f"see {log_path}"
        )

    async def run(self, *args: str) -> str:
        result = await self._run(*args, check=True)
        return result.stdout

    async def _run(self, *args: str, check: bool) -> subprocess.CompletedProcess[str]:
        command = self._command(*args)
        try:
            process = await asyncio.create_subprocess_exec(
                *command,
                cwd=self.repo_root,
                env=os.environ.copy(),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
        except FileNotFoundError as error:
            raise ExoCommandError(
                f"Exo executable was not found: {self.executable}"
            ) from error
        stdout, stderr = await process.communicate()
        if process.returncode is None:
            raise ExoCommandError("Exo process exited without a return code")
        result: subprocess.CompletedProcess[str] = subprocess.CompletedProcess(
            command,
            process.returncode,
            stdout.decode(errors="replace"),
            stderr.decode(errors="replace"),
        )
        if check and result.returncode != 0:
            raise ExoCommandError(self._failure(args, result))
        return result

    def _command(self, *args: str) -> list[str]:
        return [
            self.executable,
            "--root",
            str(self.root),
            "--harness",
            "exo",
            *args,
        ]

    @staticmethod
    def _failure(
        args: tuple[str, ...], result: subprocess.CompletedProcess[str]
    ) -> str:
        detail = result.stderr.strip() or result.stdout.strip()
        return f"exo {' '.join(args)} failed ({result.returncode}): {detail}"


def _json_object(text: str, name: str) -> dict[str, Any]:
    try:
        value = json.loads(text)
    except json.JSONDecodeError as error:
        raise ExoCommandError(f"{name} was not JSON: {text.strip()}") from error
    if not isinstance(value, dict):
        raise ExoCommandError(f"{name} must be a JSON object")
    return value


def _required_string(value: dict[str, Any], key: str) -> str:
    item = value.get(key)
    if not isinstance(item, str) or not item:
        raise ExoCommandError(f"{key} must be a non-empty string")
    return item
