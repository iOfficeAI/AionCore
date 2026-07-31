"""Run a real ACP turn against an expanded Aion-managed Hermes runtime.

The test uses the ACP SDK shipped inside the runtime and a local
OpenAI-compatible streaming endpoint. It never sends prompts or credentials to
the public network. When --capture is provided, the emitted JSON is sanitized
and can be retained as protocol evidence.
"""

from __future__ import annotations

import sys

sys.dont_write_bytecode = True

import argparse
import asyncio
import json
import os
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


REPLY = "HERMES_E2E_OK"
PROMPT = "Reply with HERMES_E2E_OK and do not call tools."
MODEL = "aion-hermes-e2e-model"
FORBIDDEN_TOOL_PREFIXES = ("browser_", "web_")
REQUIRED_TOOLS = {"read_file", "terminal"}


class EndpointState:
    def __init__(self, secret: str) -> None:
        self.secret = secret
        self.requests: list[dict[str, Any]] = []


def _tool_names(body: dict[str, Any]) -> list[str]:
    names: list[str] = []
    for tool in body.get("tools") or []:
        if not isinstance(tool, dict):
            continue
        function = tool.get("function")
        if isinstance(function, dict) and isinstance(function.get("name"), str):
            names.append(function["name"])
    return sorted(names)


def _handler_for(state: EndpointState) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
            length = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(length) or b"{}")
            authorization = self.headers.get("Authorization", "")
            state.requests.append(
                {
                    "path": self.path,
                    "model": body.get("model"),
                    "stream": body.get("stream"),
                    "messageCount": len(body.get("messages") or []),
                    "toolNames": _tool_names(body),
                    "authorizationAccepted": authorization == f"Bearer {state.secret}",
                }
            )

            if self.path.rstrip("/") != "/v1/chat/completions":
                payload = json.dumps({"error": {"message": "unexpected path"}}).encode()
                self.send_response(404)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                return

            if body.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache")
                self.send_header("Connection", "close")
                self.end_headers()
                chunks = [
                    {
                        "id": "chatcmpl-aion-hermes-e2e",
                        "object": "chat.completion.chunk",
                        "created": 1,
                        "model": MODEL,
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"role": "assistant", "content": REPLY},
                                "finish_reason": None,
                            }
                        ],
                    },
                    {
                        "id": "chatcmpl-aion-hermes-e2e",
                        "object": "chat.completion.chunk",
                        "created": 1,
                        "model": MODEL,
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                        "usage": {
                            "prompt_tokens": 10,
                            "completion_tokens": 4,
                            "total_tokens": 14,
                        },
                    },
                ]
                for chunk in chunks:
                    self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                    self.wfile.flush()
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                self.close_connection = True
                return

            payload = json.dumps(
                {
                    "id": "chatcmpl-aion-hermes-e2e",
                    "object": "chat.completion",
                    "created": 1,
                    "model": MODEL,
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": REPLY},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 4,
                        "total_tokens": 14,
                    },
                }
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    return Handler


class RecordingClient:
    def __init__(self) -> None:
        self.updates: list[dict[str, Any]] = []

    async def session_update(self, session_id: str, update: Any, **_kwargs: Any) -> None:
        self.updates.append(
            {
                "sessionId": session_id,
                "update": update.model_dump(mode="json", by_alias=True, exclude_none=True),
            }
        )

    async def request_permission(self, **_kwargs: Any) -> Any:
        raise AssertionError("Hermes unexpectedly requested permission in the no-tool smoke turn")

    async def write_text_file(self, **_kwargs: Any) -> Any:
        raise AssertionError("Hermes unexpectedly requested a file write in the no-tool smoke turn")

    async def read_text_file(self, **_kwargs: Any) -> Any:
        raise AssertionError("Hermes unexpectedly requested a file read in the no-tool smoke turn")


