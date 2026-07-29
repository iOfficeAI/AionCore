#!/usr/bin/env bash
set -euo pipefail

# Reads aionrs commit subject lines (one per line) on stdin and emits a
# conventional-commit footer block on stdout. Keeps only feat/fix/perf,
# rewrites the scope to `engine`, drops original sub-scope and breaking "!",
# dedupes, and groups by type (feat -> fix -> perf). Emits a single
# "No user-facing engine changes." line when nothing qualifies.

# Program is passed on fd 3 so that stdin stays connected to the piped
# commit-subject stream (a plain `python3 - <<'PY'` heredoc would consume
# stdin itself, leaving nothing to read).
python3 /dev/fd/3 3<<'PY'
import re, sys

pat = re.compile(r'^(feat|fix|perf)(?:\([^)]*\))?!?:\s*(.+?)\s*$')
seen = set()
groups = {'feat': [], 'fix': [], 'perf': []}

for raw in sys.stdin:
    m = pat.match(raw.rstrip('\n'))
    if not m:
        continue
    typ, desc = m.group(1), m.group(2)
    entry = f'{typ}(engine): {desc}'
    if entry in seen:
        continue
    seen.add(entry)
    groups[typ].append(entry)

out = groups['feat'] + groups['fix'] + groups['perf']
print('\n'.join(out) if out else 'No user-facing engine changes.')
PY
