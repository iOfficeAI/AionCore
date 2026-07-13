param(
    [ValidateSet("release", "debug")]
    [string] $Mode = "release"
)

$ErrorActionPreference = "Stop"

$binary = if ($Mode -eq "release") {
    "target/release/aioncore.exe"
} else {
    "target/debug/aioncore.exe"
}

if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
    Write-Error "binary not found: $binary"
    exit 1
}

$cargoHome = if ([string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
    Join-Path $HOME ".cargo"
} else {
    $env:CARGO_HOME
}
$installDir = Join-Path $cargoHome "bin"
New-Item -ItemType Directory -Force -Path $installDir | Out-Null

Copy-Item -LiteralPath $binary -Destination $installDir -Force
Write-Output "Installed aioncore.exe to $installDir"
