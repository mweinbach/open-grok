//! Content-free product telemetry for feedback drafts and the `/feedback` modal.

use serde::Serialize;

/// How the `/feedback` modal was opened.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackModalEntry {
    Slash,
    Palette,
}

/// The `/feedback` modal opened (emitted once at open, by the pager).
#[derive(Serialize)]
pub struct FeedbackModalOpened {
    pub session_id: String,
    /// Absent for programmatic opens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<FeedbackModalEntry>,
    /// A bare open that lands on Drafts when any exist, else Write.
    pub peek: bool,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackDraftOpKind {
    /// `/feedback <text>` written as a predraft at the queue drain (pager).
    CreatePredraft,
    List,
    Load,
    /// The unknown-outcome recovery write over `drafts/update`; there is no user draft edit.
    Recover,
    Delete,
}

/// Variant-only class of a draft-store failure: the store's error strings embed the session path.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackDraftOpError {
    Busy,
    NotFound,
    InvalidDocument,
    Io,
    Other,
}

/// One user-initiated feedback draft-store operation.
#[derive(Serialize)]
pub struct FeedbackDraftOp {
    pub session_id: String,
    pub op: FeedbackDraftOpKind,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<FeedbackDraftOpError>,
    /// `list` only: rows returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_count: Option<u32>,
    /// `list` only: unreadable rows hidden from the listing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<u32>,
}
