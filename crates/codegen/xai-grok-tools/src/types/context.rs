use std::collections::HashMap;

const MAX_LINES_READ_DEFAULT: usize = 1_000;

/// Client-configurable truncation settings.
/// All fields are optional — `None` means "use the tool's built-in default".
///
/// There is deliberately no per-line cap: clipping long lines silently
/// corrupts single-line files (minified JSON, data dumps) with no way for
/// the model to recover the clipped bytes. Non-skill reads are bounded by
/// the whole-read `MAX_NUM_TOKENS` cap instead (skill files are exempt from
/// all read limits by design). Other agent CLIs likewise apply no
/// per-line cap. The wire field (`TruncationConfig.max_chars_per_line` in
/// grok-tools.proto) is deprecated and ignored.
#[derive(Debug, Clone, Default)]
pub struct TruncationConfig {
    /// Max total output bytes for any tool. Default: 40KB.
    pub default_max_output_bytes: Option<usize>,
    /// Per-tool overrides keyed by canonical tool name.
    pub per_tool_max_output_bytes: HashMap<String, usize>,
    /// Max lines to read (read_file). Default: 1000.
    pub max_lines_read: Option<usize>,
    /// Inline cap for MCP tool results only (bytes). Consulted by the MCP
    /// truncation path (`mcp_max_output_bytes_for`) between the per-tool map
    /// and `default_max_output_bytes`. Deliberately separate from
    /// `default_max_output_bytes` so an MCP-specific override (e.g. a repo's
    /// `[mcp] max_output_bytes`) never changes non-MCP readers like the
    /// opencode bash cap.
    pub mcp_max_output_bytes: Option<usize>,
}

impl TruncationConfig {
    /// Resolved max lines per `read_file` window.
    pub fn max_lines_read(&self) -> usize {
        self.max_lines_read.unwrap_or(MAX_LINES_READ_DEFAULT)
    }

    /// Resolve the max output bytes for a specific tool.
    ///
    /// Precedence: per-tool override > default override > built-in fallback.
    pub fn max_output_bytes_for(&self, tool_name: &str, builtin_default: usize) -> usize {
        if let Some(&per_tool) = self.per_tool_max_output_bytes.get(tool_name) {
            return per_tool;
        }
        self.default_max_output_bytes.unwrap_or(builtin_default)
    }

    /// Resolve the max output bytes for an **MCP** payload.
    ///
    /// Precedence: per-tool override > MCP-specific override
    /// (`mcp_max_output_bytes`) > default override > built-in fallback.
    ///
    /// Only the MCP truncation path (`util::mcp_truncate`) should call this;
    /// non-MCP tools keep using [`Self::max_output_bytes_for`] so that an
    /// MCP-specific override never bleeds into their caps.
    pub fn mcp_max_output_bytes_for(&self, tool_name: &str, builtin_default: usize) -> usize {
        if let Some(&per_tool) = self.per_tool_max_output_bytes.get(tool_name) {
            return per_tool;
        }
        self.mcp_max_output_bytes
            .or(self.default_max_output_bytes)
            .unwrap_or(builtin_default)
    }

    /// Replace template placeholders in a tool description with current config values.
    ///
    /// Recognized placeholders:
    /// - `{max_lines_read}` — from `max_lines_read` (default 1000)
    /// - `{max_output_bytes}` — resolved via `max_output_bytes_for(tool_name, builtin_default)`
    /// - `{max_chars_per_line}` — fixed display value for opencode-compat
    ///   descriptions only; the opencode `read` tool clips at its own
    ///   hardcoded `MAX_LINE_LENGTH` (2000), independent of this config.
    ///   grok_build `read_file` never clips lines.
    ///
    /// Returns the original string unchanged if no placeholders are present.
    pub fn interpolate_description(
        &self,
        description: &str,
        tool_name: &str,
        builtin_output_default: usize,
    ) -> String {
        description
            .replace("{max_lines_read}", &self.max_lines_read().to_string())
            .replace("{max_chars_per_line}", "2000")
            .replace(
                "{max_output_bytes}",
                &self
                    .max_output_bytes_for(tool_name, builtin_output_default)
                    .to_string(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_lines_read_default_and_override() {
        assert_eq!(
            TruncationConfig::default().max_lines_read(),
            MAX_LINES_READ_DEFAULT
        );
        let cfg = TruncationConfig {
            max_lines_read: Some(50),
            ..Default::default()
        };
        assert_eq!(cfg.max_lines_read(), 50);
    }

    #[test]
    fn mcp_max_output_bytes_for_lookup_order() {
        // per-tool > mcp-specific > default > builtin
        let cfg = TruncationConfig {
            default_max_output_bytes: Some(1_000),
            per_tool_max_output_bytes: HashMap::from([("use_tool".to_string(), 111)]),
            mcp_max_output_bytes: Some(500),
            ..Default::default()
        };
        assert_eq!(cfg.mcp_max_output_bytes_for("use_tool", 9_999), 111);
        assert_eq!(cfg.mcp_max_output_bytes_for("CallMcpTool", 9_999), 500);

        let no_mcp = TruncationConfig {
            default_max_output_bytes: Some(1_000),
            ..Default::default()
        };
        assert_eq!(no_mcp.mcp_max_output_bytes_for("use_tool", 9_999), 1_000);
        assert_eq!(
            TruncationConfig::default().mcp_max_output_bytes_for("use_tool", 9_999),
            9_999
        );
    }

    #[test]
    fn mcp_override_does_not_bleed_into_non_mcp_lookup() {
        // Regression: the MCP-specific cap must not change what non-MCP
        // readers (e.g. opencode bash via `max_output_bytes_for("bash", ..)`)
        // resolve.
        let cfg = TruncationConfig {
            mcp_max_output_bytes: Some(123),
            ..Default::default()
        };
        assert_eq!(cfg.max_output_bytes_for("bash", 20_000), 20_000);
        assert_eq!(cfg.mcp_max_output_bytes_for("use_tool", 20_000), 123);
    }
}