def _extract_agent_text(updates: list[dict[str, Any]]) -> str:
    parts: list[str] = []
    for notification in updates:
        update = notification.get("update") or {}
        if update.get("sessionUpdate") != "agent_message_chunk":
            continue
        content = update.get("content") or {}
        if content.get("type") == "text" and isinstance(content.get("text"), str):
            parts.append(content["text"])
    return "".join(parts)


def _sanitize(value: Any, replacements: dict[str, str]) -> Any:
    if isinstance(value, dict):
        return {key: _sanitize(item, replacements) for key, item in value.items()}
    if isinstance(value, list):
        return [_sanitize(item, replacements) for item in value]
    if isinstance(value, str):
        sanitized = value
        for original, replacement in replacements.items():
            sanitized = sanitized.replace(original, replacement)
        return sanitized
    return value


async def _read_stderr(process: asyncio.subprocess.Process) -> list[str]:
    if process.stderr is None:
        return []
    lines: list[str] = []
    while line := await process.stderr.readline():
        lines.append(line.decode("utf-8", errors="replace").rstrip())
    return lines


async def run(runtime_root: Path, capture: Path | None) -> dict[str, Any]:
    python = runtime_root / "python" / "python.exe"
    bash = runtime_root / "tools" / "git" / "bin" / "bash.exe"
    git = runtime_root / "tools" / "git" / "cmd"
    rg = runtime_root / "tools" / "rg"
    for required in (python, bash, git / "git.exe", rg / "rg.exe"):
        if not required.is_file():
            raise AssertionError(f"managed runtime is missing {required}")

    from acp import PROTOCOL_VERSION
    from acp.connection import StreamEvent
    from acp.schema import Implementation, TextContentBlock
    from acp.stdio import spawn_agent_process

    secret = "aion-hermes-e2e-secret"
    endpoint_state = EndpointState(secret)
    server = ThreadingHTTPServer(("127.0.0.1", 0), _handler_for(endpoint_state))
    server_thread = threading.Thread(target=server.serve_forever, daemon=True, name="hermes-e2e-endpoint")
    server_thread.start()

    wire: list[dict[str, Any]] = []

    def observe(event: StreamEvent) -> None:
        wire.append({"direction": event.direction.value, "message": event.message})

    try:
        with tempfile.TemporaryDirectory(prefix="Aion Hermes ACP 中文 ") as temporary:
            temp_root = Path(temporary)
            hermes_home = temp_root / "Hermes 会话"
            workspace = temp_root / "ACP 工作区"
            hermes_home.mkdir()
            workspace.mkdir()
            (hermes_home / "config.yaml").write_text(
                "security:\n"
                "  allow_lazy_installs: false\n"
                "auxiliary:\n"
                "  title_generation:\n"
                "    enabled: false\n",
                encoding="utf-8",
            )

            env = {
                "OPENAI_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
                "OPENAI_API_KEY": secret,
                "HERMES_INFERENCE_MODEL": MODEL,
                "HERMES_HOME": str(hermes_home),
                "HERMES_GIT_BASH_PATH": str(bash),
                "HERMES_DISABLE_LAZY_INSTALLS": "1",
                "HERMES_ACP_SKIP_CONFIGURED_MCP": "1",
                "HERMES_ACP_TOOLSET": "hermes-acp-lite",
                "PYTHONDONTWRITEBYTECODE": "1",
                "PYTHONNOUSERSITE": "1",
                "PYTHONSAFEPATH": "1",
                "PYTHONIOENCODING": "utf-8",
                "PYTHONUTF8": "1",
                "PATH": os.pathsep.join([str(rg), str(git), os.environ.get("PATH", "")]),
            }
            client = RecordingClient()
            started = time.monotonic()
            stderr_lines: list[str] = []
            async with spawn_agent_process(
                client,
                str(python),
                "-P",
                "-m",
                "acp_adapter",
                env=env,
                cwd=workspace,
                observers=[observe],
            ) as (connection, process):
                stderr_task = asyncio.create_task(_read_stderr(process))
                initialized = await asyncio.wait_for(
                    connection.initialize(
                        protocol_version=PROTOCOL_VERSION,
                        client_info=Implementation(name="aion-hermes-smoke", version="1.0.0"),
                    ),
                    timeout=30,
                )
                session = await asyncio.wait_for(connection.new_session(cwd=str(workspace), mcp_servers=[]), timeout=60)
                response = await asyncio.wait_for(
                    connection.prompt(
                        prompt=[TextContentBlock(type="text", text=PROMPT)],
                        session_id=session.session_id,
                    ),
                    timeout=120,
                )
                await asyncio.sleep(0.2)
            stderr_lines = await asyncio.wait_for(stderr_task, timeout=5)

            if secret in "\n".join(stderr_lines):
                raise AssertionError("Hermes stderr exposed the provider credential")
            if response.stop_reason != "end_turn":
                raise AssertionError(f"unexpected ACP stop reason: {response.stop_reason}")
            if REPLY not in _extract_agent_text(client.updates):
                raise AssertionError("ACP session updates did not contain the model response")
            model_requests = [
                request
                for request in endpoint_state.requests
                if request["path"].rstrip("/") == "/v1/chat/completions"
            ]
            if len(model_requests) != 1:
                raise AssertionError(
                    "expected one model endpoint request, got "
                    f"{json.dumps(endpoint_state.requests, ensure_ascii=False)}"
                )

            request = model_requests[0]
            if request["model"] != MODEL:
                raise AssertionError(f"model propagation failed: {request['model']}")
            if not request["authorizationAccepted"]:
                raise AssertionError("provider credential was not propagated through the child environment")
            tools = set(request["toolNames"])
            forbidden = sorted(name for name in tools if name.startswith(FORBIDDEN_TOOL_PREFIXES))
            if forbidden:
                raise AssertionError(f"forbidden browser/web tools reached the model request: {forbidden}")
            missing = sorted(REQUIRED_TOOLS - tools)
            if missing:
                raise AssertionError(f"required lite tools were absent from the model request: {missing}")

            replacements = {
                str(runtime_root): "<runtime-root>",
                str(temp_root): "<temporary-root>",
                str(workspace): "<workspace>",
                str(hermes_home): "<hermes-home>",
                session.session_id: "<session-id>",
                secret: "<redacted>",
                f"127.0.0.1:{server.server_port}": "127.0.0.1:<port>",
            }
            result = {
                "fixture": "hermes-agent/0.19.0+aion.1/win32-x64",
                "runtimeRoot": str(runtime_root),
                "elapsedSeconds": round(time.monotonic() - started, 3),
                "initialize": initialized.model_dump(mode="json", by_alias=True, exclude_none=True),
                "sessionId": session.session_id,
                "promptStopReason": response.stop_reason,
                "agentText": _extract_agent_text(client.updates),
                "modelEndpointRequests": endpoint_state.requests,
                "stderr": {
                    "lineCount": len(stderr_lines),
                    "credentialPresent": False,
                },
                "acpWire": wire,
            }
            result = _sanitize(result, replacements)
            if capture is not None:
                capture.parent.mkdir(parents=True, exist_ok=True)
                capture.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            return result
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("runtime_root", type=Path)
    parser.add_argument("--capture", type=Path)
    args = parser.parse_args()
    result = asyncio.run(run(args.runtime_root.resolve(), args.capture))
    print(
        json.dumps(
            {
                "ok": True,
                "fixture": result["fixture"],
                "promptStopReason": result["promptStopReason"],
                "agentText": result["agentText"],
                "requestCount": len(result["modelEndpointRequests"]),
                "modelRequestCount": sum(
                    request["path"].rstrip("/") == "/v1/chat/completions"
                    for request in result["modelEndpointRequests"]
                ),
                "toolCount": max(
                    len(request["toolNames"]) for request in result["modelEndpointRequests"]
                ),
            },
            ensure_ascii=False,
        )
    )


if __name__ == "__main__":
    main()
