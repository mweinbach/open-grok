#!/usr/bin/env node
// Runs once after npm install/update. Reads the grok binary from the
// matching per-platform optional dependency (@xai-official/grok-<platform>)
// and installs it to ~/.grok/bin/ using versioned filenames:
//
//   Unix:    grok-<version>  +  grok  (symlink)
//   Windows: grok-<version>.exe  +  grok.exe  (copy)
//
// Versioned files ensure running processes are never disrupted on macOS
// (replacing a binary that a running process has mmap'd causes SIGKILL
// because the kernel can no longer verify the code signature).
const path = require('path');
const fs = require('fs');
const os = require('os');
const zlib = require('zlib');
const { execSync } = require('child_process');
const TOML = require('@iarna/toml');

const CANONICAL_DIR = path.join(os.homedir(), '.grok', 'bin');

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
    console.error(`@xai-official/grok: unsupported platform ${key}`);
    process.exit(0);
}

// Resolve the per-platform sibling package's directory. The matching
// optionalDependency is installed by npm based on `os`/`cpu` filters; the
// other five are silently skipped. If the matching one is missing, npm was
// likely invoked with --no-optional or the platform is unsupported.
function resolvePlatformPackageDir() {
    const platformPkg = `@xai-official/grok-${key}`;
    try {
        return path.dirname(require.resolve(`${platformPkg}/package.json`));
    } catch {
        return null;
    }
}

let version;
try { version = require('../package.json').version; } catch {}
if (!version) {
    console.error('@xai-official/grok: unable to determine version');
    process.exit(0);
}

const IS_WINDOWS = process.platform === 'win32';
const EXE = IS_WINDOWS ? '.exe' : '';

fs.mkdirSync(CANONICAL_DIR, { recursive: true });

// Install a vendored binary: versioned filename + symlink (Unix) or copy (Windows).
// Binaries are shipped brotli-compressed in the per-platform npm tarball to keep
// each sub-package well under npm's ~200 MB tarball limit. This function
// decompresses them before installing into the canonical layout.
function installBinary(binName, sourceDir, vendorSubpath) {
    const brPath = path.join(sourceDir, 'bin', vendorSubpath + '.br');
    const rawPath = path.join(sourceDir, 'bin', vendorSubpath);
    let vendoredBinPath;
    if (fs.existsSync(brPath)) {
        const compressed = fs.readFileSync(brPath);
        const decompressed = zlib.brotliDecompressSync(compressed);
        vendoredBinPath = rawPath;
        fs.writeFileSync(vendoredBinPath, decompressed);
        if (!IS_WINDOWS) fs.chmodSync(vendoredBinPath, 0o755);
        try { fs.unlinkSync(brPath); } catch {}
    } else if (fs.existsSync(rawPath)) {
        vendoredBinPath = rawPath;
    } else {
        console.error(`@xai-official/grok: missing binary at ${brPath}`);
        return false;
    }

    const versionedName = `${binName}-${version}${EXE}`;
    const versionedPath = path.join(CANONICAL_DIR, versionedName);
    const canonicalName = `${binName}${EXE}`;
    const canonicalPath = path.join(CANONICAL_DIR, canonicalName);

    // Only copy if this exact version isn't already installed.
    if (!fs.existsSync(versionedPath)) {
        const tmpPath = versionedPath + `.tmp.${process.pid}`;
        try {
            fs.copyFileSync(vendoredBinPath, tmpPath);
            if (!IS_WINDOWS) fs.chmodSync(tmpPath, 0o755);
            fs.renameSync(tmpPath, versionedPath);
        } finally {
            try { fs.unlinkSync(tmpPath); } catch {}
        }
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
                console.error(`@xai-official/grok: failed to update ${canonicalPath}: ${e2.message}`);
                console.error('Close all running grok processes and try again.');
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

    console.log(`${binName} ${version} installed to ${canonicalPath} -> ${versionedName}`);
    return true;
}

// Best-effort cleanup of old versioned binaries for a given binary name.
// Keeps the current version and the previous one (in case a process is still
// running the old binary and hasn't fully loaded all pages yet).
// Uses an exact prefix match + hyphen + digit to avoid grok-* matching grok-pager-*.
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
            .sort((a, b) => {
                const pa = a.slice(prefix.length).split('.').map(Number);
                const pb = b.slice(prefix.length).split('.').map(Number);
                for (let i = 0; i < 3; i++) {
                    if ((pa[i] || 0) !== (pb[i] || 0)) return (pb[i] || 0) - (pa[i] || 0);
                }
                return 0;
            });
        for (const old of versionedBinaries.slice(1)) {
            try { fs.unlinkSync(path.join(CANONICAL_DIR, old)); } catch {}
        }
    } catch {}
}

const platformDir = resolvePlatformPackageDir();
if (!platformDir) {
    console.error(`@xai-official/grok: platform package @xai-official/grok-${key} not installed.`);
    console.error('  This usually means npm was invoked with --no-optional, or the install failed.');
    console.error('  Try: npm install -g @xai-official/grok');
    process.exit(0);
}

installBinary('grok', platformDir, `grok${EXE}`);
cleanupOldVersions('grok');
cleanupOldVersions('grok-pager');

// Write installer config
const configDir = path.join(os.homedir(), '.grok');
const configPath = path.join(configDir, 'config.toml');
let obj = {};
try { obj = TOML.parse(fs.readFileSync(configPath, 'utf8')); } catch { }
obj.cli ??= {};
obj.cli.installer = 'npm';

// Persist the npm registry so `grok update` and the trampoline use the same one.
const npmRegistry = process.env.GROK_NPM_REGISTRY
    || (() => {
        try {
            const resolved = execSync(
                'npm config get @xai-official:registry',
                { encoding: 'utf8', timeout: 5000 }
            ).trim();
            if (resolved && resolved !== 'undefined') return resolved;
        } catch {}
        return null;
    })();

if (npmRegistry) {
    obj.cli.npm_registry = npmRegistry;
}

fs.writeFileSync(configPath, TOML.stringify(obj), 'utf8');

// Shell completions: print setup hints (no silent shell config mutation).
// Set GROK_INSTALL_COMPLETIONS=1 to auto-generate to ~/.grok/completions.
const GROK_PATH = path.join(CANONICAL_DIR, `grok${EXE}`);
if (process.env.GROK_INSTALL_COMPLETIONS === '1' && !IS_WINDOWS) {
    try {
        const { spawnSync } = require('child_process');
        const completionsDir = path.join(os.homedir(), '.grok', 'completions');
        const bashPath = path.join(completionsDir, 'bash', 'grok.bash');
        const zshPath = path.join(completionsDir, 'zsh', '_grok');
        fs.mkdirSync(path.dirname(bashPath), { recursive: true });
        fs.mkdirSync(path.dirname(zshPath), { recursive: true });
        const bashRes = spawnSync(GROK_PATH, ['completions', 'bash'], { encoding: 'utf8' });
        if (bashRes.status === 0) fs.writeFileSync(bashPath, bashRes.stdout);
        const zshRes = spawnSync(GROK_PATH, ['completions', 'zsh'], { encoding: 'utf8' });
        if (zshRes.status === 0) fs.writeFileSync(zshPath, zshRes.stdout);
        console.log('Completions generated to ~/.grok/completions (bash/zsh)');
    } catch {}
} else if (!IS_WINDOWS) {
    console.log('Tip: grok completions bash > ~/.local/share/bash-completion/completions/grok');
    console.log('     grok completions zsh  > ~/.zsh/completions/_grok');
}
