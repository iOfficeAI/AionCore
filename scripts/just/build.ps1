param(
    [ValidateSet("release", "debug")]
    [string] $Mode = "release",
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $Flags
)

$ErrorActionPreference = "Stop"

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Command,
        [string[]] $Arguments = @()
    )

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
}

$force = $Flags -contains "--force" -or $Flags -contains "-f"

if ($Mode -eq "release") {
    Invoke-Native "just" @("_cargo", "build", "--release")
    $binary = "target/release/aioncore.exe"
    $sumFile = "target/.build-sum"
    $label = "Build"
} else {
    Invoke-Native "just" @("_cargo", "build")
    $binary = "target/debug/aioncore.exe"
    $sumFile = "target/.build-debug-sum"
    $label = "Debug build"
}

$newSum = (Get-FileHash -Algorithm SHA256 -LiteralPath $binary).Hash.ToLowerInvariant()
$oldSum = ""
if ((Test-Path -LiteralPath $sumFile -PathType Leaf) -and -not $force) {
    $oldSum = (Get-Content -LiteralPath $sumFile -Raw).Trim()
}

if ($newSum -eq $oldSum) {
    Write-Output ""
    Write-Output "$label unchanged (sha256: $($newSum.Substring(0, 16)))"
} else {
    if ($Mode -eq "release") {
        Invoke-Native "powershell.exe" @("-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "scripts/just/install.ps1", "release")
    }
    Set-Content -LiteralPath $sumFile -Value $newSum -NoNewline
    Write-Output ""
    Write-Output "$label complete (sha256: $($newSum.Substring(0, 16)))"
}
