[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$OutputRoot,

    [string]$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path,

    [string]$HermesSourceArchivePath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$env:PYTHONDONTWRITEBYTECODE = "1"
$env:PYTHONNOUSERSITE = "1"
$env:PYTHONSAFEPATH = "1"
$env:PYTHONUTF8 = "1"
$env:PYTHONIOENCODING = "utf-8"

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Program,

        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,

        [string]$WorkingDirectory = $RepositoryRoot
    )

    Push-Location -LiteralPath $WorkingDirectory
    try {
        & $Program @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "$Program failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

function Get-VerifiedDownload {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Uri,

        [Parameter(Mandatory = $true)]
        [string]$Destination,

        [Parameter(Mandatory = $true)]
        [string]$ExpectedSha256
    )

    Invoke-WebRequest -Uri $Uri -OutFile $Destination -UseBasicParsing
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $Destination).Hash.ToLowerInvariant()
    if ($actual -ne $ExpectedSha256.ToLowerInvariant()) {
        throw "Checksum mismatch for $([IO.Path]::GetFileName($Destination))"
    }
}

function Get-SingleDirectory {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Root
    )

    $directories = @(Get-ChildItem -LiteralPath $Root -Directory)
    if ($directories.Count -ne 1) {
        throw "Expected one extracted directory under $Root, found $($directories.Count)"
    }
    return $directories[0].FullName
}

function Write-RuntimeChecksums {
    param(
        [Parameter(Mandatory = $true)]
        [string]$RuntimeRoot
    )

    $runtimeFull = [IO.Path]::GetFullPath($RuntimeRoot)
    if (-not $runtimeFull.EndsWith([string][IO.Path]::DirectorySeparatorChar)) {
        $runtimeFull += [IO.Path]::DirectorySeparatorChar
    }

    $lines = Get-ChildItem -LiteralPath $RuntimeRoot -Recurse -File |
        Where-Object { $_.Name -ne "runtime.sha256" } |
        ForEach-Object {
            $fileFull = [IO.Path]::GetFullPath($_.FullName)
            if (-not $fileFull.StartsWith($runtimeFull, [StringComparison]::OrdinalIgnoreCase)) {
                throw "Runtime checksum input escaped the runtime root"
            }
            $relative = $fileFull.Substring($runtimeFull.Length).Replace("\", "/")
            $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant()
            "$hash  $relative"
        } |
        Sort-Object

    [IO.File]::WriteAllLines(
        (Join-Path $RuntimeRoot "runtime.sha256"),
        [string[]]$lines,
        [Text.UTF8Encoding]::new($false)
    )
}

function Remove-NonRuntimePythonArtifacts {
    param(
        [Parameter(Mandatory = $true)]
        [string]$PythonRoot
    )

    $removedFiles = 0
    $bytecodeFiles = @(Get-ChildItem -LiteralPath $PythonRoot -Recurse -File -Filter "*.pyc")
    foreach ($file in $bytecodeFiles) {
        Remove-Item -LiteralPath $file.FullName -Force
        $removedFiles++
    }

    $cacheDirectories = @(
        Get-ChildItem -LiteralPath $PythonRoot -Recurse -Directory -Filter "__pycache__" |
            Sort-Object { $_.FullName.Length } -Descending
    )
    foreach ($directory in $cacheDirectories) {
        if (Test-Path -LiteralPath $directory.FullName) {
            Remove-Item -LiteralPath $directory.FullName -Recurse -Force
        }
    }

    # These launchers target ARM and cannot run in the win32-x64 runtime. Keep
    # unused foreign-architecture PE files out of the platform-specific bundle.
    foreach ($relative in @(
        "Lib\site-packages\pip\_vendor\distlib\t64-arm.exe",
        "Lib\site-packages\pip\_vendor\distlib\w64-arm.exe",
        "Lib\site-packages\setuptools\cli-arm64.exe",
        "Lib\site-packages\setuptools\gui-arm64.exe"
    )) {
        $candidate = Join-Path $PythonRoot $relative
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            Remove-Item -LiteralPath $candidate -Force
            $removedFiles++
        }
    }

    Write-Output "Pruned $removedFiles non-runtime Python artifact(s) from the Hermes win32-x64 bundle"
}

