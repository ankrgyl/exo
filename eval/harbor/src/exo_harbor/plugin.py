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
from uuid import uuid4

from harbor.job import Job
from harbor.models.environment_type import EnvironmentType
from harbor.models.job.plugin import BaseJobPlugin
from harbor.models.job.result import JobResult
from harbor.trial.hooks import TrialHookEvent

from exo_harbor import conventions
from exo_harbor.exo import ExoClient
from exo_harbor.feedback import REFLECTION_INSTRUCTIONS, build_feedback
from exo_harbor.protocol import TrialFeedback, send_trial_feedback
from exo_harbor.trajectory import export_trial_trajectory

logger = logging.getLogger(__name__)


class ExoSessionPlugin(BaseJobPlugin):
    """Runs one persistent Exo agent across every trial in the job."""

    def __init__(
        self,
        *,
        # Set by --pk. Note exo_root is absent: it is read off the agent's own
        # kwargs in on_job_start so it stays a single source of truth.
        adapter_start_timeout_sec: float | str = 90,
        feedback_timeout_sec: float | str = 600,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._adapter_start_timeout_sec = float(adapter_start_timeout_sec)
        self._feedback_timeout_sec = float(feedback_timeout_sec)
        self._client: ExoClient | None = None
        self._model: str | None = None

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
        self._client = client
        self._model = model
        job.on_trial_ended(self._reflect_on_trial)
        logger.info("Exo ready for job %s on %s", job.id, socket_path)

    async def _reflect_on_trial(self, event: TrialHookEvent) -> None:
        result = event.result
        context = result.agent_result
        metadata = context.metadata if context is not None else None
        if not metadata or not metadata.get("exo_snapshot_id"):
            logger.info("trial %s has no Exo snapshot; skipping feedback", result.id)
            return
        conversation_id = metadata.get("exo_conversation_id")
        instruction = metadata.get("exo_instruction")
        if not isinstance(conversation_id, str) or not isinstance(instruction, str):
            raise ValueError(f"trial {result.id} has incomplete Exo metadata")
        if self._client is None or self._model is None:
            raise RuntimeError("Exo feedback hook ran before job setup")

        trial_dir = event.config.trials_dir / event.trial_name
        response = await send_trial_feedback(
            conventions.socket_path(self._client.exo_root),
            TrialFeedback(
                request_id=str(uuid4()),
                target=str(result.id),
                instructions=REFLECTION_INSTRUCTIONS,
                feedback=build_feedback(result, trial_dir / "verifier"),
            ),
            timeout_sec=self._feedback_timeout_sec,
        )
        if response.conversation_id != conversation_id:
            raise ValueError(f"trial {result.id} feedback changed Exo conversations")
        await _export_trajectory(
            self._client,
            conversation_id,
            str(result.id),
            instruction,
            self._model,
            trial_dir / "agent" / "trajectory.json",
        )
        logger.info("trial %s feedback complete", result.id)

    async def on_job_end(self, _job_result: JobResult) -> None:
        """Discard temporary snapshots while retaining the run's other artifacts."""
        if self._client is None:
            return
        count = await self._client.delete_snapshots()
        logger.info("deleted %d Exo snapshot directories", count)

# TODO: add on_job_end to produce a report on the learnings obtained during
# the trial.


async def _export_trajectory(
    client: ExoClient,
    conversation_id: str,
    trial_id: str,
    instruction: str,
    model: str,
    destination: Path,
) -> None:
    try:
        await export_trial_trajectory(
            client,
            conversation_id,
            trial_id,
            instruction,
            model,
            destination,
        )
    except Exception:
        logger.exception("failed to export Harbor trajectory for %s", trial_id)
