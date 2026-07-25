import asyncio
import json
import tempfile
import unittest
from pathlib import Path

from exo_harbor.protocol import (
    TaskStarted,
    VerificationResult,
    send_task_started,
    send_verification_result,
)


class ProtocolTest(unittest.IsolatedAsyncioTestCase):
    async def test_task_started_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            socket = Path(directory) / "harbor.sock"
            received: dict[str, object] = {}

            async def handle(
                reader: asyncio.StreamReader, writer: asyncio.StreamWriter
            ) -> None:
                received.update(json.loads(await reader.readline()))
                writer.write(
                    json.dumps(
                        {
                            "type": "response",
                            "event": {
                                "type": "task_complete",
                                "trial_id": "trial-1",
                                "summary": "done",
                            },
                        }
                    ).encode()
                    + b"\n"
                )
                await writer.drain()
                writer.close()
                await writer.wait_closed()

            server = await asyncio.start_unix_server(handle, socket)
            try:
                response = await send_task_started(
                    socket,
                    TaskStarted(
                        trial_id="trial-1",
                        task_name="task",
                        instruction="fix it",
                        conversation_id="conversation-1",
                        sandbox_id="sandbox-1",
                    ),
                    timeout_sec=1,
                )
            finally:
                server.close()
                await server.wait_closed()

            self.assertEqual(received["type"], "task_started")
            self.assertEqual(received["instruction"], "fix it")
            self.assertEqual(response.summary, "done")

    async def test_verification_result_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            socket = Path(directory) / "harbor.sock"
            received: dict[str, object] = {}

            async def handle(
                reader: asyncio.StreamReader, writer: asyncio.StreamWriter
            ) -> None:
                received.update(json.loads(await reader.readline()))
                writer.write(
                    json.dumps(
                        {
                            "type": "response",
                            "event": {
                                "type": "feedback_processed",
                                "trial_id": "trial-1",
                                "summary": "learned",
                            },
                        }
                    ).encode()
                    + b"\n"
                )
                await writer.drain()
                writer.close()
                await writer.wait_closed()

            server = await asyncio.start_unix_server(handle, socket)
            try:
                response = await send_verification_result(
                    socket,
                    VerificationResult(
                        trial_id="trial-1",
                        task_name="task",
                        conversation_id="conversation-1",
                        rewards={"reward": 1},
                    ),
                    timeout_sec=1,
                )
            finally:
                server.close()
                await server.wait_closed()

            self.assertEqual(received["type"], "verification_result")
            self.assertEqual(received["rewards"], {"reward": 1})
            self.assertEqual(response.summary, "learned")
