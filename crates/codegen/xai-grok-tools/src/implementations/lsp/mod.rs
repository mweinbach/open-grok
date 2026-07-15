pub mod client;
pub mod config;
pub mod dispatch;
pub mod format;
pub mod manager;
pub mod restart;
mod types;

#[cfg(test)]
mod tests;

pub use dispatch::LspBackendAdapter;
pub use manager::{DiagnosticsSummary, LspManager, drain_lsp_diagnostics};
pub use restart::restart_monitor;
pub use types::{
    DiagnosticEntry, DiagnosticSeverityLevel, FileDiagnosticEntry, LspBackend, LspConfig,
    LspOperation, LspToolInput, LspToolResult,
};

// ── Shared types used across submodules ─────────────────────────────────

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_lsp::lsp_types::{
    Diagnostic, Position, TextDocumentIdentifier, TextDocumentPositionParams, Url,
};

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("failed to spawn LSP server: {0}")]
    SpawnFailed(String),
    #[error("LSP server '{0}' timed out after {1:?}")]
    Timeout(String, std::time::Duration),
    #[error("LSP initialization failed: {0}")]
    InitFailed(String),
    #[error("LSP request failed: {0}")]
    RequestFailed(String),
    #[error("invalid file path")]
    InvalidPath,
}

pub type DiagnosticsMap = Arc<std::sync::RwLock<HashMap<String, Vec<Diagnostic>>>>;
pub type DiagnosticsNotify = Arc<tokio::sync::Notify>;
pub type LspMainLoop = async_lsp::MainLoop<async_lsp::router::Router<()>>;

pub fn file_uri(path: &Path) -> Result<Url, LspError> {
    Url::from_file_path(path).map_err(|_| LspError::InvalidPath)
}

pub fn text_document_position(
    path: &Path,
    line: u32,
    column: u32,
) -> Result<TextDocumentPositionParams, LspError> {
    Ok(TextDocumentPositionParams {
        text_document: TextDocumentIdentifier {
            uri: file_uri(path)?,
        },
        position: Position {
            line,
            character: column,
        },
    })
}
