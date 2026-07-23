#
# Open Grok installer for PowerShell
# https://github.com/mweinbach/open-grok
#
# Mirrors install.sh for Windows: downloads the published GitHub Release
# artifact, verifies SHA-256, smoke-tests --version, and installs only
# open-grok.exe under OPENGROK_HOME (never ~/.grok, never grok/agent aliases).
#
# Usage:
#   irm https://github.com/mweinbach/open-grok/releases/latest/download/install.ps1 | iex
#   & ([scriptblock]::Create((irm https://github.com/mweinbach/open-grok/releases/latest/download/install.ps1))) -Version 0.1.220-open-grok.24
#   $env:OPEN_GROK_VERSION = '0.1.220-open-grok.24'; irm ... | iex
#
# Environment:
#   OPENGROK_HOME               Runtime home (default: %USERPROFILE%\.opengrok)
#   OPEN_GROK_BIN_DIR           Optional PATH-facing install directory
#   OPEN_GROK_RELEASE_BASE_URL  Direct URL containing the release assets
#   OPEN_GROK_VERSION           Version when piping irm | iex (alternative to -Version)
#

param(
    [Parameter(Position = 0)]
    [string]$Version
)

$ErrorActionPreference = 'Stop'
# Do not enable Set-StrictMode: Windows PowerShell 5.1 lacks keys such as
# $PSVersionTable.Platform, and irm|iex installers must tolerate that.

