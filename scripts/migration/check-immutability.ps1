$ErrorActionPreference = "Stop"

if ($env:AIONCORE_ALLOW_MAIN_MIGRATION_EDIT -eq "1") {
    Write-Output "AIONCORE_ALLOW_MAIN_MIGRATION_EDIT=1; skipping migration immutability check"
    exit 0
}

$repoRoot = (git rev-parse --show-toplevel)
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
Set-Location $repoRoot

$baseRef = $env:AIONCORE_MIGRATION_BASE_REF
if ([string]::IsNullOrWhiteSpace($baseRef)) {
    git rev-parse --verify --quiet origin/main | Out-Null
    if ($LASTEXITCODE -eq 0) {
        $baseRef = "origin/main"
    } else {
        git rev-parse --verify --quiet main | Out-Null
        if ($LASTEXITCODE -eq 0) {
            $baseRef = "main"
        } else {
            Write-Output "No origin/main or main ref found; skipping migration immutability check"
            exit 0
        }
    }
}

git rev-parse --verify --quiet $baseRef | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Error "Migration immutability base ref not found: $baseRef"
    exit 1
}

$baseCommit = git merge-base HEAD $baseRef
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$changed = git diff --name-status --diff-filter=DMR $baseCommit -- "crates/aionui-db/migrations/*.sql"
if (-not [string]::IsNullOrWhiteSpace(($changed -join "`n"))) {
    [Console]::Error.WriteLine("Existing migration files from main must not be modified or deleted.")
    [Console]::Error.WriteLine("")
    [Console]::Error.WriteLine("Fix this by reverting changes to existing migration files and adding a new next-numbered migration instead.")
    [Console]::Error.WriteLine("If this is an intentional high-risk exception, rerun with AIONCORE_ALLOW_MAIN_MIGRATION_EDIT=1.")
    [Console]::Error.WriteLine("")
    [Console]::Error.WriteLine("Changed existing migrations:")
    [Console]::Error.WriteLine(($changed -join "`n"))
    exit 1
}

Write-Output "Migration immutability check passed"
