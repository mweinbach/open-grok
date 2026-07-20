# Open Grok release installer for Windows.
#
# Usage:
#   irm https://raw.githubusercontent.com/mweinbach/open-grok/main/crates/codegen/xai-grok-pager/scripts/install.ps1 | iex
#   $env:OPEN_GROK_VERSION = '0.1.220-open-grok.16'; irm <url> | iex
#
# Environment:
#   OPENGROK_HOME               Runtime home (default: %USERPROFILE%\.opengrok)
#   OPEN_GROK_BIN_DIR           Installation directory override
#   OPEN_GROK_RELEASE_BASE_URL  Direct URL containing the release assets

param(
    [Parameter(Position = 0)]
    [string]$Version
)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$ProgressPreference = 'SilentlyContinue'

if (-not $Version -and $env:OPEN_GROK_VERSION) {
    $Version = $env:OPEN_GROK_VERSION
}
$Version = $Version -replace '^v', ''
if ($Version -and $Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$') {
    Write-Error "Invalid Open Grok version: $Version"
    exit 2
}

if ($PSVersionTable.Platform -and $PSVersionTable.Platform -ne 'Win32NT') {
    Write-Error 'This installer is for Windows. Use install.sh on Apple Silicon macOS.'
    exit 1
}

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'x86' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { $null }
}
if (-not $arch) {
    Write-Error "Unsupported architecture: $env:PROCESSOR_ARCHITECTURE"
    exit 1
}

$repository = 'mweinbach/open-grok'
$artifactName = "open-grok-windows-$arch.exe"
if ($env:OPEN_GROK_RELEASE_BASE_URL) {
    $releaseUrl = $env:OPEN_GROK_RELEASE_BASE_URL.TrimEnd('/')
} elseif ($Version) {
    $releaseUrl = "https://github.com/$repository/releases/download/v$Version"
} else {
    $releaseUrl = "https://github.com/$repository/releases/latest/download"
}

if ($env:OPEN_GROK_BIN_DIR) {
    $binDir = $env:OPEN_GROK_BIN_DIR
} else {
    $openGrokHome = if ($env:OPENGROK_HOME) {
        $env:OPENGROK_HOME
    } else {
        Join-Path $env:USERPROFILE '.opengrok'
    }
    $binDir = Join-Path $openGrokHome 'bin'
}

New-Item -ItemType Directory -Path $binDir -Force | Out-Null
$stageDir = Join-Path ([System.IO.Path]::GetTempPath()) ("open-grok-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $stageDir -Force | Out-Null

try {
    $binaryPath = Join-Path $stageDir $artifactName
    $checksumPath = "$binaryPath.sha256"

    Write-Host "Downloading Open Grok ${Version} for Windows $arch..." -ForegroundColor DarkGray
    Invoke-WebRequest -Uri "$releaseUrl/$artifactName" -OutFile $binaryPath -UseBasicParsing
    Invoke-WebRequest -Uri "$releaseUrl/$artifactName.sha256" -OutFile $checksumPath -UseBasicParsing

    $expected = ((Get-Content -Raw $checksumPath).Trim() -split '\s+')[0].ToLowerInvariant()
    if ($expected -notmatch '^[0-9a-f]{64}$') {
        throw 'Release checksum is not a valid SHA-256 digest.'
    }
    $actual = (Get-FileHash -Algorithm SHA256 $binaryPath).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw 'SHA-256 verification failed; Open Grok was not installed.'
    }

    $destination = Join-Path $binDir 'open-grok.exe'
    $oldDestination = "$destination.old"
    if (Test-Path $oldDestination) {
        Remove-Item $oldDestination -Force -ErrorAction SilentlyContinue
    }
    try {
        Copy-Item -Path $binaryPath -Destination $destination -Force
    } catch {
        if (Test-Path $destination) {
            Rename-Item $destination $oldDestination -Force
        }
        try {
            Copy-Item -Path $binaryPath -Destination $destination -Force
        } catch {
            if (Test-Path $oldDestination) {
                Rename-Item $oldDestination $destination -Force
            }
            throw
        }
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $pathEntries = if ($userPath) {
        $userPath -split ';' | Where-Object { $_ }
    } else {
        @()
    }
    if ($pathEntries -notcontains $binDir) {
        [Environment]::SetEnvironmentVariable('Path', ((@($binDir) + $pathEntries) -join ';'), 'User')
    }
    if ($env:Path -notlike "*$binDir*") {
        $env:Path = "$binDir;$env:Path"
    }

    Write-Host "Installed Open Grok at $destination" -ForegroundColor Green
    Write-Host "Run 'open-grok' to get started." -ForegroundColor Cyan
} finally {
    Remove-Item $stageDir -Recurse -Force -ErrorAction SilentlyContinue
}
