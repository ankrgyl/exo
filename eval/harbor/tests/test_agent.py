import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast
from unittest.mock import AsyncMock, patch
from uuid import UUID

from exo_harbor.agent import ExoAgent
from exo_harbor.docker import DockerContainer
from exo_harbor.exo import Adapter
from exo_harbor.protocol import TaskComplete
from harbor.models.agent.context import AgentContext


class FakeExo:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.exists = AsyncMock(side_effect=[False, False])
        self.create_agent = AsyncMock()
        self.create_conversation = AsyncMock()
        self.ensure_adapter = AsyncMock(
            return_value=Adapter(
                agent_id="agent-id",
                conversation_id="conversation-id",
                adapter_id="adapter-id",
                socket_path=root / "harbor.sock",
            )
        )
        self.ensure_runner = AsyncMock()
        self.attach = AsyncMock(return_value="sandbox-id")
        self.detach = AsyncMock()


class ExoAgentTest(unittest.IsolatedAsyncioTestCase):
    async def test_setup_run_and_detach(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "exo"
            agent = ExoAgent(
                logs_dir=Path(directory) / "logs",
                model_name="harbor-label",
                exo_root=str(root),
                exo_repo_root=directory,
                exo_model="registered-model",
            )
            fake_exo = FakeExo(root)
            agent._exo = cast(Any, fake_exo)
            agent.context_id = UUID("11111111-1111-1111-1111-111111111111")
            environment: Any = SimpleNamespace(
                context_id=agent.context_id,
                session_id="Task__trial__env",
                environment_name="example-task",
            )

            with (
                patch(
                    "exo_harbor.agent.resolve_main_container",
                    AsyncMock(
                        return_value=DockerContainer(
                            id="container-id", workdir="/workspace"
                        )
                    ),
                ),
                patch(
                    "exo_harbor.agent.send_task_started",
                    AsyncMock(
                        return_value=TaskComplete(
                            trial_id=str(agent.context_id), summary="finished"
                        )
                    ),
                ) as send,
            ):
                await agent.setup(environment)
                context = AgentContext()
                await agent.run("solve this", environment, context)

            fake_exo.create_agent.assert_awaited_once_with("harbor", "registered-model")
            fake_exo.attach.assert_awaited_once_with(
                "harbor",
                "harbor-11111111-1111-1111-1111-111111111111",
                container_id="container-id",
                default_workdir="/workspace",
            )
            send.assert_awaited_once()
            fake_exo.detach.assert_awaited_once_with(
                "harbor",
                "harbor-11111111-1111-1111-1111-111111111111",
                "sandbox-id",
            )
            assert context.metadata is not None
            self.assertEqual(context.metadata["exo"]["adapter_id"], "adapter-id")
            self.assertEqual(context.metadata["exo"]["summary"], "finished")

    async def test_run_detaches_when_adapter_request_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "exo"
            agent = ExoAgent(
                logs_dir=Path(directory) / "logs",
                model_name="registered-model",
                exo_root=str(root),
                exo_repo_root=directory,
            )
            fake_exo = FakeExo(root)
            agent._exo = cast(Any, fake_exo)
            agent.context_id = UUID("22222222-2222-2222-2222-222222222222")
            agent._conversation_slug = "conversation"
            agent._adapter = await fake_exo.ensure_adapter(
                "harbor", "conversation", root / "harbor.sock"
            )
            agent._sandbox_id = "sandbox-id"
            environment: Any = SimpleNamespace(
                context_id=agent.context_id,
                session_id="task__trial__env",
                environment_name="example-task",
            )

            with (
                patch(
                    "exo_harbor.agent.send_task_started",
                    AsyncMock(side_effect=RuntimeError("worker failed")),
                ),
                self.assertRaisesRegex(RuntimeError, "worker failed"),
            ):
                await agent.run("solve this", environment, AgentContext())

            fake_exo.detach.assert_awaited_once()
