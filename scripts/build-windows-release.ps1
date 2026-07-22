# Build the Open Grok Windows x86_64 release artifact.
#
# Windows counterpart of build-macos-release.sh. Differences by design:
# - No ripgrep bundling: xai-grok-tools' build.rs intentionally skips rg
#   embedding on Windows targets; the runtime resolves `rg` from PATH
#   (users install it via winget/scoop).
# - No code signing: there is no signing identity in this pipeline yet.
# - protoc cannot come from the repo's bin/protoc dotslash wrapper (no
#   Windows platform entry), so a local protoc matching the pinned version
#   is required via $env:PROTOC or PATH.
#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$versionFile = Join-Path $repoRoot 'OPEN_GROK_VERSION'
$distDir = Join-Path $repoRoot 'dist'
$artifactName = 'open-grok-windows-x86_64.exe'
$targetTriple = 'x86_64-pc-windows-msvc'
$expectedProtoc = 'libprotoc 29.3'

if (-not (Test-Path $versionFile)) {
    throw "Error: missing $versionFile"
}
$version = (Get-Content $versionFile -TotalCount 1).Trim()
if ($version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$') {
    throw "Error: invalid Open Grok version '$version' in $versionFile"
}

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
    throw 'Error: this release builder requires x86_64 Windows.'
}

foreach ($command in @('cargo', 'git')) {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "Error: required command not found: $command"
    }
}

# Resolve protoc: explicit $env:PROTOC wins, then PATH. The version must
# match the dotslash pin in bin/protoc so generated code is reproducible.
$protocPath = $env:PROTOC
if (-not $protocPath) {
    $protocCmd = Get-Command protoc -ErrorAction SilentlyContinue
    if ($protocCmd) { $protocPath = $protocCmd.Source }
}
if (-not $protocPath -or -not (Test-Path $protocPath)) {
    throw ('Error: protoc not found. Install protoc 29.3 (protoc-29.3-win64.zip from ' +
        'https://github.com/protocolbuffers/protobuf/releases/tag/v29.3) and set $env:PROTOC to protoc.exe.')
}
$protocVersion = (& $protocPath --version).Trim()
if ($protocVersion -ne $expectedProtoc) {
    throw "Error: release builds require '$expectedProtoc'; found '$protocVersion' at $protocPath"
}

$gitStatus = git -C $repoRoot status --porcelain --untracked-files=normal
if ($gitStatus) {
    throw 'Error: release builds require a clean git worktree. Commit or remove all tracked and untracked changes, then retry.'
}
$commit = (git -C $repoRoot rev-parse --short HEAD).Trim()

New-Item -ItemType Directory -Force $distDir | Out-Null
$artifactPath = Join-Path $distDir $artifactName
$checksumPath = "$artifactPath.sha256"
$releaseLicense = Join-Path $distDir 'LICENSE'
$releaseNotices = Join-Path $distDir 'THIRD-PARTY-NOTICES'

Write-Host 'Refreshing version/commit build metadata...'
Set-Location $repoRoot
cargo clean --quiet --profile release-dist --target $targetTriple `
    -p xai-grok-pager-bin -p xai-grok-pager -p xai-grok-tools
if ($LASTEXITCODE -ne 0) { throw 'Error: cargo clean failed' }

Write-Host "Building Open Grok $version ($commit)..."
$env:GROK_VERSION = $version
$env:CARGO_INCREMENTAL = '0'
$env:PROTOC = $protocPath
cargo build --locked --profile release-dist --features release-dist `
    --target $targetTriple -p xai-grok-pager-bin --bin open-grok
if ($LASTEXITCODE -ne 0) { throw 'Error: cargo build failed' }

$sourceBinary = Join-Path $repoRoot "target\$targetTriple\release-dist\open-grok.exe"
if (-not (Test-Path $sourceBinary)) {
    throw "Error: Cargo did not produce $sourceBinary"
}

$stagedArtifact = Join-Path $distDir ".open-grok-windows-x86_64.tmp.$PID.exe"
try {
    Copy-Item $sourceBinary $stagedArtifact -Force

    $versionOutput = (& $stagedArtifact --version) -join "`n"
    if ($versionOutput -notlike "*$version*") {
        throw "Error: release version verification failed. Expected '$version' in: $versionOutput"
    }
    if ($versionOutput -notlike "*$commit*") {
        throw "Error: release commit verification failed. Expected '$commit' in: $versionOutput"
    }

    $checksum = (Get-FileHash -Algorithm SHA256 $stagedArtifact).Hash.ToLowerInvariant()
    # Two-space separator matches the macOS artifact's `shasum` format.
    $checksumLine = "$checksum  $artifactName"

    Move-Item $stagedArtifact $artifactPath -Force
    [System.IO.File]::WriteAllText($checksumPath, "$checksumLine`n")
    Copy-Item (Join-Path $repoRoot 'LICENSE') $releaseLicense -Force
    Copy-Item (Join-Path $repoRoot 'THIRD-PARTY-NOTICES') $releaseNotices -Force
}
finally {
    if (Test-Path $stagedArtifact) { Remove-Item $stagedArtifact -Force }
}

Write-Host 'Release assets:'
Write-Host "  $artifactPath"
Write-Host "  $checksumPath"
Write-Host "  $releaseLicense"
Write-Host "  $releaseNotices"
