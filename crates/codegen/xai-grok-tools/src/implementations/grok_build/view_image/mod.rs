//! ViewImage — attach a local image file to the conversation as vision input.
//!
//! `read_file` can already render images, but models ported from harnesses
//! with a dedicated `view_image` tool (and models on text-only read tools,
//! e.g. the Codex toolset) do not discover that; observed fallback is
//! base64-dumping the file into terminal output, which the session layer
//! cannot attach. This tool makes the capability explicit and discoverable.
//!
//! Returns [`ReadFileOutput::ImageContent`] so the shell's existing
//! inline-attach path (which keys on that output variant) turns the result
//! into vision tokens with no additional plumbing.

use crate::implementations::read_file::bytes_to_metadata;
use crate::types::output::ReadFileOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::{
    Cwd, DisplayCwd, FileSystem, GitignoreFilter, RespectGitignore, display_cwd_or_cwd,
    resolve_model_path,
};
use crate::types::tool::{ToolKind, ToolNamespace};

fn unsupported_image_error(path: &std::path::Path, file_bytes: &[u8]) -> String {
    match bytes_to_metadata(file_bytes) {
        Ok(metadata) => format!(
            "Error: {} is not a supported image file (PNG, JPEG, GIF, WEBP, HEIC). \
             Detected type: {} ({} bytes). Use read_file for text and other file types.",
            path.display(),
            metadata.mime_type,
            metadata.size
        ),
        Err(_) if file_bytes.is_empty() => format!(
            "Error: {} is empty (0 bytes), not a supported image file (PNG, JPEG, GIF, WEBP, HEIC).",
            path.display()
        ),
        Err(_) => format!(
            "Error: {} is not a supported image file (PNG, JPEG, GIF, WEBP, HEIC). \
             Unrecognized file contents ({} bytes). Use read_file for text and other file types.",
            path.display(),
            file_bytes.len()
        ),
    }
}

pub(crate) const DESCRIPTION: &str = r#"View an image file.

Usage:
- Attaches the image at the given path to the conversation so you can see it visually.
- The path can be a relative path in the workspace or an absolute path.
- Supported formats: PNG, JPEG, GIF, WEBP, HEIC (large images are downscaled automatically; HEIC is converted to JPEG).
- Use this to inspect screenshots, rendered previews, diagrams, or any other image on disk. Do not print image bytes (e.g. base64) to the terminal — that does not attach anything; use this tool instead.
- In code mode, pass the result to the global image(...) helper to attach it: image(await tools.view_image({path})). Do not print the result."#;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ViewImageInput {
    #[schemars(
        description = "The path of the image file to view. You can use either a relative path in the workspace or an absolute path."
    )]
    pub path: String,
}

/// New-architecture `ViewImage` tool.
///
/// Params: `()` — no per-tool configuration.
#[derive(Default, Debug)]
pub struct ViewImageTool;

impl crate::types::tool_metadata::ToolMetadata for ViewImageTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }
    fn description_template(&self) -> &str {
        DESCRIPTION
    }
    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for ViewImageTool {
    type Args = ViewImageInput;
    type Output = ReadFileOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("view_image").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "view_image",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.view_image", skip_all, fields(path = %input.path))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ViewImageInput,
    ) -> Result<ReadFileOutput, xai_tool_runtime::ToolError> {
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let (cwd, display_cwd, fs);
        {
            let res = resources.lock().await;
            cwd = res.require::<Cwd>()?.0.clone();
            display_cwd = res.get::<DisplayCwd>().map(|d| d.0.clone());
            fs = res.require::<FileSystem>()?.0.clone();
        }
        let joined_path = resolve_model_path(&cwd, display_cwd.as_deref(), &input.path);
        let path = crate::util::fs::try_canonicalize(&joined_path)
            .await
            .unwrap_or(joined_path);
        {
            let res = resources.lock().await;
            let respect_gitignore = res.get::<RespectGitignore>().is_some_and(|r| r.0);
            if respect_gitignore
                && let Some(filter) = res.get::<GitignoreFilter>()
                && filter.is_ignored(&path)
            {
                let display_dcwd = display_cwd_or_cwd(&cwd, display_cwd.as_deref());
                return Ok(ReadFileOutput::FileReadError(format!(
                    "Error: {} is ignored by .gitignore and cannot be read.",
                    display_dcwd.join(&input.path).display()
                )));
            }
        }
        let file_bytes = match fs.read_file(&path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(?e, "Failed to read image file");
                let display_dcwd = display_cwd_or_cwd(&cwd, display_cwd.as_deref());
                let display_path = display_dcwd.join(&input.path);
                return Ok(match e.io_error_kind() {
                    Some(std::io::ErrorKind::NotFound) => ReadFileOutput::FileNotFound(format!(
                        "Error: {} does not exist.",
                        display_path.display()
                    )),
                    Some(std::io::ErrorKind::IsADirectory) => {
                        ReadFileOutput::IsADirectory(format!(
                            "Error: {} is a directory, not a file.",
                            display_path.display()
                        ))
                    }
                    Some(std::io::ErrorKind::PermissionDenied) => ReadFileOutput::PermissionDenied(
                        format!("Permission denied: {}", display_path.display()),
                    ),
                    _ => ReadFileOutput::FileReadError(format!(
                        "Failed to read file: {}, {e}",
                        display_path.display()
                    )),
                });
            }
        };
        match bytes_to_metadata(&file_bytes) {
            Ok(metadata) if metadata.is_image() => {
                Ok(crate::implementations::read_file::image::image_read_output(
                    file_bytes,
                    metadata.mime_type,
                )
                .await)
            }
            _ => Ok(ReadFileOutput::FileReadError(unsupported_image_error(
                &path,
                &file_bytes,
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tool_metadata::ToolMetadata;

    #[test]
    fn metadata_is_read_only_grok_build() {
        let tool = ViewImageTool;
        assert_eq!(ToolMetadata::kind(&tool), ToolKind::Read);
        assert_eq!(tool.tool_namespace(), ToolNamespace::GrokBuild);
        assert!(xai_tool_runtime::Tool::capabilities(&tool).is_read_only);
    }

    #[test]
    fn input_schema_uses_path_field() {
        let schema = serde_json::to_value(schemars::schema_for!(ViewImageInput)).unwrap();
        // The shell's inline-attach path reads the file location from the
        // `path` (or `target_file`) argument; renaming this field silently
        // breaks vision attachment.
        assert!(schema["properties"]["path"].is_object(), "{schema}");
    }

    #[test]
    fn unsupported_image_error_reports_empty_file() {
        let msg = unsupported_image_error(std::path::Path::new("/tmp/ob_list.png"), b"");
        assert!(msg.contains("empty (0 bytes)"), "{msg}");
        assert!(msg.contains("/tmp/ob_list.png"), "{msg}");
    }

    #[test]
    fn unsupported_image_error_reports_detected_type() {
        let pdf = b"%PDF-1.7 some bytes";
        let msg = unsupported_image_error(std::path::Path::new("/tmp/doc.pdf"), pdf);
        assert!(msg.contains("Detected type: application/pdf"), "{msg}");
        assert!(msg.contains(&format!("{} bytes", pdf.len())), "{msg}");
    }

    #[test]
    fn unsupported_image_error_reports_unrecognized_contents() {
        let msg = unsupported_image_error(std::path::Path::new("/tmp/note.txt"), b"hello world");
        assert!(
            msg.contains("Unrecognized file contents (11 bytes)"),
            "{msg}"
        );
    }
}
