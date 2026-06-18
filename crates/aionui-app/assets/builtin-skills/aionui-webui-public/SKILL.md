---
name: aionui-webui-public
description: Expose the user's local AionUi WebUI to the public internet with a near-zero-effort flow. Detects whether the WebUI is running, guides the user to switch it on if needed (the only manual step), self-installs cloudflared cross-platform, opens a Cloudflare quick tunnel, verifies the public URL actually works, then explains the limitations (URL is temporary, must stay running). Use whenever the user wants to reach their AionUi from outside the LAN, over the internet, or share a public link. Distinct from aionui-webui-setup (which covers manual LAN / Tailscale / server config through the settings UI): this skill produces a one-click public link via an automatic tunnel.
---

> **⚠️ Platform note — read before running any command.** The shell snippets in this skill are written for **macOS / Linux** (bash/zsh). Always check which OS you are on first. On **Windows** do **not** run them verbatim — the underlying tool/CLI commands are usually cross-platform, but the surrounding shell syntax is not. Translate it to PowerShell before running:
>
> | bash (macOS / Linux) | PowerShell (Windows) |
> | --- | --- |
> | `a && b` | run as two steps, or `a; if ($?) { b }` |
> | `cat <<'EOF' \| tool …` (heredoc) | write the text to a temp file, then pipe/pass that file to the tool |
> | `VAR=$(cmd)` … `$VAR` | `$VAR = cmd` … `$VAR` |
> | `cmd > /dev/null` | `cmd > $null` |
> | `… \| grep PAT` | `… \| Select-String PAT` |
> | `… \| jq …` | `… \| ConvertFrom-Json`, then read the fields |
> | `python3 x.py` | `python x.py` (or `py x.py`) |
> | `~/dir`, `/tmp` | `$env:USERPROFILE\dir`, `$env:TEMP` |
> | `cp` / `mkdir -p` / `rm -rf` | `Copy-Item` / `New-Item -ItemType Directory -Force` / `Remove-Item -Recurse -Force` |
>
> If a command has no obvious Windows equivalent, prefer the built-in file/HTTP tools over raw shell.

# AionUi WebUI Public Access Guide

You help a user turn their local AionUi WebUI (LAN-only at best) into a public internet URL, with the user doing almost nothing. You have a shell (Bash) and run on the same machine as AionUi, so you do the work yourself. The user only does what you architecturally cannot: flip the WebUI toggle in the desktop UI.

## Core facts (verified, do not re-derive)

- AionUi WebUI is a local HTTP server on port 25808 (prod; dev 25809). It has built-in user+password / JWT auth, so exposing it publicly is reasonably safe.
- There is NO HTTP/CLI way to start the desktop WebUI. Starting it is Electron-IPC only, so you cannot turn it on; you must guide the user to the toggle. You CAN detect its state, install the tunnel, run the tunnel, and verify, all yourself.
- The tunnel tool is cloudflared (Cloudflare quick tunnel, no account needed). It must be forced to --protocol http2 (see Gotcha).
- Password changes DO have HTTP routes you can call for the user (see "Optional: change credentials").

## The flow

Work through these steps in order. Narrate what you are doing in the user's language. Do not dump raw commands on the user unless they ask; you run them.

### Step 1 - Detect whether the WebUI is running

Run: `curl -s -o /dev/null -w "%{http_code}" --max-time 5 http://127.0.0.1:25808/`

- 200 means WebUI is up. Go to Step 3.
- 000 / connection refused means WebUI is off. Go to Step 2.

### Step 2 - Guide the user to turn on the WebUI (the only manual step)

Tell the user, in their language, something like:

"The WebUI is not running yet, and I cannot switch it on for you. Please open it manually: Settings -> WebUI -> toggle it on. (If you want LAN access too, also enable Allow remote access.) Tell me once it is on and I will continue."

After they say it is on, re-run Step 1 to confirm. Only proceed when you see 200. If still 000, ask them to double-check the toggle.

### Step 3 - Make sure cloudflared is installed (self-install, cross-platform)

First check if it is already available: `command -v cloudflared`

If missing, install it without depending on the user's package manager: download the official prebuilt binary directly. Detect the platform and pick the asset:

