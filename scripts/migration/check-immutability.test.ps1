$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $PSCommandPath
$repoRoot = (Resolve-Path (Join-Path $scriptDir "../..")).ProviderPath
$script = Join-Path $repoRoot "scripts/migration/check-immutability.ps1"
$tmpdir = Join-Path ([System.IO.Path]::GetTempPath()) ("aioncore-migration-test-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tmpdir | Out-Null

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Command,
        [Parameter(ValueFromRemainingArguments = $true)]
        [string[]] $Arguments
    )

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
}

function Invoke-WithEnv {
    param(
        [hashtable] $EnvVars,
        [scriptblock] $Body
    )

    $oldValues = @{}
    foreach ($key in $EnvVars.Keys) {
        $oldValues[$key] = [Environment]::GetEnvironmentVariable($key, "Process")
        [Environment]::SetEnvironmentVariable($key, [string]$EnvVars[$key], "Process")
    }

    try {
        & $Body
    } finally {
        foreach ($key in $EnvVars.Keys) {
            [Environment]::SetEnvironmentVariable($key, $oldValues[$key], "Process")
        }
    }
}

function Invoke-InRepo {
    param(
        [string] $Cwd,
        [int] $ExpectedStatus,
        [string] $ExpectedText,
        [hashtable] $EnvVars
    )

    Push-Location $Cwd
    try {
        $output = ""
        Invoke-WithEnv $EnvVars {
            $previousErrorActionPreference = $ErrorActionPreference
            $ErrorActionPreference = "Continue"
            try {
                $result = & powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File $script 2>&1
                $script:actualStatus = $LASTEXITCODE
                $script:actualOutput = ($result | Out-String)
            } finally {
                $ErrorActionPreference = $previousErrorActionPreference
            }
        }
        $output = $script:actualOutput
        $status = $script:actualStatus
    } finally {
        Pop-Location
    }

    if ($status -ne $ExpectedStatus) {
        [Console]::Error.WriteLine("expected status $ExpectedStatus, got $status")
        [Console]::Error.WriteLine($output)
        exit 1
    }

    if (-not [string]::IsNullOrWhiteSpace($ExpectedText) -and -not $output.Contains($ExpectedText)) {
        [Console]::Error.WriteLine("expected output to contain: $ExpectedText")
        [Console]::Error.WriteLine($output)
        exit 1
    }
}

function New-CaseRepo {
    param([string] $Name)

    $dir = Join-Path $tmpdir $Name
    New-Item -ItemType Directory -Force -Path (Join-Path $dir "crates/aionui-db/migrations") | Out-Null

    Push-Location $dir
    try {
        Invoke-Native git init -q -b main
        Invoke-Native git config user.email test@example.com
        Invoke-Native git config user.name "Migration Test"
        Set-Content -LiteralPath "crates/aionui-db/migrations/001_initial_schema.sql" -Value "-- 001 initial"
        Set-Content -LiteralPath "crates/aionui-db/migrations/002_data_fix.sql" -Value "-- 002 data fix"
        Set-Content -LiteralPath "crates/aionui-db/migrations/manual_fixture.sql" -Value "-- auxiliary sql"
        Invoke-Native git add crates/aionui-db/migrations
        Invoke-Native git commit -q -m "seed migrations"
        Invoke-Native git tag v1.0.0
        Invoke-Native git checkout -q -b feature
    } finally {
        Pop-Location
    }

    return $dir
}

try {
    $modifiedRepo = New-CaseRepo "modified"
    Add-Content -LiteralPath (Join-Path $modifiedRepo "crates/aionui-db/migrations/001_initial_schema.sql") -Value "-- modified"
    Invoke-InRepo $modifiedRepo 1 "Released migration files must not be modified or deleted" @{}

    $deletedRepo = New-CaseRepo "deleted"
    Remove-Item -LiteralPath (Join-Path $deletedRepo "crates/aionui-db/migrations/002_data_fix.sql")
    Invoke-InRepo $deletedRepo 1 "Released migration files must not be modified or deleted" @{}

    $auxiliaryRepo = New-CaseRepo "auxiliary"
    Add-Content -LiteralPath (Join-Path $auxiliaryRepo "crates/aionui-db/migrations/manual_fixture.sql") -Value "-- modified auxiliary sql"
    Invoke-InRepo $auxiliaryRepo 1 "Released migration files must not be modified or deleted" @{}

    $addedRepo = New-CaseRepo "added"
    Set-Content -LiteralPath (Join-Path $addedRepo "crates/aionui-db/migrations/003_new_change.sql") -Value "-- 003 new migration"
    Invoke-InRepo $addedRepo 0 "Migration immutability check passed" @{}

    $unshippedRepo = New-CaseRepo "unshipped"
    Set-Content -LiteralPath (Join-Path $unshippedRepo "crates/aionui-db/migrations/003_unshipped_change.sql") -Value "-- 003 unshipped migration"
    Push-Location $unshippedRepo
    try {
        Invoke-Native git add crates/aionui-db/migrations/003_unshipped_change.sql
        Invoke-Native git commit -q -m "add unshipped migration"
        Invoke-Native git tag v1.1.0-rc.1
        Invoke-Native git mv crates/aionui-db/migrations/003_unshipped_change.sql crates/aionui-db/migrations/004_unshipped_change.sql
    } finally {
        Pop-Location
    }
    Invoke-InRepo $unshippedRepo 0 "Migration immutability check passed" @{}

    $duplicateRepo = New-CaseRepo "duplicate"
    Set-Content -LiteralPath (Join-Path $duplicateRepo "crates/aionui-db/migrations/002_duplicate_change.sql") -Value "-- duplicate 002 migration"
    Invoke-InRepo $duplicateRepo 1 "Duplicate database migration versions are not allowed" @{}

    $overrideRepo = New-CaseRepo "override"
    Add-Content -LiteralPath (Join-Path $overrideRepo "crates/aionui-db/migrations/001_initial_schema.sql") -Value "-- modified with explicit override"
    Invoke-InRepo $overrideRepo 0 "skipping migration immutability check" @{ AIONCORE_ALLOW_MAIN_MIGRATION_EDIT = "1" }

    Write-Output "Migration immutability script tests passed"
} finally {
    Remove-Item -LiteralPath $tmpdir -Recurse -Force
}
