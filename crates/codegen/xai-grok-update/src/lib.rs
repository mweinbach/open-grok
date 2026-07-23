pub mod auto_update;
pub mod version;
mod version_policy;

pub use auto_update::UpdateStatus;
pub use version::{RELEASE_SOURCE, UpdateConfig, write_version_cache};
pub use version_policy::enforce_version_policy_or_exit;
