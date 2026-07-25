from __future__ import annotations

import json
import logging
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Literal

from harbor.job import Job
from harbor.models.agent.context import AgentContext
from harbor.models.environment_type import EnvironmentType
from harbor.models.job.plugin import BaseJobPlugin
from harbor.models.job.result import JobResult
from harbor.models.trial.paths import TrialPaths
from harbor.models.trial.result import ExceptionInfo, TrialResult
from harbor.trial.hooks import TrialHookEvent

from exo_harbor.exo import ExoClient
from exo_harbor.protocol import (
    VerificationException,
    VerificationResult,
    send_verification_result,
)

logger = logging.getLogger(__name__)


@dataclass(frozen=True)
class ExoTrialMetadata:
    conversation_id: str
    adapter_id: str
    socket_path: Path
    conversation_mode: Literal["per_task", "shared"]


@dataclass(frozen=True)
class FeedbackSidecar:
    trial_id: str
    status: Literal["processed", "error"]
    recorded_at: str
    summary: str | None = None
    error: str | None = None
    adapter_deleted: bool = False


class ContinualExoPlugin(BaseJobPlugin):
    """Feed completed Harbor verification back to Exo before the next trial."""

    def __init__(
        self,
        *,
        exo_repo_root: str = ".",
        exo_root: str = "~/.exo",
        exo_bin: str | None = None,
        feedback_timeout_sec: float | str = 600,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        repo_root = Path(exo_repo_root).expanduser().resolve()
        self._exo_root = Path(exo_root)
        self._exo_bin = exo_bin or str(repo_root / "target/debug/exo")
        self._repo_root = repo_root
        self._feedback_timeout_sec = float(feedback_timeout_sec)
        self._job_dir: Path | None = None
        self._exo: ExoClient | None = None

    async def on_job_start(self, job: Job) -> None:
        if job.config.environment.type != EnvironmentType.DOCKER:
            raise ValueError(
                "ContinualExoPlugin requires Harbor's Docker environment "
                "(pass --env docker)"
            )
        if job.config.n_concurrent_trials != 1:
            raise ValueError(
                "ContinualExoPlugin requires one trial at a time "
                "(pass --n-concurrent 1)"
            )
        self._job_dir = job.job_dir
        self._exo = ExoClient(
            executable=self._exo_bin,
            root=self._exo_root,
            repo_root=self._repo_root,
            logs_dir=job.job_dir,
        )
        job.on_trial_ended(self._on_trial_ended)

    async def on_job_end(self, job_result: JobResult) -> None:
        return None

    async def _on_trial_ended(self, event: TrialHookEvent) -> None:
        if self._job_dir is None or self._exo is None:
            raise RuntimeError("ContinualExoPlugin has not been attached to a job")
        paths = TrialPaths(self._job_dir / event.trial_name)
        sidecar_path = paths.trial_dir / "exo-feedback.json"
        try:
            metadata = _exo_metadata(event.result)
            stdout, stderr = _verifier_output(paths, event.result)
            response = await send_verification_result(
                metadata.socket_path,
                VerificationResult(
                    trial_id=str(event.trial_id),
                    task_name=event.task_name,
                    conversation_id=metadata.conversation_id,
                    rewards=_rewards(event.result),
                    verifier_stdout=stdout,
                    verifier_stderr=stderr,
                    exception=_exception(event.result.exception_info),
                ),
                timeout_sec=self._feedback_timeout_sec,
            )
            adapter_deleted = False
            if metadata.conversation_mode == "per_task":
                await self._exo.delete_adapter(metadata.adapter_id)
                adapter_deleted = True
            sidecar = FeedbackSidecar(
                trial_id=str(event.trial_id),
                status="processed",
                recorded_at=_now(),
                summary=response.summary,
                adapter_deleted=adapter_deleted,
            )
        except Exception as error:
            logger.exception(
                "Failed to process Exo feedback for Harbor trial %s", event.trial_id
            )
            sidecar = FeedbackSidecar(
                trial_id=str(event.trial_id),
                status="error",
                recorded_at=_now(),
                error=f"{type(error).__name__}: {error}",
            )
        _write_sidecar(sidecar_path, sidecar)


def _exo_metadata(result: TrialResult) -> ExoTrialMetadata:
    contexts = [result.agent_result] if result.agent_result is not None else []
    if result.step_results:
        contexts.extend(
            step.agent_result
            for step in result.step_results
            if step.agent_result is not None
        )
    for context in reversed(contexts):
        metadata = _metadata_object(context)
        if metadata is None:
            continue
        mode = _required_string(metadata, "conversation_mode")
        if mode not in ("per_task", "shared"):
            raise ValueError("Exo conversation_mode must be per_task or shared")
        return ExoTrialMetadata(
            conversation_id=_required_string(metadata, "conversation_id"),
            adapter_id=_required_string(metadata, "adapter_id"),
            socket_path=Path(_required_string(metadata, "socket_path")),
            conversation_mode=mode,
        )
    raise ValueError("Harbor result does not contain Exo agent metadata")


def _metadata_object(context: AgentContext) -> dict[str, Any] | None:
    if context.metadata is None:
        return None
    value = context.metadata.get("exo")
    if not isinstance(value, dict):
        return None
    return value


def _rewards(result: TrialResult) -> dict[str, float | int] | None:
    if result.verifier_result is not None:
        return result.verifier_result.rewards
    if not result.step_results:
        return None
    rewards: dict[str, float | int] = {}
    for step in result.step_results:
        if step.verifier_result is None or step.verifier_result.rewards is None:
            continue
        for name, value in step.verifier_result.rewards.items():
            rewards[f"{step.step_name}.{name}"] = value
    return rewards or None


def _verifier_output(paths: TrialPaths, result: TrialResult) -> tuple[str, str]:
    if not result.step_results:
        return _read(paths.test_stdout_path), _read(paths.test_stderr_path)
    stdout: list[str] = []
    stderr: list[str] = []
    for step in result.step_results:
        step_paths = TrialPaths(paths.step_dir(step.step_name))
        step_stdout = _read(step_paths.test_stdout_path)
        step_stderr = _read(step_paths.test_stderr_path)
        if step_stdout:
            stdout.append(f"== {step.step_name} ==\n{step_stdout}")
        if step_stderr:
            stderr.append(f"== {step.step_name} ==\n{step_stderr}")
    return "\n".join(stdout), "\n".join(stderr)


def _exception(value: ExceptionInfo | None) -> VerificationException | None:
    if value is None:
        return None
    return VerificationException(
        type=value.exception_type,
        message=value.exception_message,
        traceback=value.exception_traceback,
    )


def _read(path: Path) -> str:
    if not path.exists():
        return ""
    return path.read_text(errors="replace")


def _required_string(value: dict[str, Any], key: str) -> str:
    item = value.get(key)
    if not isinstance(item, str) or not item:
        raise ValueError(f"Exo metadata {key} must be a non-empty string")
    return item


def _write_sidecar(path: Path, sidecar: FeedbackSidecar) -> None:
    temporary = path.with_suffix(f"{path.suffix}.tmp")
    temporary.write_text(json.dumps(asdict(sidecar), indent=2) + "\n")
    temporary.replace(path)


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()