# PS 5.1 defaults to TLS 1.0; GitHub requires TLS 1.2+.
[Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

# PS 5.1's Invoke-WebRequest progress bar is extremely slow; disable it.
$ProgressPreference = 'SilentlyContinue'

$Repository = 'mweinbach/open-grok'
$ArtifactName = 'open-grok-windows-x86_64.exe'
$PlatformLabel = 'windows-x86_64'
$VersionPattern = '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$'

function Show-Usage {
    [Console]::Error.WriteLine(@'
Usage: install.ps1 [-Version VERSION]

Install the latest Open Grok release, or VERSION when supplied. VERSION may
optionally start with "v".

Environment:
  OPENGROK_HOME               Runtime home (default: %USERPROFILE%\.opengrok)
  OPEN_GROK_BIN_DIR           Optional PATH-facing install directory
  OPEN_GROK_RELEASE_BASE_URL  Direct URL containing the release assets
  OPEN_GROK_VERSION           Version when piping irm | iex
'@)
}

# Accept version from environment (useful with irm | iex).
if (-not $Version -and $env:OPEN_GROK_VERSION) {
    $Version = $env:OPEN_GROK_VERSION
}

# This script is Windows-only. PS 5.1 has no Platform property and only runs
# on Windows; without StrictMode a missing key is simply $null.
if ($PSVersionTable['Platform'] -and $PSVersionTable['Platform'] -ne 'Win32NT') {
    Write-Error 'This installer is for Windows. On macOS, use: curl -fsSL https://github.com/mweinbach/open-grok/releases/latest/download/install.sh | bash'
    exit 1
}

# Strip optional leading "v" (matches install.sh).
$requestedVersion = $Version
if ($requestedVersion) {
    $version = $requestedVersion -replace '^v', ''
    if ($version -notmatch $VersionPattern) {
        Write-Error "Invalid version '$requestedVersion' (expected X.Y.Z or X.Y.Z-suffix)."
        Show-Usage
        exit 2
    }
} else {
    $version = ''
}

# --- Detect architecture (only x86_64 is published today) ---

$processorArch = $env:PROCESSOR_ARCHITECTURE
if ($env:PROCESSOR_ARCHITEW6432) {
    # 32-bit PowerShell on 64-bit Windows reports x86; the OS arch is here.
    $processorArch = $env:PROCESSOR_ARCHITEW6432
}

if ($processorArch -ne 'AMD64') {
    Write-Error ("prebuilt Open Grok releases currently require Windows x86_64.`n" +
        "Detected: $processorArch. Build from source on unsupported platforms.")
    exit 1
}

# --- Resolve home / bin dirs ---

if (-not $env:OPENGROK_HOME -and -not $env:USERPROFILE) {
    Write-Error 'USERPROFILE or OPENGROK_HOME must be set.'
    exit 1
}

$openGrokHome = if ($env:OPENGROK_HOME) { $env:OPENGROK_HOME } else {
    Join-Path $env:USERPROFILE '.opengrok'
}
$managedBinDir = Join-Path $openGrokHome 'bin'
$binDir = if ($env:OPEN_GROK_BIN_DIR) { $env:OPEN_GROK_BIN_DIR } else { $managedBinDir }
$downloadDir = Join-Path $openGrokHome 'downloads'

foreach ($checkedDir in @($managedBinDir, $binDir, $downloadDir, $openGrokHome)) {
    if (-not [System.IO.Path]::IsPathRooted($checkedDir)) {
        Write-Error "the Open Grok path must be absolute: $checkedDir"
        exit 1
    }
    if ($checkedDir -match "[\r\n]") {
        Write-Error 'the Open Grok path contains unsupported characters.'
        exit 1
    }
}

# --- Resolve release URL ---

if ($env:OPEN_GROK_RELEASE_BASE_URL) {
    $releaseUrl = $env:OPEN_GROK_RELEASE_BASE_URL.TrimEnd('/')
} elseif ($version) {
    $releaseUrl = "https://github.com/$Repository/releases/download/v$version"
} else {
    $releaseUrl = "https://github.com/$Repository/releases/latest/download"
}

# --- Helpers ---

function Download-File([string]$Url, [string]$OutFile) {
    # Prefer curl.exe (ships with Windows 10+): native TLS, redirects, and far
    # faster than System.Net.HttpWebRequest under Windows PowerShell 5.1.
    $curl = Get-Command -Name curl.exe -ErrorAction SilentlyContinue
    if ($curl) {
        $curlArgs = @(
            '-fL',
            '--retry', '3',
            '--retry-delay', '1',
            '--connect-timeout', '30',
            '--max-time', '600',
            '-A', 'open-grok-install',
            '--progress-bar',
            '-o', $OutFile,
            '--', $Url
        )
        & $curl.Source @curlArgs
        if ($LASTEXITCODE -ne 0) {
            if (Test-Path -LiteralPath $OutFile) {
                Remove-Item -LiteralPath $OutFile -Force -ErrorAction SilentlyContinue
            }
            throw "curl download failed (exit $LASTEXITCODE): $Url"
        }
        if (-not (Test-Path -LiteralPath $OutFile)) {
            throw "curl reported success but did not create: $OutFile"
        }
        return
    }

    # Fallback: HttpWebRequest with a large buffer (Invoke-WebRequest is slower on PS 5.1).
    $request = [System.Net.HttpWebRequest]::Create($Url)
    $request.Method = 'GET'
    $request.Timeout = 600000
    $request.ReadWriteTimeout = 600000
    $request.UserAgent = 'open-grok-install'
    $request.AllowAutoRedirect = $true
    $request.AutomaticDecompression =
        [System.Net.DecompressionMethods]::GZip -bor [System.Net.DecompressionMethods]::Deflate
    $request.KeepAlive = $true
    # Avoid the extra round-trip some hosts force with Expect: 100-continue.
    $request.ServicePoint.Expect100Continue = $false
    $request.ServicePoint.ConnectionLimit = [Math]::Max($request.ServicePoint.ConnectionLimit, 16)

    $response = $request.GetResponse()
    $totalBytes = $response.ContentLength
    $stream = $response.GetResponseStream()
    $fileStream = [System.IO.File]::Create($OutFile)
    # 1 MiB buffer: 64 KiB under-utilizes the TCP window on large release binaries.
    $buffer = New-Object byte[] 1048576
    $totalRead = 0
    $lastPercent = -1
    $lastMb = -1

    try {
        while (($read = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            $fileStream.Write($buffer, 0, $read)
            $totalRead += $read
            $mb = [math]::Round($totalRead / 1MB, 1)
            if ($totalBytes -gt 0) {
                $percent = [math]::Min(100, [math]::Floor(($totalRead / [double]$totalBytes) * 100))
                if ($percent -ne $lastPercent) {
                    $totalMb = [math]::Round($totalBytes / 1MB, 1)
                    Write-Host ("`r  Downloading... {0} MB / {1} MB ({2}%)" -f $mb, $totalMb, $percent) -NoNewline
                    $lastPercent = $percent
                }
            } elseif ($mb -ne $lastMb) {
                Write-Host ("`r  Downloading... {0} MB" -f $mb) -NoNewline
                $lastMb = $mb
            }
        }
        Write-Host ''
    } finally {
        $fileStream.Close()
        $stream.Close()
        $response.Close()
    }
}

function Install-ExecutableLockedSafe {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    $parent = Split-Path -Parent $Destination
    if (-not (Test-Path -LiteralPath $parent)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }

    $old = "$Destination.old"
    if (Test-Path -LiteralPath $old) {
        Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
    }

    try {
        Copy-Item -LiteralPath $Source -Destination $Destination -Force
        return
    } catch {
        # Running open-grok.exe locks the file for write; rename-aside then copy.
    }

    try {
        if (Test-Path -LiteralPath $Destination) {
            Rename-Item -LiteralPath $Destination -NewName (Split-Path -Leaf $old) -Force
        }
        Copy-Item -LiteralPath $Source -Destination $Destination -Force
    } catch {
        if ((Test-Path -LiteralPath $old) -and -not (Test-Path -LiteralPath $Destination)) {
            Rename-Item -LiteralPath $old -NewName (Split-Path -Leaf $Destination) -Force -ErrorAction SilentlyContinue
        }
        throw "Failed to install $(Split-Path -Leaf $Destination): $_"
    }
}

function Get-ReportedVersion([string]$BinaryPath) {
    $output = & $BinaryPath --version 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "downloaded Open Grok binary failed its --version smoke test (exit $LASTEXITCODE): $output"
    }
    $text = ($output | Out-String).Trim()
    foreach ($token in ($text -split '\s+')) {
        $clean = $token.Trim(',', ';', '(', ')', '[', ']')
        if ($clean -match $VersionPattern) {
            return $clean
        }
    }
    throw "downloaded Open Grok binary did not report a valid version. Actual output: $text"
}

# --- Stage directory ---

New-Item -ItemType Directory -Path $managedBinDir -Force | Out-Null
New-Item -ItemType Directory -Path $downloadDir -Force | Out-Null
New-Item -ItemType Directory -Path $binDir -Force | Out-Null

$stageDir = Join-Path $downloadDir ('.open-grok-install.{0}' -f $PID)
if (Test-Path -LiteralPath $stageDir) {
    Remove-Item -LiteralPath $stageDir -Recurse -Force
}
New-Item -ItemType Directory -Path $stageDir -Force | Out-Null

try {
    $binaryTmp = Join-Path $stageDir $ArtifactName
    $checksumTmp = Join-Path $stageDir "$ArtifactName.sha256"

    $label = if ($version) { $version } else { 'latest' }
    Write-Host "Downloading Open Grok $label for Windows x86_64..." -ForegroundColor Cyan

    try {
        Download-File "$releaseUrl/$ArtifactName" $binaryTmp
    } catch {
        throw "Binary download failed from $releaseUrl/$ArtifactName : $_"
    }
    try {
        Download-File "$releaseUrl/$ArtifactName.sha256" $checksumTmp
    } catch {
        throw "Checksum download failed from $releaseUrl/$ArtifactName.sha256 : $_"
    }

    # --- Verify SHA-256 ---

    $checksumLine = (Get-Content -LiteralPath $checksumTmp -TotalCount 1).Trim()
    if (-not $checksumLine) {
        throw 'release checksum file is empty.'
    }
    $expectedSha = ($checksumLine -split '\s+')[0].ToLowerInvariant()
    if ($expectedSha -notmatch '^[0-9a-f]{64}$') {
        throw 'release checksum is not a valid SHA-256 digest.'
    }

    $actualSha = (Get-FileHash -Algorithm SHA256 -LiteralPath $binaryTmp).Hash.ToLowerInvariant()
    if ($actualSha -ne $expectedSha) {
        throw ("SHA-256 verification failed; Open Grok was not installed.`n" +
            "Expected: $expectedSha`n" +
            "Actual:   $actualSha")
    }

    # --- Smoke-test version ---

    $reportedVersion = Get-ReportedVersion $binaryTmp
    if ($version -and $reportedVersion -ne $version) {
        throw ("downloaded Open Grok binary reported an unexpected version.`n" +
            "Expected: $version`n" +
            "Actual:   $reportedVersion")
    }

    $installedVersion = $reportedVersion

    # --- Persist versioned binary under downloads/ ---

    $versionedName = "open-grok-$installedVersion-$PlatformLabel.exe"
    $versionedBinary = Join-Path $downloadDir $versionedName
    if (Test-Path -LiteralPath $versionedBinary) {
        $versionedName = "open-grok-$installedVersion-$PlatformLabel-reinstall-$PID.exe"
        $versionedBinary = Join-Path $downloadDir $versionedName
    }
    Move-Item -LiteralPath $binaryTmp -Destination $versionedBinary -Force

    # --- Activate managed open-grok.exe (locked-file safe) ---

    $managedCommand = Join-Path $managedBinDir 'open-grok.exe'
    Install-ExecutableLockedSafe -Source $versionedBinary -Destination $managedCommand

    if ([System.IO.Path]::GetFullPath($binDir) -ne [System.IO.Path]::GetFullPath($managedBinDir)) {
        $exposedCommand = Join-Path $binDir 'open-grok.exe'
        Install-ExecutableLockedSafe -Source $managedCommand -Destination $exposedCommand
        Write-Host "Linked $exposedCommand to the managed command." -ForegroundColor DarkGray
    }

    Write-Host "Installed Open Grok at $managedCommand" -ForegroundColor Green

    # --- Completions (best-effort) ---

    $completionsDir = Join-Path (Join-Path $openGrokHome 'completions') 'powershell'
    try {
        New-Item -ItemType Directory -Path $completionsDir -Force | Out-Null
        & $managedCommand completions powershell 2>$null |
            Set-Content -LiteralPath (Join-Path $completionsDir 'open-grok.ps1') -ErrorAction SilentlyContinue
    } catch {
        # Completions are optional; a missing subcommand must not fail install.
    }

    # --- Ensure open-grok is on PATH ---

    $pathTarget = if ($env:OPEN_GROK_BIN_DIR) { $binDir } else { $managedBinDir }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $pathEntries = if ($userPath) {
        @($userPath -split ';' | Where-Object { $_ -ne '' })
    } else {
        @()
    }
    $pathNormalized = $pathEntries | ForEach-Object {
        try { [System.IO.Path]::GetFullPath($_) } catch { $_ }
    }
    $pathTargetFull = [System.IO.Path]::GetFullPath($pathTarget)
    if ($pathNormalized -notcontains $pathTargetFull) {
        $newPath = (@($pathTarget) + $pathEntries) -join ';'
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Host "  Added $pathTarget to your User PATH." -ForegroundColor DarkGray
        if ($env:Path -notlike "*$pathTarget*") {
            $env:Path = "$pathTarget;$env:Path"
        }
    } else {
        Write-Host "$pathTarget is already on PATH." -ForegroundColor DarkGray
        if ($env:Path -notlike "*$pathTarget*") {
            $env:Path = "$pathTarget;$env:Path"
        }
    }

    Write-Host ''
    Write-Host "Open Grok $installedVersion installed. Run 'open-grok' to get started." -ForegroundColor Cyan
} finally {
    if (Test-Path -LiteralPath $stageDir) {
        Remove-Item -LiteralPath $stageDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
