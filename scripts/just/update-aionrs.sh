#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
footer_script="$script_dir/aionrs-changelog-footer.sh"

aionrs_repo="https://github.com/iOfficeAI/aionrs.git"
aionrs_slug="iOfficeAI/aionrs"
aioncore_slug="iOfficeAI/AionCore"

fail() { echo "error: $*" >&2; exit 1; }

# --- preflight ---
command -v gh >/dev/null 2>&1 || fail "gh CLI not found"
gh auth status >/dev/null 2>&1 || fail "gh not authenticated; run 'gh auth login'"
cd "$repo_root"
if ! git diff --quiet || ! git diff --cached --quiet; then
    fail "working tree not clean; commit or stash changes first"
fi

# --- resolve target tag ---
tag="${1:-}"
if [[ -z "$tag" ]]; then
    tag=$(
        git ls-remote --tags "$aionrs_repo" |
            python3 -c 'import re, sys; tags=[]; [tags.append(m.group(1)) for line in sys.stdin for m in [re.search(r"refs/tags/(v[0-9]+(?:\.[0-9]+)*(?:[-+][0-9A-Za-z.-]+)?)$", line)] if m]; print(sorted(tags, key=lambda t: [int(p) if p.isdigit() else p for p in re.split(r"[.-]", t.lstrip("v"))])[-1])'
    )
    echo "Using latest tag: $tag"
fi

# --- read current (OLD) tag from Cargo.toml, assert consistency ---
old_tag=$(
    python3 /dev/fd/3 3<<'PY'
import re, sys
from pathlib import Path
text = Path("Cargo.toml").read_text()
tags = re.findall(r'git = "https://github\.com/iOfficeAI/aionrs\.git", tag = "([^"]*)"', text)
if not tags:
    sys.exit("no aionrs git dependency tags found in Cargo.toml")
if len(set(tags)) != 1:
    sys.exit("aionrs tags in Cargo.toml are inconsistent: %s" % sorted(set(tags)))
print(tags[0])
PY
) || fail "failed to read current aionrs tag"

if [[ "$old_tag" == "$tag" ]]; then
    echo "already on $tag; nothing to do"
    exit 0
fi
echo "Updating aionrs $old_tag -> $tag"

# --- rewrite Cargo.toml tags ---
python3 /dev/fd/3 "$tag" 3<<'PY'
from pathlib import Path
import re, sys
tag = sys.argv[1]
path = Path("Cargo.toml")
text = path.read_text()
if not re.search(r'git = "https://github\.com/iOfficeAI/aionrs\.git", tag = "[^"]*"', text):
    raise SystemExit("No aionrs git dependency tags found in Cargo.toml")
path.write_text(re.sub(
    r'git = "https://github\.com/iOfficeAI/aionrs\.git", tag = "[^"]*"',
    f'git = "https://github.com/iOfficeAI/aionrs.git", tag = "{tag}"',
    text,
))
PY

# --- refresh lockfile / verify build wiring ---
cargo check --workspace

# --- build changelog footer from aionrs compare range ---
footer="$(
    gh api "repos/$aionrs_slug/compare/$old_tag...$tag" \
        --jq '.commits[].commit.message | split("\n")[0]' \
        | bash "$footer_script"
)"

pr_body="$(cat <<EOF
Bumps embedded engine aionrs $old_tag → $tag.
https://github.com/$aionrs_slug/compare/$old_tag...$tag

$footer
EOF
)"

# --- branch + commit ---
branch="chore/update-aionrs-$tag"
git checkout -b "$branch"
git add Cargo.toml Cargo.lock
git commit -m "chore(deps): update aionrs to $tag"

# --- push through the full pre-push gate ---
if ! just push -u origin "$branch"; then
    cat >&2 <<EOF

pre-push gate failed. The aionrs bump likely needs adaptation code.
Branch '$branch' is committed locally but NOT pushed, and no PR was created.
Fix the build/tests, then re-run 'just push -u origin $branch' and create the PR
manually with the body printed above.
EOF
    exit 1
fi

# --- create PR ---
# Pin --repo: gh cannot resolve a default repository when multiple GitHub
# remotes exist (e.g. origin + a contributor fork) unless one was configured
# via 'gh repo set-default'.
if ! gh pr create \
    --repo "$aioncore_slug" \
    --title "chore(deps): update aionrs to $tag" \
    --body "$pr_body" \
    --base main \
    --head "$branch"; then
    cat >&2 <<EOF

gh pr create failed. Branch '$branch' is already pushed.
Create the PR manually with this body:

$pr_body
EOF
    exit 1
fi

echo "PR created for aionrs $old_tag -> $tag"
