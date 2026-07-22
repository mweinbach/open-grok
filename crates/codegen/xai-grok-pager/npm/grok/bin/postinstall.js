#!/usr/bin/env node
// Runs once after npm install/update. Reads the binary from the matching
// per-platform package and installs it under the Open Grok namespace:
//
//   Unix:    open-grok-<version>  +  open-grok  (symlink)
//   Windows: open-grok-<version>.exe  +  open-grok.exe  (copy)
//
// Versioned files ensure running processes are never disrupted on macOS
// (replacing a binary that a running process has mmap'd causes SIGKILL
// because the kernel can no longer verify the code signature).
const path = require('path');
const fs = require('fs');
const os = require('os');
const zlib = require('zlib');

// $OPENGROK_HOME (else ~/.opengrok), matching the Rust grok_home() including its
// canonicalized-home default. Lets fleets relocate the binary off a slow $HOME
// (NFS); old code hardcoded os.homedir().
function defaultOpengrokHome() {
    const home = os.homedir();
    try { return path.join(fs.realpathSync(home), '.opengrok'); } catch { return path.join(home, '.opengrok'); }
}
const OPENGROK_HOME = process.env.OPENGROK_HOME ?? defaultOpengrokHome();
const CANONICAL_DIR = path.join(OPENGROK_HOME, 'bin');

const key = `${process.platform}-${process.arch}`;
const SUPPORTED = new Set([
    'darwin-arm64',
    'darwin-x64',
    'linux-x64',
    'linux-arm64',
    'win32-x64',
    'win32-arm64',
]);
if (!SUPPORTED.has(key)) {
    console.error(`open-grok: unsupported platform ${key}`);
    process.exit(0);
}

// Resolve the per-platform sibling package's directory. The matching
// optionalDependency is installed by npm based on `os`/`cpu` filters; the
// other five are silently skipped. If the matching one is missing, npm was
// likely invoked with --no-optional or the platform is unsupported.
function resolvePlatformPackageDir() {
    const platformPkg = `@mweinbach/open-grok-${key}`;
    try {
        return path.dirname(require.resolve(`${platformPkg}/package.json`));
    } catch {
        return null;
    }
}

let version;
try { version = require('../package.json').version; } catch {}
if (!version) {
    console.error('open-grok: unable to determine version');
    process.exit(0);
}

const IS_WINDOWS = process.platform === 'win32';
const EXE = IS_WINDOWS ? '.exe' : '';

fs.mkdirSync(CANONICAL_DIR, { recursive: true });

function writeVendorBinary(brPath, rawPath, destPath) {
    const tmp = destPath + `.tmp.${process.pid}`;
    try {
        if (fs.existsSync(brPath)) {
            fs.writeFileSync(tmp, zlib.brotliDecompressSync(fs.readFileSync(brPath)));
        } else if (fs.existsSync(rawPath)) {
            fs.copyFileSync(rawPath, tmp);
        } else {
            return false;
        }
        if (!IS_WINDOWS) fs.chmodSync(tmp, 0o755);
        fs.renameSync(tmp, destPath);
        return true;
    } catch {
        return false;
    } finally {
        try { fs.unlinkSync(tmp); } catch {}
    }
}

function installBinary(binName, sourceDir, vendorSubpath) {
    const brPath = path.join(sourceDir, 'bin', vendorSubpath + '.br');
    const rawPath = path.join(sourceDir, 'bin', vendorSubpath);

    const versionedName = `${binName}-${version}${EXE}`;
    const versionedPath = path.join(CANONICAL_DIR, versionedName);
    const canonicalName = `${binName}${EXE}`;
    const canonicalPath = path.join(CANONICAL_DIR, canonicalName);

    // Skip if this exact version is already installed.
    if (!fs.existsSync(versionedPath) && !writeVendorBinary(brPath, rawPath, versionedPath)) {
        console.error(`open-grok: missing binary at ${brPath}`);
        return false;
    }

    if (IS_WINDOWS) {
        // Symlinks need elevation on Windows; copy instead. If the exe is
        // locked by a running process, rename it aside then retry.
        const oldPath = canonicalPath + '.old';
        try { fs.unlinkSync(oldPath); } catch {} // stale backup from prior update
        try {
            try { fs.unlinkSync(canonicalPath); } catch {}
            fs.copyFileSync(versionedPath, canonicalPath);
        } catch (e) {
            try {
                fs.renameSync(canonicalPath, oldPath);
                try {
                    fs.copyFileSync(versionedPath, canonicalPath);
                } catch (copyErr) {
                    // Rollback: restore the old binary so the install isn't broken.
                    try { fs.renameSync(oldPath, canonicalPath); } catch {}
                    throw copyErr;
                }
            } catch (e2) {
                console.error(`open-grok: failed to update ${canonicalPath}: ${e2.message}`);
                console.error('Close all running Open Grok processes and try again.');
                return false;
            }
        }
    } else {
        // Atomic symlink swap.
        const tmpLink = canonicalPath + `.link.${process.pid}`;
        try { fs.unlinkSync(tmpLink); } catch {}
        fs.symlinkSync(versionedName, tmpLink);
        fs.renameSync(tmpLink, canonicalPath);
    }

    // Don't report a broken wire-up as success.
    if (!fs.existsSync(canonicalPath)) {
        console.error(`open-grok: ${canonicalName} did not resolve after install`);
        return false;
    }

    console.log(`${binName} ${version} installed to ${canonicalPath} -> ${versionedName}`);
    return true;
}

