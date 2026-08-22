"""Exo CLI typed wrapper."""

from __future__ import annotations

import asyncio
import os
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
            "--provider",
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

    def _owner(self, conversation: str) -> list[str]:
        """Address the conversation as the sandbox owner.

        A sandbox id resolves only against its owner, so every `exo sandbox`
        call has to name the conversation; without it the sandbox would belong
        to the agent and the conversation could not use it.
        """
        return ["--agent", conventions.AGENT_SLUG, "--conversation", conversation]

    async def attach_container(
        self,
        conversation: str,
        container_id: str,
        *,
        default_workdir: str | None = None,
    ) -> str:
        """Attach Harbor's task container and return the Exo sandbox id."""
        arguments = [
            "sandbox",
            "attach",
            *self._owner(conversation),
            "--provider",
            "docker",
            "--external-id",
            container_id,
        ]
        if default_workdir is not None:
            arguments.extend(("--default-workdir", default_workdir))
        return (await self._run(*arguments)).strip()

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

    async def snapshot_sandbox(self, conversation: str, sandbox_id: str) -> str:
        """Snapshot the given sandbox and return the snapshot id.

        Pass the id attach returned rather than letting anything re-derive it:
        for a trial conversation that sandbox is Harbor's task container, and
        this has to run before Harbor tears it down or the submitted state is
        gone and reflection has nothing to inspect.
        """
        return (
            await self._run(
                "sandbox",
                "snapshot",
                *self._owner(conversation),
                sandbox_id,
            )
        ).strip()

    async def restore_sandbox(self, conversation: str, snapshot_id: str) -> str:
        """Restore a snapshot into a new sandbox and make the conversation use it.

        Restoring only creates the sandbox. Nothing infers that the caller
        wants it, so the binding is recorded explicitly; the shell would
        otherwise fall back to configuration and build a fresh container,
        discarding the state just restored.
        """
        sandbox_id = (
            await self._run(
                "sandbox",
                "restore",
                *self._owner(conversation),
                snapshot_id,
                "--provider",
                "docker",
            )
        ).strip()
        await self._run(
            "conversation",
            "update",
            conventions.AGENT_SLUG,
            conversation,
            "--sandbox-id",
            sandbox_id,
        )
        return sandbox_id

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