if (-not [Environment]::Is64BitOperatingSystem -or [Environment]::Is64BitProcess -eq $false) {
    throw "The managed Hermes runtime pack must be built by a 64-bit Windows process"
}
if ($env:PROCESSOR_ARCHITECTURE -notin @("AMD64", "x86")) {
    throw "The managed Hermes Beta pack currently supports Windows x64 only"
}

$lockPath = Join-Path $RepositoryRoot "vendor\hermes-agent\runtime-lock.json"
$patchPath = Join-Path $RepositoryRoot "vendor\hermes-agent\aion-managed.patch"
$verifyPath = Join-Path $RepositoryRoot "vendor\hermes-agent\verify_patch.py"
$lock = Get-Content -LiteralPath $lockPath -Raw | ConvertFrom-Json

if ($lock.schemaVersion -ne 1) {
    throw "Unsupported Hermes runtime lock schema $($lock.schemaVersion)"
}
$patchHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $patchPath).Hash.ToLowerInvariant()
if ($patchHash -ne $lock.aionPatchSha256.ToLowerInvariant()) {
    throw "Aion Hermes patch checksum does not match runtime-lock.json"
}

$gitCommand = Get-Command git -ErrorAction Stop
$tarCommand = Get-Command tar.exe -ErrorAction Stop
$outputFull = [IO.Path]::GetFullPath($OutputRoot)
$outputParent = Split-Path -Parent $outputFull
$outputLeaf = Split-Path -Leaf $outputFull
if ([string]::IsNullOrWhiteSpace($outputLeaf) -or [string]::IsNullOrWhiteSpace($outputParent)) {
    throw "OutputRoot must name a concrete runtime directory"
}

New-Item -ItemType Directory -Path $outputParent -Force | Out-Null
$workRoot = Join-Path ([IO.Path]::GetTempPath()) ("aion-hermes-build-" + [Guid]::NewGuid().ToString("N"))
$swapRoot = Join-Path $outputParent ("." + $outputLeaf + ".new-" + [Guid]::NewGuid().ToString("N"))
$backupRoot = Join-Path $outputParent ("." + $outputLeaf + ".old-" + [Guid]::NewGuid().ToString("N"))

