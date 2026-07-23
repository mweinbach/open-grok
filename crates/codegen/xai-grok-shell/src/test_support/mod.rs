pub(crate) mod lsp_runtime;

pub(crate) const TEST_MODEL: &str = "test-model";

/// Pre-main guard: point `OPENGROK_HOME` at a per-process temp directory so
/// unit tests can never touch the real `~/.opengrok`.
///
/// A test run against the live home has real consequences: test log spam
/// trips the 5 MB `unified.jsonl` trim (discarding the user's actual log
/// history), model-catalog tests overwrite the live model caches, and
/// session tests create junk `sessions/` directories. `grok_home()` caches
/// its resolution in a process-wide `OnceLock` on first use, so the redirect
/// must happen before ANY test code runs — hence a pre-main constructor
/// rather than a per-test helper. An explicitly exported `OPENGROK_HOME` is
/// respected (that's a deliberate operator choice). Pre-main runs
/// single-threaded, which is what makes `set_var` sound here.
#[ctor::ctor(unsafe)]
fn isolate_grok_home_from_real_home() {
    if std::env::var_os("OPENGROK_HOME").is_none() {
        let dir = std::env::temp_dir().join(format!("opengrok-test-home-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        unsafe {
            std::env::set_var("OPENGROK_HOME", &dir);
        }
    }
}

/// Prepend the hermetic git binary (via `GIT_BIN_PATH`) to `PATH` so that
/// `Command::new("git")` in test helpers resolves to the Bazel-provided
/// static binary instead of relying on system-installed git.
///
/// Safe to call multiple times — only the first call mutates `PATH`.
pub(crate) fn ensure_hermetic_git_on_path() {
    use std::path::PathBuf;
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Ok(git_bin) = std::env::var("GIT_BIN_PATH") {
            let p = PathBuf::from(&git_bin);
            let p = if p.is_relative() {
                std::env::current_dir().unwrap().join(&p)
            } else {
                p
            };
            if let Some(dir) = p.parent() {
                let cur = std::env::var("PATH").unwrap_or_default();
                unsafe {
                    std::env::set_var("PATH", format!("{}:{}", dir.display(), cur));
                    // git-minimal spawns subcommands (`git stash` → `git
                    // update-index`) through its exec path, which is baked to
                    // a build-machine prefix. Helpers live next to the binary,
                    // so point the exec path there. Skip the host-fallback
                    // wrapper: host git must keep its own exec path.
                    if p.file_name().is_some_and(|name| name == "git") {
                        std::env::set_var("GIT_EXEC_PATH", dir);
                    }
                }
            }
        }
    });
}
