#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="$script_dir/aionrs-changelog-footer.sh"

assert_transform() {
    local name="$1" input="$2" expected="$3"
    local actual
    actual="$(printf '%s' "$input" | bash "$script")"
    if [[ "$actual" != "$expected" ]]; then
        echo "FAIL [$name]" >&2
        echo "--- expected ---" >&2; printf '%s\n' "$expected" >&2
        echo "--- actual ---" >&2; printf '%s\n' "$actual" >&2
        exit 1
    fi
}

# Case 1: real v0.2.7...v0.2.8-style stream with merge/release noise + duplicate
assert_transform "filter+scope+dedup" \
'feat(agent): persist and report context usage
fix(agent): preserve emergency watermark after microcompact
Merge pull request #239 from iOfficeAI/jiahe/feat/context-usage
feat(agent): persist and report context usage
chore(main): release 0.2.8
chore: sync Cargo.lock for release
Merge pull request #240 from iOfficeAI/release-please--branches--main' \
'feat(engine): persist and report context usage
fix(engine): preserve emergency watermark after microcompact'

# Case 2: grouping order feat -> fix -> perf regardless of input order
assert_transform "grouping" \
'fix(providers): buffer partial UTF-8 across SSE chunk boundaries
perf(agent): reduce token accounting overhead
feat: add openai responses api support' \
'feat(engine): add openai responses api support
fix(engine): buffer partial UTF-8 across SSE chunk boundaries
perf(engine): reduce token accounting overhead'

# Case 3: no qualifying commits -> sentinel line
assert_transform "empty" \
'chore: sync Cargo.lock for release
docs: update readme
Merge pull request #1 from x/y' \
'No user-facing engine changes.'

# Case 4: scope-less and breaking-marker forms normalize to engine, drop "!"
assert_transform "scopeless-and-bang" \
'feat!: drop legacy config
fix(config)!: rename field' \
'feat(engine): drop legacy config
fix(engine): rename field'

echo "aionrs-changelog-footer script tests passed"
