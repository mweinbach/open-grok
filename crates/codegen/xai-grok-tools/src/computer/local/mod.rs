pub mod cgroup;
#[cfg(unix)]
pub mod embedded_search_tools;
pub mod file_system;
pub mod mock_fs;
#[cfg(unix)]
pub mod shell_state;
pub mod terminal;

pub use cgroup::{CgroupMemoryConfig, PROCESS_OOM_EXIT_CODE};
pub use file_system::LocalFs;
pub use mock_fs::MockFs;
pub use terminal::{ExitStatus, LocalTerminalBackend};

/// Per-backend enable state for the bash-harness `find`→`bfs` / `grep`→`ugrep`
/// shadows.
///
/// Resolved once by the host (config.toml `[toolset.bash]` / env / requirements)
/// and baked into a [`LocalTerminalBackend`] at creation. Keeping it on the
/// backend instead of a process-global means a subagent that reuses the parent's
/// `LocalTerminalBackend` inherits the parent's shadows — it can't overwrite the
/// enable state for bash that later runs on the shared backend. Defaults to
/// both-on for standalone backends with no host wiring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchShadowConfig {
    pub find_bfs: bool,
    pub grep_ugrep: bool,
}

impl Default for SearchShadowConfig {
    fn default() -> Self {
        Self {
            find_bfs: true,
            grep_ugrep: true,
        }
    }
}
