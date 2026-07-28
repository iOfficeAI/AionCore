$ErrorActionPreference = "Stop"

# Reads aionrs commit subject lines from the pipeline and emits a
# conventional-commit footer block. Keeps only feat/fix/perf, rewrites the
# scope to `engine`, drops original sub-scope and breaking "!", dedupes, and
# groups by type (feat -> fix -> perf). Emits "No user-facing engine changes."
# when nothing qualifies.

$lines = @($input)
$pattern = '^(feat|fix|perf)(?:\([^)]*\))?!?:\s*(.+?)\s*$'
$seen = [System.Collections.Generic.HashSet[string]]::new()
$groups = @{
    feat = [System.Collections.Generic.List[string]]::new()
    fix  = [System.Collections.Generic.List[string]]::new()
    perf = [System.Collections.Generic.List[string]]::new()
}

foreach ($line in $lines) {
    $m = [regex]::Match(($line -replace "`r$", ""), $pattern)
    if (-not $m.Success) { continue }
    $typ = $m.Groups[1].Value
    $entry = "$typ(engine): $($m.Groups[2].Value)"
    if ($seen.Add($entry)) { $groups[$typ].Add($entry) }
}

$out = @($groups.feat) + @($groups.fix) + @($groups.perf)
if ($out.Count -gt 0) { $out -join "`n" } else { "No user-facing engine changes." }
