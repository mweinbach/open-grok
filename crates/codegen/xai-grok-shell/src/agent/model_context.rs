//! Codex-compatible context-window and auto-compact overrides.
//!
//! OpenAI documents top-level `model_context_window` and
//! `model_auto_compact_token_limit` so GPT-5.6 sessions can opt into the
//! model's published ~1.05M raw window instead of the product-tuned catalog
//! default (~353K effective). Per-model `[model.*] context_window` still wins.

use std::num::NonZeroU64;

use indexmap::IndexMap;
use xai_grok_sampling_types::ModelProvider;

use super::config::{Config, ModelEntry};

/// Published GPT-5.6 Sol/Terra/Luna raw context window (OpenAI model spec).
pub(crate) const GPT56_PUBLISHED_RAW_CONTEXT_WINDOW: u64 = 1_050_000;
/// Codex picker/session budget is 95% of the raw window.
pub(crate) const CODEX_EFFECTIVE_CONTEXT_PERCENT: u64 = 95;
/// Codex default auto-compact is 90% of the raw window.
pub(crate) const CODEX_RAW_AUTO_COMPACT_PERCENT: u64 = 90;

/// True for the GPT-5.6 family, including the `gpt-5.6` alias that routes to Sol.
pub(crate) fn is_gpt56_family(slug: &str) -> bool {
    let slug = slug.trim();
    slug == "gpt-5.6" || slug.starts_with("gpt-5.6-")
}

/// Published raw maximum for a routing slug, when Open Grok knows one.
pub(crate) fn published_raw_max_context_window(slug: &str) -> Option<u64> {
    is_gpt56_family(slug).then_some(GPT56_PUBLISHED_RAW_CONTEXT_WINDOW)
}

/// 95%-effective session budget from a raw Codex context window.
pub(crate) fn effective_from_raw(raw: u64) -> Option<NonZeroU64> {
    NonZeroU64::new(raw.saturating_mul(CODEX_EFFECTIVE_CONTEXT_PERCENT) / 100)
}

/// 90%-of-raw auto-compact threshold expressed in the 95%-effective coordinate
/// used by Open Grok's picker and session sampling config.
pub(crate) fn derived_auto_compact_from_effective(effective: u64) -> u64 {
    effective.saturating_mul(CODEX_RAW_AUTO_COMPACT_PERCENT) / CODEX_EFFECTIVE_CONTEXT_PERCENT
}

fn inferred_raw_from_effective(effective: u64) -> u64 {
    effective.saturating_mul(100) / CODEX_EFFECTIVE_CONTEXT_PERCENT
}

