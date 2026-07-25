from __future__ import annotations

import asyncio
import re
from pathlib import Path
from typing import Literal

from filelock import FileLock
from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from exo_harbor import __version__
from exo_harbor.docker import resolve_main_container
from exo_harbor.exo import Adapter, ExoClient
from exo_harbor.protocol import TaskStarted, send_task_started


class ExoAgent(BaseAgent):
    """Harbor external agent backed by a host-side Exo conversation."""

    def __init__(
        self,
        *args,
        exo_repo_root: str = ".",
        exo_root: str = "~/.exo",
        exo_bin: str | None = None,
        exo_agent: str = "harbor",
        exo_model: str | None = None,
        conversation_mode: Literal["per_task", "shared"] = "per_task",
        conversation: str = "harbor",
        adapter_start_timeout_sec: float | str = 30,
        task_timeout_sec: float | str = 3600,
        **kwargs,
    ) -> None:
        super().__init__(*args, **kwargs)
        if conversation_mode not in ("per_task", "shared"):
            raise ValueError("conversation_mode must be per_task or shared")
        model = exo_model or self.model_name
        if not model:
            raise ValueError("exo_model or Harbor model_name is required")

        repo_root = Path(exo_repo_root).expanduser().resolve()
        executable = exo_bin or str(repo_root / "target/debug/exo")
        self._exo = ExoClient(
            executable=executable,
            root=Path(exo_root),
            repo_root=repo_root,
            logs_dir=self.logs_dir,
        )
        self._agent_slug = _slug(exo_agent)
        self._model = model
        self._conversation_mode = conversation_mode
        self._shared_conversation = _slug(conversation)
        self._adapter_start_timeout_sec = float(adapter_start_timeout_sec)
        self._task_timeout_sec = float(task_timeout_sec)
        self._conversation_slug: str | None = None
        self._adapter: Adapter | None = None
        self._sandbox_id: str | None = None

    @staticmethod
    def name() -> str:
        return "exo"

    def version(self) -> str:
        return __version__

    async def setup(self, environment: BaseEnvironment) -> None:
        trial_id = self._trial_id(environment)
        conversation = (
            self._shared_conversation
            if self._conversation_mode == "shared"
            else _slug(f"harbor-{trial_id}")
        )
        socket_path = self._exo.root / "harbor" / f"{conversation}.sock"
        self._exo.root.mkdir(parents=True, exist_ok=True)
        lock = FileLock(self._exo.root / "harbor-setup.lock")
        await asyncio.to_thread(lock.acquire)
        try:
            if not await self._exo.exists("agent", "show", self._agent_slug):
                await self._exo.create_agent(self._agent_slug, self._model)
            if not await self._exo.exists(
                "conversation", "show", self._agent_slug, conversation
            ):
                await self._exo.create_conversation(self._agent_slug, conversation)
            adapter = await self._exo.ensure_adapter(
                self._agent_slug, conversation, socket_path
            )
        finally:
            await asyncio.to_thread(lock.release)

        await self._exo.ensure_runner(
            adapter.socket_path, self._adapter_start_timeout_sec
        )
        container = await resolve_main_container(environment.session_id)
        sandbox_id = await self._exo.attach(
            self._agent_slug,
            conversation,
            container_id=container.id,
            default_workdir=container.workdir,
        )
        self._conversation_slug = conversation
        self._adapter = adapter
        self._sandbox_id = sandbox_id

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if not self._conversation_slug or not self._adapter or not self._sandbox_id:
            raise RuntimeError("ExoAgent.setup() must complete before run()")
        trial_id = self._trial_id(environment)
        request = TaskStarted(
            trial_id=trial_id,
            task_name=environment.environment_name,
            instruction=instruction,
            conversation_id=self._adapter.conversation_id,
            sandbox_id=self._sandbox_id,
        )
        exo_metadata: dict[str, object] = {
            "trial_id": trial_id,
            "agent": self._agent_slug,
            "conversation": self._conversation_slug,
            "conversation_id": self._adapter.conversation_id,
            "adapter_id": self._adapter.adapter_id,
            "socket_path": str(self._adapter.socket_path),
            "sandbox_id": self._sandbox_id,
            "conversation_mode": self._conversation_mode,
        }
        context.metadata = {**(context.metadata or {}), "exo": exo_metadata}
        try:
            completed = await send_task_started(
                self._adapter.socket_path,
                request,
                timeout_sec=self._task_timeout_sec,
            )
            exo_metadata["summary"] = completed.summary
        finally:
            sandbox_id = self._sandbox_id
            self._sandbox_id = None
            await self._exo.detach(
                self._agent_slug, self._conversation_slug, sandbox_id
            )

    def _trial_id(self, environment: BaseEnvironment) -> str:
        context_id = self.context_id or environment.context_id
        if context_id is None:
            raise RuntimeError("Harbor did not assign a trial context_id")
        return str(context_id)


def _slug(value: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-")
    if not slug:
        raise ValueError("Exo slug must contain a letter or number")
    return slug
