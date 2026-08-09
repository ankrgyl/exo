"""ExoAgent — Harbor's per-trial bridge to the generic Exo trial adapter.

Where this sits in Harbor's trial lifecycle:

    Trial.create()                    <- new ExoAgent instance
    trial.run()
      _prepare()
        _setup_agent_environment()    <- docker compose up --wait
        run_healthcheck()
        setup(environment)            <- HERE: resolve the container  [1/trial]
      _run()
        run(instruction, ...)         <- HERE: work the task      [1/step]
        _run_verifier()               <- the grade appears here
        _stop_agent_environment()
      _finalize() -> TrialEvent.END   <- plugin records the inventory

Notes:
 * a fresh instance is constructed for each trial then thrown away, so holds
no cross-task state. The actual Exo process is managed by ExoSessionPlugin.
 * setup() is called for each trial, and run()/resume() for each step.
 * run() returns before the verifier executes. Feedback is intentionally not
   part of the first version of the trial protocol.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any, override
from uuid import uuid4

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from exo_harbor import conventions
from exo_harbor.docker import resolve_main_container
from exo_harbor.exo import ExoClient
from exo_harbor.protocol import TrialRun, TrialStarted, send_trial_run
from exo_harbor.trajectory import export_trial_trajectory

logger = logging.getLogger(__name__)


class ExoAgent(BaseAgent):
    """Submits Harbor's task container to Exo and waits for completion."""

    # Can be enabled for multi-step tasks after resume() support is added.
    SUPPORTS_RESUME = False
    SUPPORTS_ATIF = True
    SUPPORTS_WINDOWS = False

    def __init__(
        self,
        *args: Any,
        # Set by --ak. The plugin reads these same values back off job.config.agents[0].kwargs.
        exo_root: str | Path,
        exo_bin: str | Path,
        exo_repo_root: str | Path,
        exo_model: str,
        task_timeout_sec: float | str | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._exo_root = Path(exo_root)
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

    @staticmethod
    @override
    def name() -> str:
        return "exo"

    @override
    def version(self) -> str | None:
        # TODO: report the Exo build under test rather than this package's
        # version — the eval result needs to identify the agent, and "0.1.0"
        # identifies the wrapper.
        return None

    # ----------------------------------------------------------------------
    # Per-trial setup
    # ----------------------------------------------------------------------
    @override
    async def setup(self, environment: BaseEnvironment) -> None:
        """Resolve the Docker container supplied by Harbor for this trial.

        Runs once per trial, after Harbor has started and health-checked the
        environment. Everything expensive happened in on_job_start.
        """
        socket_path = conventions.socket_path(self._exo_root)
        if not socket_path.exists():
            raise RuntimeError(
                f"no trial adapter socket at {socket_path}. Pass "
                "--plugin exo_harbor.plugin:ExoSessionPlugin alongside --agent."
            )

        container = resolve_main_container(environment.session_id)
        self._container_id = container.container_id
        logger.info(
            "resolved Harbor container %s for trial %s",
            container.container_id[:12],
            self.context_id,
        )

    # ----------------------------------------------------------------------
    # Per-step execution
    # ----------------------------------------------------------------------

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Hand Exo the task and block until it says it is done."""
        if self._container_id is None:
            raise RuntimeError("ExoAgent.run called before setup")

        conversation_id: str | None = None

        def trial_started(started: TrialStarted) -> None:
            nonlocal conversation_id
            conversation_id = started.conversation_id
            logger.info(
                "trial %s started in conversation %s",
                self.context_id,
                conversation_id,
            )

        try:
            response = await send_trial_run(
                conventions.socket_path(self._exo_root),
                TrialRun(
                    request_id=str(uuid4()),
                    target=str(self.context_id),
                    container_id=self._container_id,
                    instructions=instruction,
                ),
                timeout_sec=self._task_timeout_sec,
                on_started=trial_started,
            )
            conversation_id = response.conversation_id
        finally:
            if conversation_id is not None:
                try:
                    await export_trial_trajectory(
                        self._client,
                        conversation_id,
                        str(self.context_id),
                        instruction,
                        self._model,
                        self.logs_dir / "trajectory.json",
                    )
                except Exception:
                    logger.exception(
                        "failed to export Harbor trajectory for %s", self.context_id
                    )

        if response.summary:
            logger.info("trial %s complete: %s", self.context_id, response.summary)
