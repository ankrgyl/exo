"""Small CLI client for Exo job setup and trajectory export."""

from __future__ import annotations

import asyncio
import os
import shutil
import subprocess
import threading
from dataclasses import dataclass
from pathlib import Path

from exo_harbor import conventions
from exo_harbor.protocol import probe


class ExoCommandError(RuntimeError):
    """An Exo CLI command failed."""


@dataclass(frozen=True)
class ExoClient:
    exo_bin: Path
    exo_root: Path
    repo_root: Path
    sandbox_backend: str = "docker"

    async def ensure_agent(self, model: str) -> None:
        """Create the persistent evaluation agent if it does not exist."""
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
            str(self.repo_root / "exo/harness.ts"),
            "--sandbox-provider",
            "docker",
            "--sandbox-scope",
            "agent",
            "--tool-creation",
            "enabled",
        )

    async def ensure_trial_adapter(self, socket_path: Path) -> None:
        """Create the job's trial adapter if it does not exist."""
        setup = conventions.SETUP_CONVERSATION_SLUG
        await self._ensure_conversation(setup)
        await self._run(
            "conversation",
            "send",
            conventions.AGENT_SLUG,
            setup,
            _ADAPTER_SETUP_PROMPT.format(socket_path=socket_path),
        )

    async def ensure_adapter_runner(
        self, socket_path: Path, *, timeout_sec: float
    ) -> None:
        """Start the adapter supervisor and wait for its socket."""
        if await probe(socket_path, timeout_sec=0.5):
            return
        await asyncio.to_thread(self._spawn_adapter_runner)
        if not await probe(socket_path, timeout_sec=timeout_sec):
            raise ExoCommandError(
                f"trial adapter socket never appeared at {socket_path}; "
                f"check {self.exo_root / 'exo-adapters.log'}"
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
        args = [
            "conversation",
            "events",
            conventions.AGENT_SLUG,
            conversation,
        ]
        for event_type in types:
            args.extend(("--type", event_type))
        if turn_id is not None:
            args.extend(("--turn-id", turn_id))
        args.extend(("--limit", str(limit)))
        return await self._run(*args)

    async def delete_snapshots(self) -> int:
        """Delete snapshot payloads while preserving the rest of the Exo run."""
        snapshot_directories = list(
            (self.exo_root / "exoharness" / "agents").glob(
                "*/conversations/*/snapshots"
            )
        )
        for directory in snapshot_directories:
            await asyncio.to_thread(shutil.rmtree, directory)
        return len(snapshot_directories)

    async def _ensure_conversation(self, slug: str) -> None:
        if await self._exists("conversation", "show", conventions.AGENT_SLUG, slug):
            return
        await self._run(
            "conversation",
            "create",
            conventions.AGENT_SLUG,
            slug,
            "--slug",
            slug,
            "--sandbox-scope",
            "agent",
        )

    def _spawn_adapter_runner(self) -> None:
        log_path = self.exo_root / "exo-adapters.log"
        log_path.parent.mkdir(parents=True, exist_ok=True)
        with log_path.open("ab") as log:
            process = subprocess.Popen(  # noqa: S603 - fixed argv, no shell
                self._argv(
                    "adapters",
                    "run",
                    "--lock-file",
                    str(self.exo_root / "exo-adapters.lock"),
                    "--drain-marker",
                    str(self.exo_root / "exo-adapters.restart"),
                    "--reboot-notice",
                    str(self.exo_root / "exo-reboot-notice.json"),
                ),
                cwd=self.repo_root,
                stdout=log,
                stderr=log,
                env=self._environment(),
                start_new_session=True,
            )
        (self.exo_root / "exo-adapters.pid").write_text(
            f"{process.pid}\n", encoding="utf-8"
        )
        threading.Thread(target=process.wait, daemon=True).start()

    async def _run(self, *args: str) -> str:
        process = await asyncio.create_subprocess_exec(
            *self._argv(*args),
            cwd=self.repo_root,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self._environment(),
        )
        stdout, stderr = await process.communicate()
        if process.returncode != 0:
            raise ExoCommandError(
                f"exo {' '.join(args)} failed ({process.returncode}): "
                f"{stderr.decode().strip()}"
            )
        return stdout.decode().strip()

    def _environment(self) -> dict[str, str]:
        return {
            **os.environ,
            "EXO_PROFILE": os.environ.get("EXO_PROFILE", "practical"),
            "EXO_ROOT": str(self.exo_root),
        }

    async def _exists(self, *args: str) -> bool:
        process = await asyncio.create_subprocess_exec(
            *self._argv(*args),
            cwd=self.repo_root,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
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

    def _argv(self, *args: str) -> list[str]:
        return [
            str(self.exo_bin),
            "--root",
            str(self.exo_root),
            "--harness",
            "exo",
            "--sandbox-backend",
            self.sandbox_backend,
            *args,
        ]


_ADAPTER_SETUP_PROMPT = (
    "Configure the trial adapter for this conversation. Call list_adapters "
    "with includeDisabled=true. If there is no enabled adapter named `trial` "
    "with type `trial` and socketPath exactly `{socket_path}`, call "
    "create_adapter with name `trial`, source `library`, and config "
    '{{"type":"trial","socketPath":"{socket_path}"}}. Do not create a '
    "duplicate. If an adapter named `trial` exists but is disabled or has "
    "different configuration, report that clearly instead of changing or "
    "deleting it."
)