- macOS / Linux: `https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-<goos>-<goarch>.tgz` where goos = darwin or linux, goarch = arm64 (Apple Silicon / aarch64) or amd64 (x86_64). Then `tar xzf` to get the cloudflared binary.
- Windows: download `cloudflared-windows-<arch>.exe` (raw binary, no archive).

Example for macOS/Linux (put it in a stable spot so a restart can reuse it):

```bash
mkdir -p ~/.aionui/tools && cd ~/.aionui/tools
OS=$(uname -s); ARCH=$(uname -m)
case "$OS" in Darwin) goos=darwin;; Linux) goos=linux;; esac
case "$ARCH" in arm64|aarch64) goarch=arm64;; x86_64|amd64) goarch=amd64;; esac
curl -fsSL -o cf.tgz "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-${goos}-${goarch}.tgz"
tar xzf cf.tgz && rm cf.tgz
./cloudflared --version
```

`brew install cloudflared` also works on macOS but is not cross-platform; prefer the direct binary so this works on any host.

### Step 4 - Open the tunnel (MUST force http2)

Run cloudflared as a long-lived background process pointing at the local WebUI:

```bash
cloudflared tunnel --protocol http2 --url http://127.0.0.1:25808
```

(Use the path to the binary you installed, e.g. `~/.aionui/tools/cloudflared`.)

Watch its output for two things:
1. The public URL line: `https://<random-words>.trycloudflare.com`
2. A line `Registered tunnel connection ... protocol=http2`. This means it actually connected.

Gotcha - QUIC / HTTP 530: without `--protocol http2`, cloudflared defaults to QUIC over UDP port 7844, which many networks block. The tunnel then never registers and the public URL returns HTTP 530 forever while cloudflared silently retries. The startup pre-check shows `UDP Connectivity ... FAIL` / `TCP Connectivity ... PASS`. Always pass `--protocol http2`. If you ever see persistent 530, this is the cause; confirm the flag is set.

### Step 5 - Verify the public URL really works (do not skip)

Before handing the URL to the user, prove it from the public side:

```bash
curl -s -o /dev/null -w "%{http_code}" --max-time 20 "<public-url>/"
```

Retry 2-3 times with a few seconds between; a freshly created tunnel can return 530/000 for the first few seconds before the edge is ready. Consider it good only when you get 200. For extra confidence, confirm it is really AionUi:

```bash
curl -s --max-time 20 "<public-url>/" | grep -i "<title>AionUi</title>"
```

### Step 6 - Hand off the URL and explain the limitations clearly

Give the user the public URL, and tell them plainly (in their language):

- Log in with your WebUI username + password when you open the link. The WebUI is auth-protected; that is what keeps it safe on the public internet.
- The URL is temporary. It changes if the tunnel restarts, and if you restart the WebUI service the tunnel breaks too. In either case, ask me again and I will generate a fresh URL.
- Keep it running. The tunnel is a process on this machine; if you close it (or shut down/sleep the computer), the public URL stops working.

### Optional - change the WebUI username / password for the user

If the user wants to change credentials (e.g. before sharing the link), you can do it via the backend API (no UI needed):

```bash
curl -s -X POST http://127.0.0.1:25808/api/webui/change-password -H "Content-Type: application/json" -d '{"new_password":"<new>"}'
curl -s -X POST http://127.0.0.1:25808/api/webui/change-username -H "Content-Type: application/json" -d '{"new_username":"<new>"}'
```

(Confirm exact field names from the response if it errors; reset-password / generate-qr-token endpoints also exist.)

### Optional - a permanent / fixed address (mention only, do not push)

Only bring this up after the user has experienced the free public access and asks for a URL that does not change. A fixed address requires a Cloudflare account + your own domain wired to a cloudflared named tunnel (more setup, and a custom domain is paid). The free *.trycloudflare.com URL is always random/ephemeral. Recommend the named-tunnel path as an option, but do not set it up unless they explicitly want it.

## Style

- Do the technical work yourself; keep the user's part to the single WebUI toggle.
- Verify before you promise; never hand over a URL you have not seen return 200.
- Be honest about the ephemeral nature up front, not after they complain it broke.
