#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
#![warn(unreachable_pub)]
#[cfg(all(test, feature = "dhat-heap"))]
#[global_allocator]
static DHAT_ALLOC: dhat::Alloc = dhat::Alloc;
pub(crate) use xai_grok_telemetry::unified_log;
pub use xai_tracing_macros::{teprintln, timed, tprintln};
pub mod active_sessions;
pub mod agent;
pub mod auth;
pub mod builtin;
pub mod claude_import;
pub mod claude_import_state;
pub mod cli_models;
pub mod codex_auth;
pub(crate) mod codex_models;
pub mod config;
pub mod custom_models;
pub mod deepseek_models;
pub mod fireworks_models;
pub mod kimi_models;
pub mod meta_models;
pub mod opencode_go_models;
pub mod wafer_models;
pub mod zai_models;
pub use xai_grok_bundle as bundle;
pub use xai_grok_shell_base::cpu_profile;
pub use xai_grok_shell_base::env;
pub mod extensions;
pub use xai_grok_foreign_sessions as foreign_sessions;
pub mod heap_profile;
pub use xai_grok_http as http;
pub mod inspect;
pub mod instrumentation;
pub mod leader;
pub mod managed_config;
pub mod mcp_doctor;
pub use xai_grok_models as models;
pub mod plugin;
pub mod relay;
pub mod remote;
pub mod sampling;
pub mod session;
pub mod terminal;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tier;
pub mod tools;
pub mod upload;
pub mod util;
