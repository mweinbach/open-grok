//! Intra-turn compaction — orchestration of the
//! `select → sample → guard → commit` pass, generic over
//! [`CompactionItemBuilder`](crate::CompactionItemBuilder).
//!
//! Harness wiring (trigger call sites, LLM transport, metrics backends,
//! state commit) stays per-harness; the Grok chat host
//! wraps these entry points with its tokenizer + metrics observers.

pub mod compact;
pub mod config;
pub mod observer;
pub mod traits;
pub mod trigger;

pub use compact::{
    apply_full_replace_compaction, apply_history_compaction, apply_intra_compaction,
    apply_steps_compaction, error_status_label,
};
pub use config::{
    DEFAULT_COMPACTION_MODEL_NAME, IntraCompactionConfig, IntraCompactionMode, IntraSummarizer,
};
pub use observer::IntraCompactionObserver;
pub use traits::{CompactionStreamProc, CompactionTarget};
pub use trigger::{
    IntraCompactionError, IntraCompactionResult, IntraCompactionTrigger, should_compact,
};