fn raw_max_context_window(slug: &str, live_max: Option<u64>, current_effective: u64) -> u64 {
    [
        live_max,
        published_raw_max_context_window(slug),
        Some(inferred_raw_from_effective(current_effective).max(current_effective)),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(current_effective)
}

/// Raise Codex model session windows from `cfg.model_context_window`.
///
/// The config value is the raw token budget (Codex `config.toml` semantics).
/// Open Grok stores the 95%-effective picker/session window. Keys with an
/// explicit `[model.*] context_window` are left untouched.
pub(crate) fn apply_codex_style_context_overrides(
    cfg: &Config,
    models: &mut IndexMap<String, ModelEntry>,
    live_max_for_slug: impl Fn(&str) -> Option<u64>,
) {
    let Some(requested_raw) = cfg.model_context_window.filter(|tokens| *tokens > 0) else {
        return;
    };
    for (key, entry) in models.iter_mut() {
        if entry.info.provider != ModelProvider::Codex {
            continue;
        }
        if cfg
            .config_models
            .get(key)
            .and_then(|model_override| model_override.context_window)
            .is_some()
        {
            continue;
        }
        let slug = entry.info.model.as_str();
        let max_raw = raw_max_context_window(
            slug,
            live_max_for_slug(slug),
            entry.info.context_window.get(),
        );
        let raw = requested_raw.min(max_raw);
        if let Some(effective) = effective_from_raw(raw) {
            entry.info.context_window = effective;
        }
    }
}

/// Absolute auto-compact token limit after Codex-style config overrides.
///
/// `None` means "use the live catalog limit or the historical 90%-of-raw
/// fallback" — the same signal `ModelsManager::codex_compaction_metadata`
/// used before these keys existed.
///
/// Only the Codex-compatible top-level keys change this. A `[model.*]
/// context_window` still wins the session window, but it is not a compact
/// opt-in: copying the catalog default (`353000`) must keep the live
/// `auto_compact_token_limit`.
pub(crate) fn overridden_auto_compact_token_limit(
    cfg: &Config,
    session_effective: u64,
) -> Option<u64> {
    let derived = derived_auto_compact_from_effective(session_effective);
    if let Some(user_limit) = cfg
        .model_auto_compact_token_limit
        .filter(|tokens| *tokens > 0)
    {
        return Some(user_limit.min(derived.max(1)));
    }
    cfg.model_context_window
        .filter(|tokens| *tokens > 0)
        .is_some()
        .then_some(derived)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{ConfigModelOverride, EndpointsConfig};

    fn sol_entry() -> ModelEntry {
        let mut entry = ModelEntry::fallback("gpt-5.6-sol", &EndpointsConfig::default());
        entry.info.provider = ModelProvider::Codex;
        entry.info.model = "gpt-5.6-sol".to_owned();
        entry.info.context_window = NonZeroU64::new(353_000).unwrap();
        entry
    }

    #[test]
    fn gpt56_family_matches_alias_and_named_tiers() {
        assert!(is_gpt56_family("gpt-5.6"));
        assert!(is_gpt56_family("gpt-5.6-sol"));
        assert!(is_gpt56_family("gpt-5.6-terra"));
        assert!(is_gpt56_family(" gpt-5.6-luna "));
        assert!(!is_gpt56_family("gpt-5.5"));
        assert!(!is_gpt56_family("grok-4.5"));
    }

    #[test]
    fn million_raw_projects_to_codex_effective_budget() {
        assert_eq!(effective_from_raw(1_000_000).unwrap().get(), 950_000);
        assert_eq!(effective_from_raw(1_050_000).unwrap().get(), 997_500);
        assert_eq!(derived_auto_compact_from_effective(950_000), 900_000);
    }

    #[test]
    fn global_override_raises_codex_gpt56_and_skips_xai() {
        let mut cfg = Config::default();
        cfg.model_context_window = Some(1_000_000);
        let mut models = IndexMap::new();
        models.insert("gpt-5.6-sol".to_owned(), sol_entry());
        let mut xai = ModelEntry::fallback("grok-4.5", &EndpointsConfig::default());
        xai.info.context_window = NonZeroU64::new(131_072).unwrap();
        models.insert("grok-4.5".to_owned(), xai);

        apply_codex_style_context_overrides(&cfg, &mut models, |_| None);

        assert_eq!(models["gpt-5.6-sol"].info.context_window.get(), 950_000);
        assert_eq!(models["grok-4.5"].info.context_window.get(), 131_072);
    }

    #[test]
    fn published_spec_clamps_raw_window_to_1_05m() {
        let mut cfg = Config::default();
        cfg.model_context_window = Some(4_000_000);
        let mut models = IndexMap::new();
        models.insert("gpt-5.6-sol".to_owned(), sol_entry());

        apply_codex_style_context_overrides(&cfg, &mut models, |_| None);

        assert_eq!(models["gpt-5.6-sol"].info.context_window.get(), 997_500);
    }

    #[test]
    fn live_catalog_max_can_raise_ceiling_above_published() {
        let mut cfg = Config::default();
        cfg.model_context_window = Some(2_000_000);
        let mut models = IndexMap::new();
        models.insert("gpt-5.6-sol".to_owned(), sol_entry());

        apply_codex_style_context_overrides(&cfg, &mut models, |_| Some(2_000_000));

        assert_eq!(models["gpt-5.6-sol"].info.context_window.get(), 1_900_000);
    }

    #[test]
    fn per_model_context_window_wins_over_global() {
        let mut cfg = Config::default();
        cfg.model_context_window = Some(1_000_000);
        cfg.config_models.insert(
            "gpt-5.6-sol".to_owned(),
            ConfigModelOverride {
                context_window: Some(400_000),
                ..ConfigModelOverride::default()
            },
        );
        let mut models = IndexMap::new();
        let mut sol = sol_entry();
        sol.info.context_window = NonZeroU64::new(400_000).unwrap();
        models.insert("gpt-5.6-sol".to_owned(), sol);

        apply_codex_style_context_overrides(&cfg, &mut models, |_| None);

        assert_eq!(models["gpt-5.6-sol"].info.context_window.get(), 400_000);
    }

    #[test]
    fn auto_compact_override_clamps_to_ninety_percent_of_raw() {
        let mut cfg = Config::default();
        cfg.model_context_window = Some(1_000_000);
        cfg.model_auto_compact_token_limit = Some(900_000);
        assert_eq!(
            overridden_auto_compact_token_limit(&cfg, 950_000),
            Some(900_000)
        );

        cfg.model_auto_compact_token_limit = Some(2_000_000);
        assert_eq!(
            overridden_auto_compact_token_limit(&cfg, 950_000),
            Some(900_000),
            "absolute compact limit cannot exceed 90% of the resolved raw window"
        );
    }

    #[test]
    fn window_override_without_compact_limit_derives_900k() {
        let mut cfg = Config::default();
        cfg.model_context_window = Some(1_000_000);
        assert_eq!(
            overridden_auto_compact_token_limit(&cfg, 950_000),
            Some(900_000)
        );
    }

    #[test]
    fn unset_overrides_leave_catalog_compaction_alone() {
        let cfg = Config::default();
        assert_eq!(overridden_auto_compact_token_limit(&cfg, 353_400), None);
    }

    #[test]
    fn documented_per_model_window_is_not_a_compact_opt_in() {
        let mut cfg = Config::default();
        cfg.config_models.insert(
            "gpt-5.6-sol".to_owned(),
            ConfigModelOverride {
                context_window: Some(353_000),
                ..ConfigModelOverride::default()
            },
        );
        assert_eq!(
            overridden_auto_compact_token_limit(&cfg, 353_000),
            None,
            "copying the catalog context_window must keep the live Codex compact limit"
        );
        assert_eq!(
            derived_auto_compact_from_effective(353_000),
            334_421,
            "the old per-model window gate would have replaced ~300k with this derived limit"
        );
    }
}
