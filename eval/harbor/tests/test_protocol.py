from __future__ import annotations

import asyncio
import json
import tempfile
import unittest
from pathlib import Path

from exo_harbor.protocol import TrialRun, send_trial_run


class TrialProtocolTest(unittest.IsolatedAsyncioTestCase):
    async def test_cancellation_propagates_and_closes_socket(self) -> None:
        request_received = asyncio.Event()
        client_disconnected = asyncio.Event()

        with tempfile.TemporaryDirectory() as directory:
            socket_path = Path(directory) / "trial.sock"

            async def wait_forever(
                reader: asyncio.StreamReader, writer: asyncio.StreamWriter
            ) -> None:
                await reader.readline()
                request_received.set()
                await reader.read()
                client_disconnected.set()
                writer.close()
                await writer.wait_closed()

            server = await asyncio.start_unix_server(wait_forever, path=socket_path)
            task = asyncio.create_task(
                send_trial_run(
                    socket_path,
                    TrialRun(
                        request_id="request-1",
                        target="trial-1",
                        container_id="container-1",
                        instructions="Fix it",
                    ),
                    timeout_sec=None,
                )
            )
            await asyncio.wait_for(request_received.wait(), timeout=1)
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
            await asyncio.wait_for(client_disconnected.wait(), timeout=1)
            server.close()
            await server.wait_closed()

    async def test_reconnects_with_same_request_after_worker_restart(self) -> None:
        request = TrialRun(
            request_id="request-1",
            target="trial-1",
            container_id="container-1",
            instructions="Fix it",
        )
        received: list[dict] = []

        with tempfile.TemporaryDirectory() as directory:
            socket_path = Path(directory) / "trial.sock"

            async def disconnect(
                reader: asyncio.StreamReader, writer: asyncio.StreamWriter
            ) -> None:
                received.append(json.loads(await reader.readline()))
                writer.close()
                await writer.wait_closed()

            first = await asyncio.start_unix_server(disconnect, path=socket_path)
            task = asyncio.create_task(
                send_trial_run(socket_path, request, timeout_sec=5)
            )
            while not received:
                await asyncio.sleep(0.01)
            first.close()
            await first.wait_closed()
            socket_path.unlink(missing_ok=True)

            async def complete(
                reader: asyncio.StreamReader, writer: asyncio.StreamWriter
            ) -> None:
                received.append(json.loads(await reader.readline()))
                writer.write(
                    (
                        json.dumps(
                            {
                                "type": "response",
                                "event": {
                                    "type": "trial_complete",
                                    "request_id": "request-1",
                                    "target": "trial-1",
                                    "conversation_id": "conversation-1",
                                    "summary": "done",
                                },
                            }
                        )
                        + "\n"
                    ).encode()
                )
                await writer.drain()
                writer.close()
                await writer.wait_closed()

            second = await asyncio.start_unix_server(complete, path=socket_path)
            response = await task
            second.close()
            await second.wait_closed()

        self.assertEqual(received, [request.payload(), request.payload()])
        self.assertEqual(response.conversation_id, "conversation-1")
        self.assertEqual(response.summary, "done")


if __name__ == "__main__":
    unittest.main()
