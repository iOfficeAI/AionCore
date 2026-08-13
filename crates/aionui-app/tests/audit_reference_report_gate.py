#!/usr/bin/env python3
"""Local gate: director report audit must require Aion experience markers."""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

TESTS_DIR = Path(__file__).resolve().parent
APP_DIR = TESTS_DIR.parent
SCRIPTS = APP_DIR / "assets" / "builtin-skills" / "threejs-game-director" / "scripts"
PYTHON_SCRIPT = SCRIPTS / "audit_reference_report.py"
NODE_SCRIPT = SCRIPTS / "audit_reference_report.mjs"
FIXTURES = TESTS_DIR / "fixtures" / "audit-reference-report"


def run_audit(command: list[str], report: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [*command, str(report)],
        check=False,
        capture_output=True,
        text=True,
    )


def check_pair(label: str, command: list[str]) -> None:
    passed = run_audit(command, FIXTURES / "pass.md")
    if passed.returncode != 0:
        raise SystemExit(f"{label}: expected pass.md to pass, got:\n{passed.stdout or passed.stderr}")

    failed = run_audit(command, FIXTURES / "fail-missing-beat-share.md")
    if failed.returncode == 0:
        raise SystemExit(f"{label}: expected fail-missing-beat-share.md to fail")

    output = (failed.stdout + failed.stderr).lower()
    if "emotion beat" not in output or "share" not in output:
        raise SystemExit(
            f"{label}: fail fixture must report missing emotion beat and share:\n{failed.stdout or failed.stderr}"
        )


def main() -> int:
    if not PYTHON_SCRIPT.is_file():
        print(f"missing audit script: {PYTHON_SCRIPT}", file=sys.stderr)
        return 1

    check_pair("python", [sys.executable, str(PYTHON_SCRIPT)])

    if NODE_SCRIPT.is_file() and shutil.which("node"):
        check_pair("node", ["node", str(NODE_SCRIPT)])
    elif not NODE_SCRIPT.is_file():
        print(f"missing node audit script: {NODE_SCRIPT}", file=sys.stderr)
        return 1

    print("audit_reference_report experience gate fixtures passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
