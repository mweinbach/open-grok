# Build the Open Grok Windows x86_64 release artifact.
#
# Windows counterpart of build-macos-release.sh. Differences by design:
# - No ripgrep bundling: xai-grok-tools' build.rs intentionally skips rg
#   embedding on Windows targets; the runtime resolves `rg` from PATH
#   (users install it via winget/scoop).
# - No code signing: there is no signing identity in this pipeline yet.
# - The repo's bin/protoc dotslash wrapper has no Windows platform entry.
#   An explicit $env:PROTOC or PATH installation wins; otherwise this script
#   downloads and verifies the pinned official Windows archive under target/.
#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$versionFile = Join-Path $repoRoot 'OPEN_GROK_VERSION'
$distDir = Join-Path $repoRoot 'dist'
$artifactName = 'open-grok-windows-x86_64.exe'
$targetTriple = 'x86_64-pc-windows-msvc'
$expectedProtoc = 'libprotoc 29.3'
$protocArchiveUrl = 'https://github.com/protocolbuffers/protobuf/releases/download/v29.3/protoc-29.3-win64.zip'
$protocArchiveSha256 = '57ea59e9f551ad8d71ffaa9b5cfbe0ca1f4e720972a1db7ec2d12ab44bff9383'

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

function Install-PinnedProtoc {
    $toolsDir = Join-Path $repoRoot 'target\release-tools'
    $protocDir = Join-Path $toolsDir 'protoc-29.3-win64'
    $protocExe = Join-Path $protocDir 'bin\protoc.exe'
    if (Test-Path $protocExe) {
        $cachedVersion = (& $protocExe --version).Trim()
        if ($cachedVersion -eq $expectedProtoc) {
            return $protocExe
        }
    }

    New-Item -ItemType Directory -Force $toolsDir | Out-Null
    $archivePath = Join-Path $toolsDir 'protoc-29.3-win64.zip'
    $downloadPath = "$archivePath.download.$PID"
    try {
        Write-Host "Downloading pinned protoc 29.3 for Windows..."
        Invoke-WebRequest -Uri $protocArchiveUrl -OutFile $downloadPath -UseBasicParsing
        $archiveDigest = (Get-FileHash -Algorithm SHA256 $downloadPath).Hash.ToLowerInvariant()
        if ($archiveDigest -ne $protocArchiveSha256) {
            throw "Error: protoc archive SHA-256 mismatch (expected $protocArchiveSha256, got $archiveDigest)"
        }
        Move-Item $downloadPath $archivePath -Force
        if (Test-Path $protocDir) {
            Remove-Item $protocDir -Recurse -Force
        }
        Expand-Archive -LiteralPath $archivePath -DestinationPath $protocDir
    }
    finally {
        if (Test-Path $downloadPath) {
            Remove-Item $downloadPath -Force
        }
    }

    if (-not (Test-Path $protocExe)) {
        throw "Error: verified protoc archive did not contain $protocExe"
    }
    return $protocExe
}

# Resolve protoc: explicit $env:PROTOC wins, then PATH. The version must
# match the dotslash pin in bin/protoc so generated code is reproducible.
$protocPath = $env:PROTOC
if (-not $protocPath) {
    $protocCmd = Get-Command protoc -ErrorAction SilentlyContinue
    if ($protocCmd) { $protocPath = $protocCmd.Source }
}
if (-not $protocPath) {
    $protocPath = Install-PinnedProtoc
}
if (-not (Test-Path $protocPath)) {
    throw "Error: protoc not found at '$protocPath'"
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
$releaseInstaller = Join-Path $distDir 'install.ps1'
$releaseLicense = Join-Path $distDir 'LICENSE'
$releaseNotices = Join-Path $distDir 'THIRD-PARTY-NOTICES'

Set-Location $repoRoot
Write-Host "Building Open Grok $version ($commit)..."
$env:GROK_VERSION = $version
$env:CARGO_INCREMENTAL = '0'
$env:PROTOC = $protocPath
cargo build --locked --profile release-dist --features release-dist `
    --target $targetTriple -p xai-grok-pager-bin --bin open-grok --timings
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
    Copy-Item (Join-Path $repoRoot 'crates\codegen\xai-grok-pager\scripts\install.ps1') $releaseInstaller -Force
    Copy-Item (Join-Path $repoRoot 'LICENSE') $releaseLicense -Force
    Copy-Item (Join-Path $repoRoot 'THIRD-PARTY-NOTICES') $releaseNotices -Force
}
finally {
    if (Test-Path $stagedArtifact) { Remove-Item $stagedArtifact -Force }
}

Write-Host 'Release assets:'
Write-Host "  $artifactPath"
Write-Host "  $checksumPath"
Write-Host "  $releaseInstaller"
Write-Host "  $releaseLicense"
Write-Host "  $releaseNotices"
