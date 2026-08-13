#!/usr/bin/env python3
"""launch_game.mjs treats an already-serving 5188 as success without starting Vite."""

from __future__ import annotations

import http.server
import socketserver
import subprocess
import sys
import threading
from pathlib import Path

APP_DIR = Path(__file__).resolve().parents[1]
SCRIPT = (
    APP_DIR
    / "assets"
    / "builtin-skills"
    / "threejs-gameplay-systems"
    / "scripts"
    / "launch_game.mjs"
)
SCAFFOLD = (
    APP_DIR
    / "assets"
    / "builtin-skills"
    / "threejs-gameplay-systems"
    / "assets"
    / "threejs-vite-game"
)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, format: str, *args: object) -> None:
        return


class ReusableServer(socketserver.TCPServer):
    allow_reuse_address = True


def port_open() -> bool:
    import urllib.error
    import urllib.request

    try:
        urllib.request.urlopen("http://127.0.0.1:5188/", timeout=1)
        return True
    except urllib.error.HTTPError:
        return True
    except OSError:
        return False


def main() -> int:
    if not SCRIPT.is_file():
        print(f"missing {SCRIPT}", file=sys.stderr)
        return 1

    httpd = None
    if not port_open():
        httpd = ReusableServer(("127.0.0.1", 5188), Handler)
        thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        thread.start()
    try:
        result = subprocess.run(
            ["node", str(SCRIPT), "--no-open", str(SCAFFOLD)],
            check=False,
            capture_output=True,
            text=True,
        )
    finally:
        if httpd is not None:
            httpd.shutdown()
            httpd.server_close()

    if result.returncode != 0:
        print(result.stdout or result.stderr, file=sys.stderr)
        return 1
    if "LAUNCH_OK" not in result.stdout or "already_running=1" not in result.stdout:
        print(result.stdout or result.stderr, file=sys.stderr)
        return 1
    print("launch_game already-running gate passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
