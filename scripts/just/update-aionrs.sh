#!/usr/bin/env bash
set -euo pipefail

tag="${1:-}"
if [[ -z "$tag" ]]; then
    tag=$(
        git ls-remote --tags https://github.com/iOfficeAI/aionrs.git |
            python3 -c 'import re, sys; tags=[]; [tags.append(m.group(1)) for line in sys.stdin for m in [re.search(r"refs/tags/(v[0-9]+(?:\.[0-9]+)*(?:[-+][0-9A-Za-z.-]+)?)$", line)] if m]; print(sorted(tags, key=lambda t: [int(p) if p.isdigit() else p for p in re.split(r"[.-]", t.lstrip("v"))])[-1])'
    )
    echo "Using latest tag: $tag"
fi

python3 - "$tag" <<'PY'
from pathlib import Path
import re
import sys

tag = sys.argv[1]
path = Path("Cargo.toml")
text = path.read_text()
updated = re.sub(
    r'git = "https://github\.com/iOfficeAI/aionrs\.git", tag = "[^"]*"',
    f'git = "https://github.com/iOfficeAI/aionrs.git", tag = "{tag}"',
    text,
)
if not re.search(r'git = "https://github\.com/iOfficeAI/aionrs\.git", tag = "[^"]*"', text):
    raise SystemExit("No aionrs git dependency tags found in Cargo.toml")
path.write_text(updated)
PY

cargo check --workspace
