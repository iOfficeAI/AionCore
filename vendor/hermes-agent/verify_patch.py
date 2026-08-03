"""Build-time contract checks for the pinned Aion-managed Hermes patch."""

from __future__ import annotations

import argparse
import importlib
import os
import subprocess
import sys
import types
from pathlib import Path


FORBIDDEN_LITE_TOOLS = {
    "web_search",
    "web_extract",
    "browser_navigate",
    "browser_snapshot",
    "browser_click",
    "browser_type",
    "browser_scroll",
    "browser_back",
    "browser_press",
    "browser_get_images",
    "browser_vision",
    "browser_console",
    "browser_cdp",
    "browser_dialog",
}

REQUIRED_LITE_TOOLS = {
    "terminal",
    "process",
    "read_file",
    "write_file",
    "patch",
    "search_files",
    "todo",
    "memory",
    "session_search",
    "delegate_task",
}


def verify(source_root: Path) -> None:
    sys.path.insert(0, str(source_root))

    toolsets = importlib.import_module("toolsets")
    lite = set(toolsets.TOOLSETS["hermes-acp-lite"]["tools"])
    assert REQUIRED_LITE_TOOLS <= lite, sorted(REQUIRED_LITE_TOOLS - lite)
    assert not (FORBIDDEN_LITE_TOOLS & lite), sorted(FORBIDDEN_LITE_TOOLS & lite)

    session = importlib.import_module("acp_adapter.session")
    coding_context = importlib.import_module("agent.coding_context")
    assert session._expand_acp_enabled_toolsets(["hermes-acp-lite"], ["docs"]) == [
        "hermes-acp-lite",
        "mcp-docs",
    ]

    calls: list[dict[str, object]] = []
    runtime_provider = types.ModuleType("hermes_cli.runtime_provider")

    def fake_resolve_runtime_provider(**kwargs):
        calls.append(kwargs)
        return {"provider": "custom"}

    runtime_provider.resolve_runtime_provider = fake_resolve_runtime_provider
    hermes_cli = sys.modules.setdefault("hermes_cli", types.ModuleType("hermes_cli"))
    hermes_cli.runtime_provider = runtime_provider
    sys.modules["hermes_cli.runtime_provider"] = runtime_provider

    previous = {
        name: os.environ.get(name)
        for name in ("OPENAI_BASE_URL", "OPENAI_API_KEY")
    }
    try:
        os.environ["OPENAI_BASE_URL"] = "http://127.0.0.1:43123/v1"
        os.environ["OPENAI_API_KEY"] = "aion-build-test-key"
        result = session._managed_acp_runtime(
            requested_provider="ignored",
            config_provider="ignored",
            selected_model="aion-build-test-model",
        )
    finally:
        for name, value in previous.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value

    assert result["provider"] == "custom"
    assert calls == [
        {
            "requested": "custom",
            "explicit_api_key": "aion-build-test-key",
            "explicit_base_url": "http://127.0.0.1:43123/v1",
            "target_model": "aion-build-test-model",
        }
    ]

    entry_source = (source_root / "acp_adapter" / "entry.py").read_text(encoding="utf-8")
    assert 'os.environ.get("HERMES_ACP_SKIP_CONFIGURED_MCP") != "1"' in entry_source

    events_source = (source_root / "acp_adapter" / "events.py").read_text(encoding="utf-8")
    assert "future.add_done_callback(_log_delivery_failure)" in events_source
    assert "future.result(timeout=5)" not in events_source

    original_is_windows = coding_context.IS_WINDOWS
    original_windows_hide_flags = coding_context.windows_hide_flags
    original_run = coding_context.subprocess.run
    calls: list[dict[str, object]] = []

    class Completed:
        returncode = 0

    def fake_windows_run(command, **kwargs):
        calls.append({"command": command, **kwargs})
        stdout_file = kwargs["stdout"]
        stdout_file.write("clean\n")
        stdout_file.flush()
        return Completed()

    try:
        coding_context.IS_WINDOWS = True
        coding_context.windows_hide_flags = lambda: 0x08000000
        coding_context.subprocess.run = fake_windows_run
        coding_context._GIT_TIMEOUT_ROOTS.clear()
        assert coding_context._git(Path("C:/repo"), "status", "--short") == "clean"
        assert calls[0]["stderr"] is subprocess.DEVNULL
        assert "capture_output" not in calls[0]
        assert calls[0]["creationflags"] == 0x08000000

        timeout_calls = 0

        def fake_timeout_run(command, **kwargs):
            nonlocal timeout_calls
            timeout_calls += 1
            raise subprocess.TimeoutExpired(command, coding_context._GIT_TIMEOUT)

        coding_context.subprocess.run = fake_timeout_run
        coding_context._GIT_TIMEOUT_ROOTS.clear()
        assert coding_context._git(Path("C:/slow-repo"), "status") == ""
        assert coding_context._git(Path("C:/slow-repo"), "log") == ""
        assert timeout_calls == 1
    finally:
        coding_context.IS_WINDOWS = original_is_windows
        coding_context.windows_hide_flags = original_windows_hide_flags
        coding_context.subprocess.run = original_run
        coding_context._GIT_TIMEOUT_ROOTS.clear()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("source_root", type=Path)
    args = parser.parse_args()
    verify(args.source_root.resolve())


if __name__ == "__main__":
    main()
