# Aion managed Hermes runtime

This directory pins and patches the Hermes ACP adapter bundled in the Windows
x64 managed-resources pack.

- Upstream: `NousResearch/hermes-agent`
- Release: `v2026.7.20` / package `0.19.0`
- Commit: `3ef6bbd201263d354fd83ec55b3c306ded2eb72a`
- Aion patch: `aion-managed.patch`

The upstream sources used to establish the launch contract are:

- `acp_adapter/__main__.py`: `python -m acp_adapter` entry point.
- `acp_adapter/entry.py`: ACP startup and configured MCP discovery.
- `acp_adapter/session.py`: ACP model/provider and toolset selection.
- `hermes_cli/runtime_provider.py`: explicit custom provider resolver.
- `toolsets.py`: the upstream `hermes-acp` tool inventory.
- `tools/environments/local.py`: `HERMES_GIT_BASH_PATH` handling.

The patch is intentionally narrow. It adds a browser/web-free
`hermes-acp-lite` toolset, lets an embedding host select that toolset, skips
only globally configured MCP discovery when the host owns `session/new`
injection, and makes the session-scoped OpenAI-compatible endpoint, API key,
and model authoritative without writing them to Hermes configuration files.

`runtime-lock.json` pins every downloaded input. The build script verifies the
official PyPI sdist and wheel checksums, applies the patch to the sdist with
context checks, installs a committed hash-locked dependency export generated
from the upstream `uv.lock`, overlays only the three patched files onto the
official wheel installation, and validates the installed adapter before
export. This avoids both GitHub's dynamically generated source archives and an
unpinned PEP 517 build environment.
