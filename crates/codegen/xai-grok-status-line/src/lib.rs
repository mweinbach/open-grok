pub mod config;
pub mod context;

pub const STATUS_LINE_CAPABILITY: &str = "x.ai/statusLine";

pub const CLIENT_STATUS_LINE_META: &str = "clientStatusLine";

#[cfg(any(test, feature = "test-support"))]
pub use config::test_support;
pub use config::{ResolvedStatusLine, StatusLineConfig, StatusLineItem, StatusLineType};
pub use context::{
    STATUS_LINE_SCHEMA_VERSION, StatusLineContext, StatusLineContextWindow, StatusLineCost,
    StatusLineEffort, StatusLineModel, StatusLineRepo, StatusLineSessionUsage, StatusLineTrigger,
    StatusLineTurn, StatusLineWorkspace, StatusLineWorktree,
};
