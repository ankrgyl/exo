"""Host-side resolution of the Docker container Harbor started for a trial.

Harbor's DockerEnvironment brings the task up with Docker Compose under a
project name derived from the public ``environment.session_id``, with the task
container as the ``main`` service. We find it by label from the host.

Deliberately not done via ``BaseEnvironment.exec()``: that runs *inside* the
task container, where recovering the host-side container identity is
unreliable. This is host work and belongs in the external agent.
"""

from __future__ import annotations

import re
import subprocess
from dataclasses import dataclass

MAIN_SERVICE = "main"
PROJECT_LABEL = "com.docker.compose.project"
SERVICE_LABEL = "com.docker.compose.service"


class DockerResolutionError(RuntimeError):
    pass


@dataclass(frozen=True)
class DockerContainer:
    container_id: str
    project: str


def compose_project_name(session_id: str) -> str:
    """Reproduce Harbor's Compose project-name normalization.

    1. lowercase
    2. prefix "0" if the first character is not alphanumeric
    3. replace anything outside [a-z0-9_-] with "-"

    Mirrors Harbor's own logic, so it must be re-checked on a Harbor bump.
    """
    name = session_id.lower()
    if not name or not name[0].isalnum():
        name = f"0{name}"
    return re.sub(r"[^a-z0-9_-]", "-", name)


def resolve_main_container(session_id: str) -> DockerContainer:
    """Find the single running ``main`` container for this trial.

    Zero matches means the environment is gone. More than one is ambiguous.
    Both raise: submitting an arbitrary container would silently grade the wrong
    machine, which is worse than failing the trial.
    """
    project = compose_project_name(session_id)
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
        raise DockerResolutionError(
            f"no running {MAIN_SERVICE} container for Compose project {project!r}"
        )
    if len(ids) > 1:
        raise DockerResolutionError(
            f"{len(ids)} running {MAIN_SERVICE} containers for Compose project "
            f"{project!r}; refusing to guess"
        )

    container_id = ids[0]
    # Re-read the labels off the chosen container. `docker ps` filtering and
    # this inspect are separate calls, and the trial is about to be graded on
    # whatever we submit.
    labels = _docker(
        "inspect",
        "--format",
        f"{{{{index .Config.Labels \"{PROJECT_LABEL}\"}}}} "
        f"{{{{index .Config.Labels \"{SERVICE_LABEL}\"}}}}",
        container_id,
    ).split()
    if labels != [project, MAIN_SERVICE]:
        raise DockerResolutionError(
            f"container {container_id} no longer matches {project}/{MAIN_SERVICE}"
        )

    return DockerContainer(container_id=container_id, project=project)


def _docker(*args: str) -> str:
    result = subprocess.run(
        ["docker", *args], capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise DockerResolutionError(
            f"docker {' '.join(args)} failed: {result.stderr.strip()}"
        )
    return result.stdout.strip()
