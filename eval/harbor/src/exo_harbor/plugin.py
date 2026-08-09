"""ExoSessionPlugin — owner of the Exo process for the whole job.

This plugin owns Exo's job-scoped lifetime, whereas ExoAgent is a
per-trial stub.

The lifecycle:

    attach_job_plugins(job)
      on_job_start(job)         start Exo and create the adapter
      job.run()                 run trials through that persistent Exo agent

Requires --n-concurrent 1. Trials share one Exo, so serial execution gives
each trial a deterministic view of changes made by earlier trials.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

from harbor.job import Job
from harbor.models.environment_type import EnvironmentType
from harbor.models.job.plugin import BaseJobPlugin
from harbor.models.job.result import JobResult

from exo_harbor import conventions
from exo_harbor.exo import ExoClient

logger = logging.getLogger(__name__)


class ExoSessionPlugin(BaseJobPlugin):
    """Runs one persistent Exo agent across every trial in the job."""

    def __init__(
        self,
        *,
        # Set by --pk. Note exo_root is absent: it is read off the agent's own
        # kwargs in on_job_start so it stays a single source of truth.
        adapter_start_timeout_sec: float | str = 90,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._adapter_start_timeout_sec = float(adapter_start_timeout_sec)

    async def on_job_start(self, job: Job) -> None:
        """Bring Exo up before the first container is built."""
        if job.config.environment.type != EnvironmentType.DOCKER:
            raise ValueError(
                "ExoSessionPlugin resolves task containers by Compose label on "
                "the host, so it requires --env docker"
            )
        if job.config.n_concurrent_trials != 1:
            # To properly learn, Exo can only handle one task at a time.
            raise ValueError("ExoSessionPlugin requires --n-concurrent 1")
        if len(job.config.agents) != 1:
            raise ValueError("ExoSessionPlugin requires exactly one --agent")

        # Read the agent's --ak values rather than taking --pk copies, so
        # exo_root and friends are written down once.
        kwargs = job.config.agents[0].kwargs
        try:
            model = kwargs["exo_model"]
            client = ExoClient(
                exo_bin=Path(kwargs["exo_bin"]),
                exo_root=Path(kwargs["exo_root"]),
                repo_root=Path(kwargs["exo_repo_root"]),
            )
        except KeyError as error:
            raise ValueError(
                f"ExoAgent is missing required --ak {error.args[0]}"
            ) from error

        socket_path = conventions.socket_path(client.exo_root)
        await client.ensure_agent(model)
        await client.ensure_trial_adapter(socket_path)
        await client.ensure_adapter_runner(
            socket_path, timeout_sec=self._adapter_start_timeout_sec
        )
        logger.info("Exo ready for job %s on %s", job.id, socket_path)

    async def on_job_end(self, _job_result: JobResult) -> None:
        """Satisfy Harbor's plugin lifecycle; Exo state remains on disk."""

# TODO: add on_job_end to produce a report on the learnings obtained during
# the trial.
