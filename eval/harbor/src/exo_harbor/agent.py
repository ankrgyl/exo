"""ExoAgent — per-task driver, killed after completion of task and reinstantiated.

See https://www.harborframework.com/docs/agents#external-agents for more details
on the lifecycle."""

from __future__ import annotations

import logging
import re
import subprocess
from pathlib import Path
from typing import Any, override

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from exo_harbor import conventions
from exo_harbor.exo import ExoClient
from exo_harbor.trajectory import export_trial_trajectory

logger = logging.getLogger(__name__)


class ExoAgent(BaseAgent):
    SUPPORTS_RESUME = False
    SUPPORTS_ATIF = True
    SUPPORTS_WINDOWS = False
    def __init__(
        self,
        *args: Any,
        exo_root: str | Path,
        exo_bin: str | Path,
        exo_repo_root: str | Path,
        exo_model: str,
        task_timeout_sec: float | str | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._model = exo_model
        self._task_timeout_sec = (
            float(task_timeout_sec) if task_timeout_sec is not None else None
        )
        self._client = ExoClient(
            exo_bin=Path(exo_bin),
            exo_root=Path(exo_root),
            repo_root=Path(exo_repo_root),
        )
        self._container_id: str | None = None
        self._sandbox_id: str | None = None

    @property
    def _conversation(self) -> str:
        """Convo slug computed on demand rather than in __init__ because
        Harbor assigns context_id *after* constructing the agent."""
        assert self.context_id is not None, "Harbor has not assigned context_id yet"
        return conventions.trial_conversation_slug(str(self.context_id))

    @staticmethod
    @override
    def name() -> str:
        return "exo"

    @override
    def version(self) -> str | None:
        # TODO: eventually, should have a version included from the exo binary.
        return None

    @override
    async def setup(self, environment: BaseEnvironment) -> None:
        self._container_id = get_harbor_docker_container_id(environment.session_id)

        # setup dedicated conversation for the trial
        await self._client.ensure_conversation(self._conversation)
        self._sandbox_id = await self._client.attach_container(
            self._conversation, self._container_id
        )
        # Attaching only registers it; this is what runs the trial in it.
        await self._client.select_sandbox(self._conversation, self._sandbox_id)

        
        logger.info(
            "trial %s attached Harbor container %s as sandbox %s in conversation %s",
            self.context_id,
            self._container_id[:12],
            self._sandbox_id,
            self._conversation,
        )

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Hand Exo the task, wait for the turn, and snapshot what it built (to enable reflection)."""
        assert self._sandbox_id is not None, "ExoAgent.run called before setup"

        snapshot_id: str | None = None
        try:
            await self._client.send(
                self._conversation, instruction, timeout_sec=self._task_timeout_sec
            )
        finally:
            # Snapshot the sandbox so we have a record of the final state.
            # Useful b/c Harbor injects verifier code and tears down after completion.
            # TODO: add a flag to enable/disable reflection, and only snapshot if reflection is enabled.
            try:
                snapshot_id = await self._client.snapshot_sandbox(
                    self._conversation, self._sandbox_id
                )
            except Exception:
                logger.exception("trial %s failed to snapshot at end", self.context_id)

            context.metadata = {
                **(context.metadata or {}),
                conventions.CONVERSATION_METADATA_KEY: self._conversation,
                conventions.SNAPSHOT_METADATA_KEY: snapshot_id,
                conventions.INSTRUCTION_METADATA_KEY: instruction,
            }

            try:
                await export_trial_trajectory(
                    self._client,
                    self._conversation,
                    str(self.context_id),
                    instruction,
                    self._model,
                    self.logs_dir / "trajectory.json",
                )
            except Exception:
                logger.exception(
                    "trial %s failed to export its trajectory", self.context_id
                )


def get_harbor_docker_container_id(session_id: str) -> str:
    """Reverse Harbor's Compose project name and finds the main container for it."""
    MAIN_SERVICE = "main"
    PROJECT_LABEL = "com.docker.compose.project"
    SERVICE_LABEL = "com.docker.compose.service"
    
    # Harbor's Compose project name normalization
    name = session_id.lower()
    if not name or not name[0].isalnum():
        name = f"0{name}"
    project = re.sub(r"[^a-z0-9_-]", "-", name)
    
    ids = _docker(
        "ps",
        "--filter",
        f"label={PROJECT_LABEL}={project}",
        "--filter",
        f"label={SERVICE_LABEL}={MAIN_SERVICE}",
        "--format",
        "{{.ID}}",
    ).split()

    if not ids:
        raise RuntimeError(
            f"no running {MAIN_SERVICE} container for Compose project {project!r}"
        )
    if len(ids) > 1:
        raise RuntimeError(
            f"{len(ids)} running {MAIN_SERVICE} containers for Compose project "
            f"{project!r}; refusing to guess"
        )
    return ids[0]


def _docker(*args: str) -> str:
    result = subprocess.run(
        ["docker", *args], capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise RuntimeError(f"docker {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout.strip()