try {
    New-Item -ItemType Directory -Path $workRoot -Force | Out-Null
    New-Item -ItemType Directory -Path $swapRoot -Force | Out-Null

    $uvCommand = Get-Command uv -ErrorAction SilentlyContinue
    if ($null -ne $uvCommand) {
        $uvExecutable = $uvCommand.Source
    }
    else {
        $uvArchive = Join-Path $workRoot "uv.zip"
        $uvExtract = Join-Path $workRoot "uv"
        Get-VerifiedDownload $lock.uvArchive $uvArchive $lock.uvSha256
        Expand-Archive -LiteralPath $uvArchive -DestinationPath $uvExtract
        $uvCandidates = @(
            Get-ChildItem -LiteralPath $uvExtract -Recurse -Filter uv.exe |
                Where-Object { $_.PSIsContainer -eq $false }
        )
        if ($uvCandidates.Count -ne 1) {
            throw "Expected one bootstrapped uv.exe, found $($uvCandidates.Count)"
        }
        $uvExecutable = $uvCandidates[0].FullName
        Write-Output "Bootstrapped pinned uv $($lock.uvVersion) for Hermes runtime preparation"
    }
    $uvVersion = (& $uvExecutable --version).Trim()
    if ($uvVersion -notmatch ('^uv ' + [Regex]::Escape($lock.uvVersion) + '(?:\s|$)')) {
        throw "Expected uv $($lock.uvVersion), got $uvVersion"
    }

    # Fetch and patch the exact first-party adapter source.
    $sourceArchive = Join-Path $workRoot "hermes-source.tar.gz"
    $sourceExtract = Join-Path $workRoot "source"
    if ([string]::IsNullOrWhiteSpace($HermesSourceArchivePath)) {
        Get-VerifiedDownload $lock.hermesSourceArchive $sourceArchive $lock.hermesSourceSha256
    }
    else {
        Copy-Item -LiteralPath $HermesSourceArchivePath -Destination $sourceArchive
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $sourceArchive).Hash.ToLowerInvariant()
        if ($actual -ne $lock.hermesSourceSha256.ToLowerInvariant()) {
            throw "Checksum mismatch for cached Hermes source archive"
        }
    }
    New-Item -ItemType Directory -Path $sourceExtract -Force | Out-Null
    Invoke-Checked $tarCommand.Source @("-xf", $sourceArchive, "-C", $sourceExtract)
    $sourceRoot = Get-SingleDirectory $sourceExtract

    $headVersion = Select-String -LiteralPath (Join-Path $sourceRoot "pyproject.toml") `
        -Pattern ('^version = "' + [Regex]::Escape($lock.hermesVersion) + '"$')
    if (-not $headVersion) {
        throw "Pinned Hermes source does not declare version $($lock.hermesVersion)"
    }

    Invoke-Checked $gitCommand.Source @("apply", "--check", $patchPath) $sourceRoot
    Invoke-Checked $gitCommand.Source @("apply", $patchPath) $sourceRoot

    # Install a relocatable CPython distribution directly, without a venv.
    $pythonStore = Join-Path $workRoot "python-store"
    Invoke-Checked $uvExecutable @(
        "python", "install", $lock.pythonVersion,
        "--install-dir", $pythonStore,
        "--no-bin",
        "--reinstall",
        "--no-config"
    )
    $pythonCandidates = @(
        Get-ChildItem -LiteralPath $pythonStore -Directory |
            Where-Object { $_.Name.StartsWith("cpython-$($lock.pythonVersion)-") } |
            ForEach-Object { Join-Path $_.FullName "python.exe" } |
            Where-Object { Test-Path -LiteralPath $_ -PathType Leaf }
    )
    if ($pythonCandidates.Count -ne 1) {
        throw "Expected one portable python.exe, found $($pythonCandidates.Count)"
    }
    $pythonHome = Split-Path -Parent $pythonCandidates[0]
    Move-Item -LiteralPath $pythonHome -Destination (Join-Path $swapRoot "python")
    $pythonExe = Join-Path $swapRoot "python\python.exe"

    $pythonVersion = (& $pythonExe --version).Trim()
    if ($pythonVersion -ne "Python $($lock.pythonVersion)") {
        throw "Expected Python $($lock.pythonVersion), got $pythonVersion"
    }

    # Install the committed, hash-locked dependency export. It was generated
    # from the upstream uv.lock at the pinned release commit.
    $repositoryFull = [IO.Path]::GetFullPath($RepositoryRoot).TrimEnd("\") + "\"
    $requirements = [IO.Path]::GetFullPath(
        (Join-Path $RepositoryRoot ([string]$lock.requirementsFile))
    )
    if (-not $requirements.StartsWith($repositoryFull, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Hermes requirements file escaped the repository root"
    }
    $requirementsHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $requirements).Hash.ToLowerInvariant()
    if ($requirementsHash -ne $lock.requirementsSha256.ToLowerInvariant()) {
        throw "Hermes requirements checksum does not match runtime-lock.json"
    }
    Invoke-Checked $uvExecutable @(
        "pip", "install",
        "--python", $pythonExe,
        "--break-system-packages",
        "--require-hashes",
        "--link-mode", "copy",
        "--no-config",
        "--no-python-downloads",
        "--requirement", $requirements
    )

    # Install the official wheel by its release hash, then overlay only the
    # five audited files changed by the pinned Aion patch. This avoids an
    # unpinned PEP 517 build environment while keeping the patch reviewable.
    $wheel = Join-Path $workRoot "hermes_agent-0.19.0-py3-none-any.whl"
    Get-VerifiedDownload $lock.hermesWheel $wheel $lock.hermesWheelSha256
    Invoke-Checked $uvExecutable @(
        "pip", "install",
        "--python", $pythonExe,
        "--break-system-packages",
        "--no-deps",
        "--link-mode", "copy",
        "--no-config",
        "--no-python-downloads",
        $wheel
    )

    $sitePackages = Join-Path (Split-Path -Parent $pythonExe) "Lib\site-packages"
    if (-not (Test-Path -LiteralPath $sitePackages -PathType Container)) {
        throw "Failed to locate portable Python site-packages"
    }
    foreach ($relative in @("acp_adapter\entry.py", "acp_adapter\events.py", "acp_adapter\session.py", "agent\coding_context.py", "toolsets.py")) {
        $installedFile = Join-Path $sitePackages $relative
        if (-not (Test-Path -LiteralPath $installedFile -PathType Leaf)) {
            throw "Official Hermes wheel is missing $relative under $sitePackages"
        }
        Copy-Item -LiteralPath (Join-Path $sourceRoot $relative) -Destination $installedFile -Force
    }

    Remove-NonRuntimePythonArtifacts (Split-Path -Parent $pythonExe)
    Invoke-Checked $pythonExe @($verifyPath, $sitePackages)
    Invoke-Checked $pythonExe @("-m", "acp_adapter", "--version")
    Invoke-Checked $pythonExe @("-m", "acp_adapter", "--check")

    # PortableGit is used instead of MinGit because the official adapter
    # requires a real bash plus POSIX utilities.
    $toolsRoot = Join-Path $swapRoot "tools"
    $gitRoot = Join-Path $toolsRoot "git"
    New-Item -ItemType Directory -Path $toolsRoot -Force | Out-Null
    $gitArchive = Join-Path $workRoot "PortableGit.7z.exe"
    Get-VerifiedDownload $lock.portableGitArchive $gitArchive $lock.portableGitSha256
    $extract = Start-Process -FilePath $gitArchive `
        -ArgumentList @("-y", "-o`"$gitRoot`"") `
        -WindowStyle Hidden `
        -Wait `
        -PassThru
    if ($extract.ExitCode -ne 0) {
        throw "PortableGit extraction failed with exit code $($extract.ExitCode)"
    }
    foreach ($required in @("bin\bash.exe", "cmd\git.exe")) {
        if (-not (Test-Path -LiteralPath (Join-Path $gitRoot $required) -PathType Leaf)) {
            throw "PortableGit is missing $required"
        }
    }

    $rgArchive = Join-Path $workRoot "ripgrep.zip"
    $rgExtract = Join-Path $workRoot "ripgrep"
    Get-VerifiedDownload $lock.ripgrepArchive $rgArchive $lock.ripgrepSha256
    Expand-Archive -LiteralPath $rgArchive -DestinationPath $rgExtract
    $rgSource = Get-SingleDirectory $rgExtract
    Copy-Item -LiteralPath $rgSource -Destination (Join-Path $toolsRoot "rg") -Recurse
    if (-not (Test-Path -LiteralPath (Join-Path $toolsRoot "rg\rg.exe") -PathType Leaf)) {
        throw "ripgrep archive is missing rg.exe"
    }

    $licensesRoot = Join-Path $swapRoot "licenses"
    New-Item -ItemType Directory -Path $licensesRoot -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $sourceRoot "LICENSE") `
        -Destination (Join-Path $licensesRoot "hermes-agent-MIT.txt")
    Copy-Item -LiteralPath (Join-Path (Split-Path -Parent $pythonExe) "LICENSE.txt") `
        -Destination (Join-Path $licensesRoot "cpython-PSF.txt")
    Copy-Item -LiteralPath (Join-Path $gitRoot "LICENSE.txt") `
        -Destination (Join-Path $licensesRoot "portable-git-GPL.txt")
    Copy-Item -LiteralPath (Join-Path $rgSource "LICENSE-MIT") `
        -Destination (Join-Path $licensesRoot "ripgrep-MIT.txt")
    Copy-Item -LiteralPath $lockPath -Destination (Join-Path $licensesRoot "runtime-lock.json")
    Copy-Item -LiteralPath $patchPath -Destination (Join-Path $licensesRoot "aion-managed.patch")
    Copy-Item -LiteralPath $requirements -Destination (Join-Path $licensesRoot "requirements-win32-x64.txt")

    Write-RuntimeChecksums $swapRoot

    # Swap only after every download, install, and validation succeeds.
    if (Test-Path -LiteralPath $outputFull) {
        Move-Item -LiteralPath $outputFull -Destination $backupRoot
    }
    try {
        Move-Item -LiteralPath $swapRoot -Destination $outputFull
    }
    catch {
        if (Test-Path -LiteralPath $backupRoot) {
            Move-Item -LiteralPath $backupRoot -Destination $outputFull
        }
        throw
    }
    if (Test-Path -LiteralPath $backupRoot) {
        Remove-Item -LiteralPath $backupRoot -Recurse -Force
    }

    Write-Output "Prepared Hermes $($lock.hermesVersion) runtime at $outputFull"
}
finally {
    if (Test-Path -LiteralPath $workRoot) {
        Remove-Item -LiteralPath $workRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $swapRoot) {
        Remove-Item -LiteralPath $swapRoot -Recurse -Force
    }
}
