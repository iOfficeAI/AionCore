#!/usr/bin/env bash
set -euo pipefail

if [[ -n "$(git diff --name-only)" ]]; then
    git add -A
    git commit -m "chore: apply auto-fixes (fmt + clippy)"
fi
