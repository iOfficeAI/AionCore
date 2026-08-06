param([string] $Tag = "")
$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = (Resolve-Path (Join-Path $scriptDir "../..")).ProviderPath
$footerScript = Join-Path $scriptDir "aionrs-changelog-footer.ps1"
$aionrsRepo = "https://github.com/iOfficeAI/aionrs.git"
$aionrsSlug = "iOfficeAI/aionrs"
$aioncoreSlug = "iOfficeAI/AionCore"

function Fail($msg) { Write-Error $msg; exit 1 }

# preflight
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { Fail "gh CLI not found" }
gh auth status *> $null
if ($LASTEXITCODE -ne 0) { Fail "gh not authenticated; run 'gh auth login'" }
Set-Location $repoRoot
git diff --quiet; $d1 = $LASTEXITCODE
git diff --cached --quiet; $d2 = $LASTEXITCODE
if ($d1 -ne 0 -or $d2 -ne 0) { Fail "working tree not clean; commit or stash changes first" }

# resolve target tag
if ([string]::IsNullOrWhiteSpace($Tag)) {
    $refs = git ls-remote --tags $aionrsRepo
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $Tag = $refs |
        ForEach-Object { if ($_ -match "refs/tags/(v[0-9]+(?:\.[0-9]+)*(?:[-+][0-9A-Za-z.-]+)?)$") { $Matches[1] } } |
        Sort-Object { [version](($_ -replace "^v", "") -replace "[-+].*$", "") } |
        Select-Object -Last 1
    if ([string]::IsNullOrWhiteSpace($Tag)) { Fail "No aionrs tags found" }
    Write-Output "Using latest tag: $Tag"
}

# read OLD tag from Cargo.toml, assert consistency
$cargo = Get-Content -LiteralPath "Cargo.toml" -Raw
$readPattern = 'git = "https://github\.com/iOfficeAI/aionrs\.git", tag = "([^"]*)"'
$found = [regex]::Matches($cargo, $readPattern) | ForEach-Object { $_.Groups[1].Value }
if ($found.Count -eq 0) { Fail "No aionrs git dependency tags found in Cargo.toml" }
$uniq = @($found | Select-Object -Unique)
if ($uniq.Count -ne 1) { Fail "aionrs tags in Cargo.toml are inconsistent: $($uniq -join ', ')" }
$oldTag = $uniq[0]

if ($oldTag -eq $Tag) { Write-Output "already on $Tag; nothing to do"; exit 0 }
Write-Output "Updating aionrs $oldTag -> $Tag"

# rewrite Cargo.toml
$replacePattern = 'git = "https://github\.com/iOfficeAI/aionrs\.git", tag = "[^"]*"'
$replacement = "git = `"https://github.com/iOfficeAI/aionrs.git`", tag = `"$Tag`""
$updated = [regex]::Replace($cargo, $replacePattern, $replacement)
[System.IO.File]::WriteAllText((Resolve-Path -LiteralPath "Cargo.toml").ProviderPath, $updated, [System.Text.UTF8Encoding]::new($false))

# refresh lockfile
cargo check --workspace
if ($LASTEXITCODE -ne 0) { Fail "cargo check failed" }

# build changelog footer from aionrs compare range
$subjects = gh api "repos/$aionrsSlug/compare/$oldTag...$Tag" --jq '.commits[].commit.message | split("\n")[0]'
if ($LASTEXITCODE -ne 0) { Fail "failed to fetch aionrs compare range" }
$footer = ($subjects | & $footerScript) -join "`n"

$prBody = @"
Bumps embedded engine aionrs $oldTag → $Tag.
https://github.com/$aionrsSlug/compare/$oldTag...$Tag

$footer
"@

# branch + commit
$branch = "chore/update-aionrs-$Tag"
git checkout -b $branch
git add Cargo.toml Cargo.lock
git commit -m "chore(deps): update aionrs to $Tag"

# push through the full pre-push gate
just push -u origin $branch
if ($LASTEXITCODE -ne 0) {
    Write-Error "pre-push gate failed. The aionrs bump likely needs adaptation code. Branch '$branch' is committed locally but NOT pushed, and no PR was created."
    exit 1
}

# create PR
# Pin --repo: gh cannot resolve a default repository when multiple GitHub
# remotes exist (e.g. origin + a contributor fork) unless one was configured
# via 'gh repo set-default'.
gh pr create --repo $aioncoreSlug --title "chore(deps): update aionrs to $Tag" --body $prBody --base main --head $branch
if ($LASTEXITCODE -ne 0) {
    Fail "gh pr create failed. Branch '$branch' is already pushed. Create the PR manually with this body:`n`n$prBody"
}
Write-Output "PR created for aionrs $oldTag -> $Tag"
