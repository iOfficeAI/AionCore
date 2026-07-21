$ErrorActionPreference = "Stop"

$configFile = if ([string]::IsNullOrWhiteSpace($env:AIONUI_CONFIG_DEV_FILE)) {
    Join-Path $HOME ".aionui-config-dev/aionui-config.txt"
} else {
    $env:AIONUI_CONFIG_DEV_FILE
}

if (-not (Test-Path -LiteralPath $configFile -PathType Leaf)) {
    Write-Error "config file not found: $configFile"
    exit 1
}

$encoded = (Get-Content -LiteralPath $configFile -Raw).Trim()
$bytes = [Convert]::FromBase64String($encoded)
$decoded = [Text.Encoding]::UTF8.GetString($bytes)
$plain = [Uri]::UnescapeDataString($decoded)

if (Get-Command Set-Clipboard -ErrorAction SilentlyContinue) {
    Set-Clipboard -Value $plain
    Write-Output "Config copied to clipboard"
} else {
    Write-Output $plain
}
