$ErrorActionPreference = "Stop"
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$script = Join-Path $scriptDir "aionrs-changelog-footer.ps1"

function Assert-Transform($name, $inputText, $expected) {
    $actual = ($inputText -split "`n" | & $script) -join "`n"
    if ($actual -ne $expected) {
        Write-Error "FAIL [$name]`n--- expected ---`n$expected`n--- actual ---`n$actual"
        exit 1
    }
}

Assert-Transform "filter+scope+dedup" @"
feat(agent): persist and report context usage
fix(agent): preserve emergency watermark after microcompact
Merge pull request #239 from iOfficeAI/jiahe/feat/context-usage
feat(agent): persist and report context usage
chore(main): release 0.2.8
chore: sync Cargo.lock for release
"@ @"
feat(engine): persist and report context usage
fix(engine): preserve emergency watermark after microcompact
"@

Assert-Transform "grouping" @"
fix(providers): buffer partial UTF-8 across SSE chunk boundaries
perf(agent): reduce token accounting overhead
feat: add openai responses api support
"@ @"
feat(engine): add openai responses api support
fix(engine): buffer partial UTF-8 across SSE chunk boundaries
perf(engine): reduce token accounting overhead
"@

Assert-Transform "empty" @"
chore: sync Cargo.lock for release
docs: update readme
"@ "No user-facing engine changes."

Assert-Transform "scopeless-and-bang" @"
feat!: drop legacy config
fix(config)!: rename field
"@ @"
feat(engine): drop legacy config
fix(engine): rename field
"@

Write-Output "aionrs-changelog-footer ps1 tests passed"
