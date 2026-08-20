"""Exo CLI typed wrapper."""

from __future__ import annotations

import asyncio
import os
import re
import shutil
from dataclasses import dataclass
from pathlib import Path

from exo_harbor import conventions


class ExoCommandError(RuntimeError):
    """An Exo CLI command failed."""


@dataclass(frozen=True)
class ExoClient:
    exo_bin: Path
    exo_root: Path
    repo_root: Path

    async def ensure_agent(self, model: str) -> None:
        if await self._exists("agent", "show", conventions.AGENT_SLUG):
            return
        await self._run(
            "agent",
            "create",
            "Harbor eval",
            "--slug",
            conventions.AGENT_SLUG,
            "--model",
            model,
            "--module",
            str(self.repo_root / conventions.HARNESS_MODULE),
            "--sandbox-provider",
            "docker",
            "--sandbox-scope",
            "agent",
            "--tool-creation",
            "enabled",
        )

    async def ensure_conversation(self, slug: str) -> None:
        if await self._exists(
            "conversation", "show", conventions.AGENT_SLUG, slug
        ):
            return
        await self._run(
            "conversation",
            "create",
            conventions.AGENT_SLUG,
            slug,
            "--slug",
            slug,
            "--sandbox-scope",
            "conversation",
        )

    async def attach_container(
        self,
        conversation: str,
        container_id: str,
        *,
        default_workdir: str | None = None,
    ) -> str:
        """Attach Harbor's task container and return the Exo sandbox id."""
        arguments = [
            "conversation",
            "sandbox",
            "attach",
            conventions.AGENT_SLUG,
            conversation,
            "--provider",
            "docker",
            "--external-id",
            container_id,
        ]
        if default_workdir is not None:
            arguments.extend(("--default-workdir", default_workdir))
        return _parse_trailing_id(
            await self._run(*arguments),
            pattern=r"attached Docker container as sandbox (\S+) for ",
            command="conversation sandbox attach",
        )

    async def send(
        self, conversation: str, prompt: str, *, timeout_sec: float | None
    ) -> str:
        """Run one Exo turn to completion and return its printed messages.

        Blocks for as long as the turn takes. On timeout the subprocess is
        killed, which aborts the turn; convo survives in the state so can
        still be inspected.
        """
        return await self._run(
            "conversation",
            "send",
            conventions.AGENT_SLUG,
            conversation,
            prompt,
            timeout_sec=timeout_sec,
        )

    async def snapshot_sandbox(
        self, conversation: str, *, sandbox_id: str | None = None
    ) -> str:
        arguments = [
            "conversation",
            "sandbox",
            "snapshot",
            conventions.AGENT_SLUG,
            conversation,
        ]
        if sandbox_id is not None:
            arguments.extend(("--sandbox-id", sandbox_id))
        return _parse_trailing_id(
            await self._run(*arguments),
            pattern=r"snapshotted sandbox \S+ as (\S+)$",
            command="conversation sandbox snapshot",
        )

    async def create_sandbox_from_snapshot(
        self, conversation: str, snapshot_id: str
    ) -> str:
        return _parse_trailing_id(
            await self._run(
                "conversation",
                "sandbox",
                "create-from-snapshot",
                conventions.AGENT_SLUG,
                conversation,
                "--snapshot-id",
                snapshot_id,
            ),
            pattern=r"created sandbox (\S+) from snapshot ",
            command="conversation sandbox create-from-snapshot",
        )

    async def read_conversation_events(
        self,
        conversation: str,
        *,
        types: list[str],
        turn_id: str | None = None,
        limit: int,
    ) -> str:
        """Return canonical conversation events as JSON."""
        arguments = [
            "conversation",
            "events",
            conventions.AGENT_SLUG,
            conversation,
        ]
        for event_type in types:
            arguments.extend(("--type", event_type))
        if turn_id is not None:
            arguments.extend(("--turn-id", turn_id))
        arguments.extend(("--limit", str(limit)))
        return await self._run(*arguments)

    async def delete_snapshots(self) -> int:
        # TODO: consider scoping this to be for a specified convo.
        snapshot_directories = list(
            (self.exo_root / "exoharness" / "agents").glob(
                "*/conversations/*/snapshots"
            )
        )
        for directory in snapshot_directories:
            await asyncio.to_thread(shutil.rmtree, directory)
        return len(snapshot_directories)

    async def _run(self, *args: str, timeout_sec: float | None = None) -> str:
        process = await asyncio.create_subprocess_exec(
            *self._argv(*args),
            cwd=self.repo_root,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self._environment(),
        )
        try:
            stdout, stderr = await asyncio.wait_for(
                process.communicate(), timeout=timeout_sec
            )
        except (asyncio.TimeoutError, asyncio.CancelledError):
            # Kill rather than terminate: the turn holds a sandbox and we want
            # the process gone before the caller moves on to snapshotting.
            process.kill()
            await process.wait()
            raise
        if process.returncode != 0:
            raise ExoCommandError(
                f"exo {' '.join(args)} failed ({process.returncode}): "
                f"{stderr.decode().strip()}"
            )
        return stdout.decode().strip()

    async def _exists(self, *args: str) -> bool:
        process = await asyncio.create_subprocess_exec(
            *self._argv(*args),
            cwd=self.repo_root,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self._environment(),
        )
        _, stderr = await process.communicate()
        if process.returncode == 0:
            return True
        if "not found" in stderr.decode().lower():
            return False
        raise ExoCommandError(
            f"exo {' '.join(args)} failed ({process.returncode}): "
            f"{stderr.decode().strip()}"
        )

    def _environment(self) -> dict[str, str]:
        return {
            **os.environ,
            "EXO_PROFILE": os.environ.get("EXO_PROFILE", "practical"),
            "EXO_ROOT": str(self.exo_root),
        }

    def _argv(self, *args: str) -> list[str]:
        return [
            str(self.exo_bin),
            "--root",
            str(self.exo_root),
            "--harness",
            "exo",
            *args,
        ]


def _parse_trailing_id(output: str, *, pattern: str, command: str) -> str:
    """Pull an id out of the CLI's human-readable confirmation line."""
    for line in output.splitlines():
        if match := re.search(pattern, line.strip()):
            return match.group(1)
    raise ExoCommandError(
        f"could not read the id from `exo {command}` output: {output!r}"
    )
