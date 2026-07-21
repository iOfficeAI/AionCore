param(
    [string] $Tag = ""
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($Tag)) {
    $refs = git ls-remote --tags https://github.com/iOfficeAI/aionrs.git
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }

    $Tag = $refs |
        ForEach-Object {
            if ($_ -match "refs/tags/(v[0-9]+(?:\.[0-9]+)*(?:[-+][0-9A-Za-z.-]+)?)$") {
                $Matches[1]
            }
        } |
        Sort-Object { [version](($_ -replace "^v", "") -replace "[-+].*$", "") } |
        Select-Object -Last 1

    if ([string]::IsNullOrWhiteSpace($Tag)) {
        Write-Error "No aionrs tags found"
        exit 1
    }
    Write-Output "Using latest tag: $Tag"
}

$path = "Cargo.toml"
$text = Get-Content -LiteralPath $path -Raw
$pattern = 'git = "https://github\.com/iOfficeAI/aionrs\.git", tag = "[^"]*"'
$replacement = "git = `"https://github.com/iOfficeAI/aionrs.git`", tag = `"$Tag`""
$matches = [regex]::Matches($text, $pattern)
if ($matches.Count -eq 0) {
    Write-Error "No aionrs git dependency tags found in Cargo.toml"
    exit 1
}
$updated = [regex]::Replace($text, $pattern, $replacement)

[System.IO.File]::WriteAllText(
    (Resolve-Path -LiteralPath $path).ProviderPath,
    $updated,
    [System.Text.UTF8Encoding]::new($false)
)
cargo check --workspace
exit $LASTEXITCODE
