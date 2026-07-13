$ErrorActionPreference = "Stop"

$changed = git diff --name-only
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

if (-not [string]::IsNullOrWhiteSpace(($changed -join "`n"))) {
    git add -A
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }

    git commit -m "chore: apply auto-fixes (fmt + clippy)"
    exit $LASTEXITCODE
}
