//! Per-turn `config_files.json` trace artifact — currently disabled.

use super::turn::PromptTraceContext;

/// Build and upload `config_files.json` for the turn.
pub(crate) async fn upload_config_files(ctx: &PromptTraceContext) {
    super::manifest::skip_artifact(
        &ctx.artifact_tracker,
        "config_files.json",
        "config_content_upload_disabled",
    );
}
