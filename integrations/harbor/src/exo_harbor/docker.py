from __future__ import annotations

import asyncio
import json
import re
from dataclasses import dataclass


@dataclass(frozen=True)
class DockerContainer:
    id: str
    workdir: str


class DockerResolutionError(RuntimeError):
    pass


def compose_project_name(session_id: str) -> str:
    name = session_id.lower()
    if not re.match(r"^[a-z0-9]", name):
        name = f"0{name}"
    return re.sub(r"[^a-z0-9_-]", "-", name)


async def resolve_main_container(session_id: str) -> DockerContainer:
    project = compose_project_name(session_id)
    output = await _docker(
        "ps",
        "--quiet",
        "--filter",
        f"label=com.docker.compose.project={project}",
        "--filter",
        "label=com.docker.compose.service=main",
    )
    ids = [line.strip() for line in output.splitlines() if line.strip()]
    if len(ids) != 1:
        raise DockerResolutionError(
            f"expected one running Harbor main container for project {project}, "
            f"found {len(ids)}"
        )

    inspected = await _docker("inspect", ids[0])
    try:
        values = json.loads(inspected)
        if not isinstance(values, list) or len(values) != 1:
            raise ValueError("inspect result must contain one container")
        value = values[0]
        container_id = value["Id"]
        running = value["State"]["Running"]
        workdir = value["Config"]["WorkingDir"]
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        raise DockerResolutionError("unexpected docker inspect response") from error
    if not isinstance(container_id, str) or not container_id:
        raise DockerResolutionError("docker inspect returned an invalid container ID")
    if running is not True:
        raise DockerResolutionError(
            f"Harbor main container {container_id} is not running"
        )
    if not isinstance(workdir, str):
        raise DockerResolutionError(
            "docker inspect returned an invalid working directory"
        )
    return DockerContainer(id=container_id, workdir=workdir or "/")


async def _docker(*args: str) -> str:
    try:
        process = await asyncio.create_subprocess_exec(
            "docker",
            *args,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
    except FileNotFoundError as error:
        raise DockerResolutionError("docker executable was not found") from error
    stdout, stderr = await process.communicate()
    if process.returncode != 0:
        detail = stderr.decode(errors="replace").strip()
        raise DockerResolutionError(f"docker {' '.join(args)} failed: {detail}")
    return stdout.decode()
