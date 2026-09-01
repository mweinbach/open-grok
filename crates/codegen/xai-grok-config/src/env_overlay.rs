use crate::loader::{
    apply_version_overrides_with_registered, expand_env_vars_in_toml, normalize_config_layer,
};
use std::path::{Path, PathBuf};
pub const OPENGROK_CONFIG_ENV: &str = "OPENGROK_CONFIG";
pub const OPENGROK_CONFIG_PATH_ENV: &str = "OPENGROK_CONFIG_PATH";
pub const GROK_CONFIG_ENV: &str = OPENGROK_CONFIG_ENV;
pub const GROK_CONFIG_PATH_ENV: &str = OPENGROK_CONFIG_PATH_ENV;
const MAX_OVERLAY_BYTES: u64 = 4 * 1024 * 1024;
#[derive(Clone, Copy, Debug)]
enum OverlayFormat {
    Json,
    Toml,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlaySource {
    Inline,
    Path(PathBuf),
}
#[derive(Debug, Clone)]
pub struct ResolvedOverlay {
    pub source: OverlaySource,
    pub sections: Vec<String>,
    pub value: toml::Value,
}
fn env_overlay_inputs() -> (Option<String>, Option<PathBuf>) {
    let inline = match std::env::var_os(OPENGROK_CONFIG_ENV) {
        Some(raw) => match raw.into_string() {
            Ok(text) => Some(text),
            Err(_) => {
                tracing::warn!("OPENGROK_CONFIG is not valid UTF-8; ignoring the overlay");
                None
            }
        },
        None => None,
    };
    let path = std::env::var_os(OPENGROK_CONFIG_PATH_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    (inline, path)
}
pub(crate) fn load_env_overlay() -> Option<toml::Value> {
    let (inline, path) = env_overlay_inputs();
    let (overlay, source, sections) = resolve_overlay_detailed(inline.as_deref(), path.as_deref())?;
    let source_label = match source {
        OverlaySource::Inline => OPENGROK_CONFIG_ENV,
        OverlaySource::Path(_) => OPENGROK_CONFIG_PATH_ENV,
    };
    tracing::trace!(
        source = source_label,
        ?sections,
        "resolved OPENGROK_CONFIG overlay"
    );
    Some(overlay)
}
pub fn resolved_env_overlay() -> Option<ResolvedOverlay> {
    let (inline, path) = env_overlay_inputs();
    let (value, source, sections) = resolve_overlay_detailed(inline.as_deref(), path.as_deref())?;
    Some(ResolvedOverlay {
        source,
        sections,
        value,
    })
}
fn resolve_overlay_detailed(
    inline: Option<&str>,
    path: Option<&Path>,
) -> Option<(toml::Value, OverlaySource, Vec<String>)> {
    if let Some(inline) = inline
        && let Some(resolved) = resolve_inline_overlay(inline)
    {
        return Some(resolved);
    }
    resolve_path_overlay(path?)
}
#[cfg(test)]
fn resolve_overlay(inline: Option<&str>, path: Option<&Path>) -> Option<toml::Value> {
    resolve_overlay_detailed(inline, path).map(|(value, _, _)| value)
}
fn resolve_inline_overlay(inline: &str) -> Option<(toml::Value, OverlaySource, Vec<String>)> {
    let trimmed = inline.trim();
    if trimmed.is_empty() {
        return None;
    }
    let overlay = parse_overlay(trimmed, OverlayFormat::Json, OPENGROK_CONFIG_ENV)?;
    finalize_overlay(overlay, OPENGROK_CONFIG_ENV, OverlaySource::Inline)
}
fn resolve_path_overlay(path: &Path) -> Option<(toml::Value, OverlaySource, Vec<String>)> {
    let raw = read_capped_overlay_file(path)?;
    let format = match path.extension() {
        Some(ext) if ext.eq_ignore_ascii_case("json") => OverlayFormat::Json,
        _ => OverlayFormat::Toml,
    };
    let overlay = parse_overlay(&raw, format, OPENGROK_CONFIG_PATH_ENV)?;
    finalize_overlay(
        overlay,
        OPENGROK_CONFIG_PATH_ENV,
        OverlaySource::Path(path.to_path_buf()),
    )
}
fn read_capped_overlay_file(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(path = %path.display(), error = %error, "OPENGROK_CONFIG_PATH is unreadable; ignoring the overlay");
            return None;
        }
    };
    match file.metadata() {
        Ok(meta) if !meta.file_type().is_file() => {
            tracing::warn!(path = %path.display(), "OPENGROK_CONFIG_PATH is not a regular file; ignoring the overlay");
            return None;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), error = %error, "OPENGROK_CONFIG_PATH is unreadable; ignoring the overlay");
            return None;
        }
    }
    let mut raw = String::new();
    if let Err(error) = file.take(MAX_OVERLAY_BYTES + 1).read_to_string(&mut raw) {
        tracing::warn!(path = %path.display(), error = %error, "OPENGROK_CONFIG_PATH is unreadable; ignoring the overlay");
        return None;
    }
    if raw.len() as u64 > MAX_OVERLAY_BYTES {
        tracing::warn!(path = %path.display(), max = MAX_OVERLAY_BYTES, "OPENGROK_CONFIG_PATH exceeds the max overlay size; ignoring the overlay");
        return None;
    }
    Some(raw)
}
fn finalize_overlay(
    mut overlay: toml::Value,
    source_label: &str,
    source: OverlaySource,
) -> Option<(toml::Value, OverlaySource, Vec<String>)> {
    expand_env_vars_in_toml(&mut overlay);
    if let Err(error) = apply_version_overrides_with_registered(&mut overlay) {
        tracing::warn!(source = source_label, error = %error, "config overlay `version_overrides` failed to apply; ignoring this overlay candidate");
        return None;
    }
    let _ = crate::campaigns::take_campaign_entries(&mut overlay, "env_overlay");
    if let Some(table) = overlay.as_table_mut() {
        crate::config_override::retain_overlay_allowed(table);
    }
    normalize_config_layer(&mut overlay);
    let sections: Vec<String> = overlay
        .as_table()
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default();
    if sections.is_empty() {
        return None;
    }
    Some((overlay, source, sections))
}
fn parse_overlay(raw: &str, format: OverlayFormat, source: &str) -> Option<toml::Value> {
    let value = match format {
        OverlayFormat::Json => {
            let mut json: serde_json::Value = match serde_json::from_str(raw) {
                Ok(json) => json,
                Err(_) => {
                    tracing::warn!(
                        source,
                        "config overlay is not valid JSON; ignoring the overlay"
                    );
                    return None;
                }
            };
            strip_json_nulls(&mut json);
            match toml::Value::try_from(json) {
                Ok(value) => value,
                Err(_) => {
                    tracing::warn!(
                        source,
                        "config overlay JSON is not representable as TOML; ignoring the overlay"
                    );
                    return None;
                }
            }
        }
        OverlayFormat::Toml => match toml::from_str::<toml::Value>(raw) {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(
                    source,
                    "config overlay is not valid TOML; ignoring the overlay"
                );
                return None;
            }
        },
    };
    if !value.is_table() {
        tracing::warn!(
            source,
            "config overlay is not a table; ignoring the overlay"
        );
        return None;
    }
    Some(value)
}
fn strip_json_nulls(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|_, value| !value.is_null());
            for value in map.values_mut() {
                strip_json_nulls(value);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                strip_json_nulls(item);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}
#[cfg(test)]
#[path = "env_overlay_tests.rs"]
mod tests;