// Comparator: sort "<prefix>X.Y.Z" filenames by version, newest first.
function byVersionDescending(prefix) {
    return (a, b) => {
        const pa = a.slice(prefix.length).split('.').map(Number);
        const pb = b.slice(prefix.length).split('.').map(Number);
        for (let i = 0; i < 3; i++) {
            if ((pa[i] || 0) !== (pb[i] || 0)) return (pb[i] || 0) - (pa[i] || 0);
        }
        return 0;
    };
}

// Best-effort cleanup of old versioned binaries for a given binary name.
// Keeps the current version and the previous one (in case a process is still
// running the old binary and hasn't fully loaded all pages yet).
// Uses an exact prefix match + hyphen + digit.
function cleanupOldVersions(binName) {
    try {
        const prefix = `${binName}-`;
        const currentVersioned = `${binName}-${version}${EXE}`;
        const entries = fs.readdirSync(CANONICAL_DIR);
        const versionedBinaries = entries
            .filter(e => {
                if (!e.startsWith(prefix)) return false;
                if (e.includes('.tmp.') || e.includes('.link.')) return false;
                if (e === currentVersioned) return false;
                const suffix = e.slice(prefix.length);
                return /^\d/.test(suffix);
            })
            .sort(byVersionDescending(prefix));
        for (const old of versionedBinaries.slice(1)) {
            try { fs.unlinkSync(path.join(CANONICAL_DIR, old)); } catch {}
        }
    } catch {}
}

const platformDir = resolvePlatformPackageDir();
if (!platformDir) {
    console.error(`open-grok: platform package @mweinbach/open-grok-${key} not installed.`);
    console.error('  This usually means npm was invoked with --no-optional, or the install failed.');
    console.error('  Try reinstalling the open-grok npm package with optional dependencies enabled.');
    process.exit(0);
}

installBinary('open-grok', platformDir, `open-grok${EXE}`);
cleanupOldVersions('open-grok');

// Shell completions: print setup hints (no silent shell config mutation).
// Set OPENGROK_INSTALL_COMPLETIONS=1 to auto-generate under OPENGROK_HOME.
const OPEN_GROK_PATH = path.join(CANONICAL_DIR, `open-grok${EXE}`);
if (process.env.OPENGROK_INSTALL_COMPLETIONS === '1' && !IS_WINDOWS) {
    try {
        const { spawnSync } = require('child_process');
        const completionsDir = path.join(OPENGROK_HOME, 'completions');
        const bashPath = path.join(completionsDir, 'bash', 'open-grok.bash');
        const zshPath = path.join(completionsDir, 'zsh', '_open-grok');
        fs.mkdirSync(path.dirname(bashPath), { recursive: true });
        fs.mkdirSync(path.dirname(zshPath), { recursive: true });
        const bashRes = spawnSync(OPEN_GROK_PATH, ['completions', 'bash'], { encoding: 'utf8' });
        if (bashRes.status === 0) fs.writeFileSync(bashPath, bashRes.stdout);
        const zshRes = spawnSync(OPEN_GROK_PATH, ['completions', 'zsh'], { encoding: 'utf8' });
        if (zshRes.status === 0) fs.writeFileSync(zshPath, zshRes.stdout);
        console.log(`Completions generated to ${completionsDir} (bash/zsh)`);
    } catch {}
} else if (!IS_WINDOWS) {
    console.log('Tip: open-grok completions bash > ~/.local/share/bash-completion/completions/open-grok');
    console.log('     open-grok completions zsh  > ~/.zsh/completions/_open-grok');
}
