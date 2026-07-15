pub mod config;
pub mod grok_auth_credentials;
pub mod hooks;

// The foundation utilities live in `xai-grok-shell-base` (upstream of this
// crate so they build in parallel). Re-exported at the original paths so
// existing `crate::util::…` / `xai_grok_shell::util::…` users compile
// unchanged.
pub use xai_grok_shell_base::util::*;

/// Aborts the wrapped tokio task when dropped.
///
/// Use to tie a spawned helper task's lifetime to an async scope so that
/// cancelling the parent future (e.g. a turn abort dropping the tool loop)
/// also tears down the helper instead of leaving it running detached.
/// Aborting an already-finished task is a no-op, so this is safe to hold
/// across normal scope exit too.
pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
