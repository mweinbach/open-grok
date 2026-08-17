//! Per-model image-input capability.
//!
//! GLM-5 / 5.1 / 5.2 / 5.3 are text-only (Z.AI documents text input/output
//! only; vision lives on a separate GLM-5V family). Catalog ids, wire slugs,
//! and Fireworks `glm-5p2` paths all go through the same matcher so a
//! mid-session model switch cannot send vision tokens to a text-only route.

/// Model-facing error when a text-only model is given image input or tries
/// to read an image file.
pub(crate) const TEXT_ONLY_MODEL_IMAGE_ERROR: &str =
    "Error: this model is text-only and cannot accept image input.";

/// Whether `model` (catalog key or wire slug) accepts image / vision input.
///
/// Defaults to `true` for unknown slugs so existing vision-capable Grok and
/// Codex routes stay unchanged. Returns `false` only for the curated
/// text-only GLM-5 family.
pub(crate) fn model_accepts_images(model: &str) -> bool {
    !is_text_only_glm5_model(model)
}

/// `true` for glm-5, glm-5.1, glm-5.2, glm-5.3 and their non-vision suffixes
/// (`-fast`, `-preview`, Fireworks `glm-5p2`, catalog keys such as
/// `zai:glm-5.2`). Vision variants (`glm-5v`, `glm-5.2v`) stay multimodal.
pub(crate) fn is_text_only_glm5_model(model: &str) -> bool {
    let slug = normalize_model_slug(model);
    !is_glm5_vision(&slug) && is_glm5_text_family(&slug)
}

fn normalize_model_slug(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    let slug = lower.rsplit(['/', ':']).next().unwrap_or(lower.as_str());
    // Fireworks routes glm-5.2 as `glm-5p2` / `glm-5p2-fast`.
    if let Some(rest) = slug.strip_prefix("glm-5p")
        && rest.starts_with(|c: char| c.is_ascii_digit())
    {
        return format!("glm-5.{rest}");
    }
    slug.to_owned()
}

fn is_glm5_vision(slug: &str) -> bool {
    let Some(rest) = slug.strip_prefix("glm-5") else {
        return false;
    };
    let rest = rest
        .strip_prefix(['.', '-', '_'])
        .unwrap_or(rest)
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start_matches(['.', '-', '_']);
    rest.starts_with('v') || rest.starts_with("vl") || rest.contains("vision")
}

fn is_glm5_text_family(slug: &str) -> bool {
    let Some(rest) = slug.strip_prefix("glm-5") else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('-') || rest.starts_with('_') {
        return true;
    }
    let Some(after_dot) = rest.strip_prefix('.') else {
        return false;
    };
    after_dot.starts_with('1') || after_dot.starts_with('2') || after_dot.starts_with('3')
}

/// ACP `meta.acceptsImages` / `meta.inputModalities` for a catalog entry.
pub(crate) fn acp_accepts_images(catalog_key: &str, wire_model: &str) -> bool {
    model_accepts_images(catalog_key) && model_accepts_images(wire_model)
}

/// Tool-result text when a text-only model reads an image (or PDF-as-images).
pub(crate) fn text_only_image_read_error(path: &str) -> String {
    format!("{TEXT_ONLY_MODEL_IMAGE_ERROR} Image `{path}` was not attached.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_glm5_family_is_text_only() {
        for id in [
            "glm-5",
            "GLM-5",
            "glm-5-turbo",
            "glm-5.1",
            "glm-5.1-preview",
            "glm-5.2",
            "glm-5.2-fast",
            "glm-5.3",
            "zai:glm-5.2",
            "zai:glm-5.3",
            "accounts/fireworks/models/glm-5p2",
            "accounts/fireworks/routers/glm-5p2-fast",
        ] {
            assert!(is_text_only_glm5_model(id), "{id} should be text-only");
            assert!(!model_accepts_images(id), "{id} must reject image input");
        }
    }

    #[test]
    fn vision_and_other_models_still_accept_images() {
        for id in [
            "glm-5v",
            "glm-5v-turbo",
            "glm-5.2v",
            "glm-5.2-vl",
            "glm-4.6",
            "glm-4.5v",
            "grok-4.6",
            "gpt-5.4",
            "unknown",
        ] {
            assert!(
                !is_text_only_glm5_model(id),
                "{id} should not match the text-only GLM-5 family"
            );
            assert!(model_accepts_images(id), "{id} should accept images");
        }
    }

    #[test]
    fn acp_accepts_images_is_false_when_either_id_is_text_only() {
        assert!(!acp_accepts_images("zai:glm-5.2", "glm-5.2"));
        assert!(!acp_accepts_images(
            "glm-5.2",
            "accounts/fireworks/models/glm-5p2"
        ));
        assert!(acp_accepts_images("grok-4.6", "grok-4.6"));
    }

    #[test]
    fn image_read_error_names_the_path() {
        let err = text_only_image_read_error("shot.png");
        assert!(err.contains("text-only"));
        assert!(err.contains("shot.png"));
    }
}
