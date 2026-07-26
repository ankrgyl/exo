import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast
from unittest.mock import AsyncMock, patch
from uuid import UUID

from exo_harbor.agent import ExoAgent
from exo_harbor.docker import DockerContainer
from exo_harbor.protocol import TaskComplete
from harbor.models.agent.context import AgentContext


class FakeExo:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.exists = AsyncMock(side_effect=[False, False, False])
        self.create_agent = AsyncMock()
        self.create_conversation = AsyncMock()
        self.ensure_harbor_adapter = AsyncMock()
        self.ensure_runner = AsyncMock()
        self.conversation_id = AsyncMock(return_value="conversation-id")
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
                    "exo_harbor.agent.probe",
                    AsyncMock(return_value=False),
                ),
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
            fake_exo.ensure_harbor_adapter.assert_awaited_once_with(
                "harbor", "harbor-setup", root / "harbor.sock"
            )
            self.assertEqual(
                fake_exo.create_conversation.await_args_list[0].args,
                ("harbor", "harbor-setup"),
            )
            self.assertEqual(
                fake_exo.create_conversation.await_args_list[1].args,
                (
                    "harbor",
                    "harbor-11111111-1111-1111-1111-111111111111",
                ),
            )
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
            self.assertNotIn("adapter_id", context.metadata["exo"])
            self.assertEqual(
                context.metadata["exo"]["conversation_id"], "conversation-id"
            )
            self.assertEqual(context.metadata["exo"]["summary"], "finished")

    async def test_shared_mode_reuses_a_work_conversation_not_setup(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "exo"
            agent = ExoAgent(
                logs_dir=Path(directory) / "logs",
                model_name="registered-model",
                exo_root=str(root),
                exo_repo_root=directory,
                conversation_mode="shared",
            )
            fake_exo = FakeExo(root)
            agent._exo = cast(Any, fake_exo)
            agent.context_id = UUID("33333333-3333-3333-3333-333333333333")
            environment: Any = SimpleNamespace(
                context_id=agent.context_id,
                session_id="task__trial__env",
                environment_name="example-task",
            )

            with (
                patch("exo_harbor.agent.probe", AsyncMock(return_value=True)),
                patch(
                    "exo_harbor.agent.resolve_main_container",
                    AsyncMock(
                        return_value=DockerContainer(
                            id="container-id", workdir="/workspace"
                        )
                    ),
                ),
            ):
                await agent.setup(environment)

            self.assertEqual(
                [call.args for call in fake_exo.create_conversation.await_args_list],
                [("harbor", "harbor-setup"), ("harbor", "harbor")],
            )
            fake_exo.ensure_harbor_adapter.assert_not_awaited()
            fake_exo.attach.assert_awaited_once_with(
                "harbor",
                "harbor",
                container_id="container-id",
                default_workdir="/workspace",
            )

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
            agent._conversation_id = "conversation-id"
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
