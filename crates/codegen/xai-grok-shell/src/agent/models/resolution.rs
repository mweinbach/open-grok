use super::*;

pub(crate) fn resolve_catalog_key(
    models: &IndexMap<String, ModelEntry>,
    id: &acp::ModelId,
) -> Option<acp::ModelId> {
    let id_str = id.0.as_ref();
    if models.contains_key(id_str) {
        return Some(id.clone());
    }
    models
        .iter()
        .rev()
        .find(|(_, entry)| entry.info.model == id_str)
        .map(|(key, _)| acp::ModelId::new(key.clone()))
}

/// Resolve tool-mode metadata for a selected catalog id or persisted routing
/// slug using the same identity semantics as the rest of model selection.
/// Exact catalog keys win; otherwise the last matching slug wins so user
/// overrides cannot be shadowed by bundled defaults.
pub(crate) fn resolve_model_tool_mode(
    models: &IndexMap<String, ModelEntry>,
    id: &acp::ModelId,
) -> Option<ToolMode> {
    let key = resolve_catalog_key(models, id)?;
    models
        .get(key.0.as_ref())
        .and_then(|entry| entry.info.tool_mode)
}

/// Catalog key for a persisted session model id, restricted to **selectable**
/// entries. A selectable exact-key match wins (as in [`resolve_catalog_key`]);
/// otherwise the last selectable entry whose routing slug matches `id`, so a
/// non-selectable exact-key entry never shadows a selectable slug match.
pub(crate) fn selectable_catalog_key_for_persisted(
    models: &IndexMap<String, ModelEntry>,
    available: &IndexMap<acp::ModelId, acp::ModelInfo>,
    id: &acp::ModelId,
) -> Option<acp::ModelId> {
    if available.contains_key(id) {
        return Some(id.clone());
    }
    let id_str = id.0.as_ref();
    if let Some((key, _)) = models.iter().rev().find(|(key, entry)| {
        available.contains_key(&acp::ModelId::new((*key).clone())) && entry.info.model == id_str
    }) {
        return Some(acp::ModelId::new(key.clone()));
    }
    resolve_catalog_key(models, id).filter(|key| available.contains_key(key))
}

/// A "campaign-only" preferred flip: the default changed and either side's value
/// is an active campaign default, i.e. the change is attributable to a campaign
/// overlay appearing/disappearing rather than a user/CLI/env edit.
pub(crate) fn is_campaign_only_flip(
    old_preferred: &Option<String>,
    new_preferred: &Option<String>,
    campaign_defaults: &std::collections::HashSet<String>,
) -> bool {
    if new_preferred == old_preferred || new_preferred.is_none() {
        return false;
    }
    new_preferred
        .as_ref()
        .is_some_and(|p| campaign_defaults.contains(p))
        || old_preferred
            .as_ref()
            .is_some_and(|p| campaign_defaults.contains(p))
}

/// Pick the default model: CLI > env > config > remote-settings hint, falling
/// back to the bundled default when the catalog is empty or the preferred
/// model isn't present.
pub(crate) fn resolve_default_model(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> (String, ModelEntry, config::ConfigSource) {
    resolve_default_model_with_provider_auth(cfg, catalog, is_session_auth, is_session_auth)
}

/// Resolve only the configured startup preference, without provider-auth
/// filtering or fallback selection. Initialize metadata uses this same helper
/// so client-side provider planning cannot drift from the shell's precedence.
pub(crate) fn resolved_default_model_preference(
    cfg: &config::Config,
) -> Option<config::Resolved<String>> {
    config::resolve_string_flag(
        cfg.default_model_override.as_deref(),
        "GROK_DEFAULT_MODEL",
        cfg.models.default.as_deref(),
        cfg.remote_settings
            .as_ref()
            .and_then(|settings| settings.default_model.as_deref()),
    )
}

pub(crate) fn resolve_default_model_with_provider_auth(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
    has_xai_session: bool,
    has_codex_session: bool,
) -> (String, ModelEntry, config::ConfigSource) {
    let visible: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| {
            model_available_for_provider_auth(e, has_xai_session, has_codex_session)
                && e.info.user_selectable
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let model_pref = resolved_default_model_preference(cfg);

    let first_or_fallback = || -> (String, ModelEntry) {
        let bundled_default = acp::ModelId::new(crate::models::default_model());
        if let Some(key) = resolve_catalog_key(&visible, &bundled_default)
            && let Some(entry) = visible.get(key.0.as_ref())
        {
            return (key.0.to_string(), entry.clone());
        }
        if let Some((key, first)) = visible.first() {
            return (key.clone(), first.clone());
        }
        if let Some((key, entry)) = catalog.iter().find(|(_, e)| e.info.user_selectable) {
            tracing::warn!("no auth-visible selectable model; using first selectable entry");
            return (key.clone(), entry.clone());
        }
        // Pre-catalog/degenerate only: nothing selectable. Set the bundled
        // default's flag from `allowed_models` so no reader treats it as allowed.
        tracing::warn!("no selectable models; falling back to bundled default (pre-catalog)");
        let default_id = crate::models::default_model().to_string();
        let mut entry = ModelEntry::fallback(&default_id, &cfg.endpoints);
        entry.info.user_selectable = match ModelGlobSet::compile(cfg.models.allowed_models.as_ref())
        {
            Ok(None) => true,
            Ok(Some(set)) => set.matches(&default_id, &default_id),
            Err(_) => false,
        };
        (default_id, entry)
    };

    match &model_pref {
        None => {
            let (key, first) = first_or_fallback();
            (key, first, config::ConfigSource::Default)
        }
        Some(pref) => {
            let found = visible
                .get_key_value(&pref.value)
                .or_else(|| visible.iter().find(|(_, m)| m.model == pref.value));

            if let Some((key, entry)) = found {
                (key.clone(), entry.clone(), pref.source)
            } else {
                let is_explicit = matches!(
                    pref.source,
                    config::ConfigSource::Cli
                        | config::ConfigSource::Env
                        | config::ConfigSource::Config
                );
                if is_explicit {
                    tracing::warn!(
                        model_id = %pref.value, source = %pref.source,
                        "preferred model not in available models, falling back"
                    );
                } else {
                    tracing::debug!(
                        model_id = %pref.value, source = %pref.source,
                        "remote default_model not in available models, skipping"
                    );
                }
                // A campaign default missing from the catalog falls back to the
                // pre-campaign default before the first-visible fallback. Gated
                // on the missing pref actually being the campaign-driven config
                // value — a CLI/env pref that misses the catalog is not a
                // campaign problem and must not detour through campaign state.
                let campaign_pref_missing = cfg.models.default_is_campaign_driven
                    && matches!(pref.source, config::ConfigSource::Config);
                if campaign_pref_missing
                    && let Some(prev) = cfg
                        .models
                        .pre_campaign_default
                        .as_deref()
                        .filter(|s| !s.is_empty())
                    && let Some((key, entry)) = visible
                        .get_key_value(prev)
                        .or_else(|| visible.iter().find(|(_, m)| m.model == prev))
                {
                    tracing::info!(
                        unavailable = %pref.value, fallback = %prev,
                        "campaign-driven default unavailable in catalog; recovering the pre-campaign default"
                    );
                    return (key.clone(), entry.clone(), config::ConfigSource::Config);
                }
                let (key, first) = first_or_fallback();
                (key, first, config::ConfigSource::Default)
            }
        }
    }
}

/// Filter hidden and auth-gated entries out of `catalog` and convert to ACP wire format.
pub(crate) fn available_models(
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    available_models_with_provider_auth(catalog, is_session_auth, is_session_auth)
}

pub(crate) fn available_models_with_provider_auth(
    catalog: &IndexMap<String, ModelEntry>,
    has_xai_session: bool,
    has_codex_session: bool,
) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    let visible: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| model_available_for_provider_auth(e, has_xai_session, has_codex_session))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    config::to_acp_model_info(&visible)
}

/// Whether a model's provider is usable with the credentials available now.
/// OAuth-backed providers use their isolated login state; API-key-only
/// providers must resolve a non-empty model-owned or provider-scoped key.
pub(crate) fn model_available_for_provider_auth(
    entry: &ModelEntry,
    has_xai_session: bool,
    has_codex_session: bool,
) -> bool {
    if !entry
        .info
        .visible_for_provider_auth(has_xai_session, has_codex_session)
    {
        return false;
    }

    match entry.info.provider.profile().session_auth {
        xai_grok_sampling_types::BuiltInSessionAuthKind::ApiKeyOnly => {
            resolve_credentials(entry, None)
                .api_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty())
        }
        xai_grok_sampling_types::BuiltInSessionAuthKind::XaiSession
        | xai_grok_sampling_types::BuiltInSessionAuthKind::CodexOAuth => true,
    }
}

/// Compiled glob matcher shared by `allowed_models`, `disabled_models`, and
/// `hidden_models`. Patterns (globset syntax: `*`, `?`, `[...]`) are matched
/// against either the catalog key or the model id.
pub(crate) struct ModelGlobSet(GlobSet);

impl ModelGlobSet {
    /// Compile a filter list (`Ok(None)` for `None`/empty). Fails **closed**: an
    /// invalid pattern returns `Err` listing every bad one for config to reject.
    pub(crate) fn compile(patterns: Option<&Vec<String>>) -> Result<Option<Self>, Vec<String>> {
        let patterns = match patterns {
            Some(p) if !p.is_empty() => p,
            _ => return Ok(None),
        };
        let mut builder = GlobSetBuilder::new();
        let mut invalid = Vec::new();
        for pat in patterns {
            match Glob::new(pat) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(_) => invalid.push(pat.clone()),
            }
        }
        if !invalid.is_empty() {
            return Err(invalid);
        }
        builder
            .build()
            .map(|set| Some(Self(set)))
            .map_err(|e| vec![e.to_string()])
    }

    fn matches(&self, key: &str, model: &str) -> bool {
        self.0.is_match(key) || self.0.is_match(model)
    }
}

/// Single source of truth for the catalog. Applies, in order: `disabled_models`
/// (remove), `allowed_models` (mark `user_selectable`), `hidden_models` (mark
/// `hidden`). Special/internal models (web_search, subagents, …) resolve via
/// `find_model_by_id`/`models()` and ignore `user_selectable`, so they need no
/// exemption. Globs are validated at load (`Config::validate_model_filters`);
/// the arms here fail closed if one slips through.
pub(crate) fn resolve_model_catalog(
    cfg: &config::Config,
    prefetched: Option<IndexMap<String, ModelEntry>>,
) -> IndexMap<String, ModelEntry> {
    resolve_model_catalog_with_provider_catalogs(
        cfg, prefetched, None, None, None, None, None, None,
    )
}

/// Resolve the combined catalog while preserving each provider's independently
/// authenticated remote state before shared filters run.
pub(crate) fn resolve_model_catalog_with_provider_catalogs(
    cfg: &config::Config,
    prefetched: Option<IndexMap<String, ModelEntry>>,
    codex_catalog: Option<&CodexModelsCatalog>,
    kimi_catalog: Option<&KimiModelsCatalog>,
    fireworks_catalog: Option<&FireworksModelsCatalog>,
    deepseek_catalog: Option<&DeepSeekModelsCatalog>,
    meta_catalog: Option<&MetaModelsCatalog>,
    opencode_go_catalog: Option<&OpenCodeGoModelsCatalog>,
) -> IndexMap<String, ModelEntry> {
    resolve_model_catalog_with_provider_catalogs_and_wafer(
        cfg,
        prefetched,
        codex_catalog,
        kimi_catalog,
        fireworks_catalog,
        deepseek_catalog,
        meta_catalog,
        opencode_go_catalog,
        None,
        None,
    )
}

pub(crate) fn resolve_model_catalog_with_provider_catalogs_and_wafer(
    cfg: &config::Config,
    prefetched: Option<IndexMap<String, ModelEntry>>,
    codex_catalog: Option<&CodexModelsCatalog>,
    kimi_catalog: Option<&KimiModelsCatalog>,
    fireworks_catalog: Option<&FireworksModelsCatalog>,
    deepseek_catalog: Option<&DeepSeekModelsCatalog>,
    meta_catalog: Option<&MetaModelsCatalog>,
    opencode_go_catalog: Option<&OpenCodeGoModelsCatalog>,
    wafer_catalog: Option<&crate::wafer_models::WaferModelsCatalog>,
    zai_catalog: Option<&crate::zai_models::ZaiModelsCatalog>,
) -> IndexMap<String, ModelEntry> {
    let codex_entries = codex_catalog.map(CodexModelsCatalog::entries);
    let codex_authoritative = codex_catalog.is_some_and(CodexModelsCatalog::is_authoritative);
    let kimi_entries = kimi_catalog.map(KimiModelsCatalog::entries);
    let kimi_authoritative = kimi_catalog.is_some_and(KimiModelsCatalog::is_authoritative);
    let fireworks_entries = fireworks_catalog.map(FireworksModelsCatalog::entries);
    let fireworks_authoritative =
        fireworks_catalog.is_some_and(FireworksModelsCatalog::is_authoritative);
    let deepseek_entries = deepseek_catalog.map(DeepSeekModelsCatalog::entries);
    let deepseek_authoritative =
        deepseek_catalog.is_some_and(DeepSeekModelsCatalog::is_authoritative);
    let meta_entries = meta_catalog.map(MetaModelsCatalog::entries);
    let meta_authoritative = meta_catalog.is_some_and(MetaModelsCatalog::is_authoritative);
    let opencode_go_entries = opencode_go_catalog.map(OpenCodeGoModelsCatalog::entries);
    let opencode_go_authoritative =
        opencode_go_catalog.is_some_and(OpenCodeGoModelsCatalog::is_authoritative);
    let mut catalog: IndexMap<String, ModelEntry> =
        config::resolve_model_list_with_provider_catalogs(
            cfg,
            prefetched,
            codex_entries,
            codex_authoritative,
            kimi_entries,
            kimi_authoritative,
            fireworks_entries,
            fireworks_authoritative,
            deepseek_entries,
            deepseek_authoritative,
            meta_entries,
            meta_authoritative,
            opencode_go_entries,
            opencode_go_authoritative,
        );

    if let Some(wafer_catalog) = wafer_catalog {
        catalog.retain(|_, entry| {
            entry.info.provider != xai_grok_sampling_types::ModelProvider::Wafer
        });
        catalog.extend(wafer_catalog.entries());
    }

    if let Some(zai_catalog) = zai_catalog {
        catalog
            .retain(|_, entry| entry.info.provider != xai_grok_sampling_types::ModelProvider::Zai);
        catalog.extend(zai_catalog.entries());
    }

    let enabled_open_code_go = cfg
        .models
        .opencode_go_enabled_models
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    catalog.retain(|key, entry| {
        entry.info.provider != xai_grok_sampling_types::ModelProvider::OpenCodeGo
            || enabled_open_code_go.contains(key.as_str())
            || enabled_open_code_go.contains(entry.info.model.as_str())
    });

    if let Ok(Some(disabled)) = ModelGlobSet::compile(cfg.models.disabled_models.as_ref()) {
        let before = catalog.len();
        catalog.retain(|key, entry| !disabled.matches(key, &entry.model));
        let removed = before - catalog.len();
        if removed > 0 {
            tracing::info!(count = removed, "disabled_models: removed from catalog");
        }
    }

    // None/empty allowlist = allow all.
    match ModelGlobSet::compile(cfg.models.allowed_models.as_ref()) {
        Ok(None) => {
            for entry in catalog.values_mut() {
                entry.info.user_selectable = true;
            }
        }
        Ok(Some(allowed)) => {
            for (key, entry) in catalog.iter_mut() {
                entry.info.user_selectable = allowed.matches(key, &entry.model);
            }
        }
        Err(bad) => {
            tracing::error!(patterns = ?bad, "allowed_models: invalid glob(s); marking nothing selectable");
            for entry in catalog.values_mut() {
                entry.info.user_selectable = false;
            }
        }
    }

    if let Ok(Some(hidden)) = ModelGlobSet::compile(cfg.models.hidden_models.as_ref()) {
        for (key, entry) in catalog.iter_mut() {
            if hidden.matches(key, &entry.model) {
                entry.info.hidden = true;
            }
        }
    }

    // Persisted default first; CLI override below wins when set.
    // Only apply if the model supports reasoning effort.
    if let Some(effort) = cfg.models.default_reasoning_effort
        && let Some(default_id) = cfg.models.default.as_deref()
        && let Some(entry) = catalog.get_mut(default_id)
        && entry.info.supports_reasoning_effort
    {
        entry.info.reasoning_effort = Some(effort);
    }

    // Skip non-reasoning models so we don't send the field to providers that reject it.
    // Also skip models whose effort menu does not include the override (e.g. `--effort none`
    // must not stamp `none` onto grok-4.5, which only offers low/medium/high).
    if let Some(effort) = cfg.reasoning_effort_override {
        for entry in catalog.values_mut() {
            if model_offers_reasoning_effort(&entry.info, effort) {
                entry.info.reasoning_effort = Some(effort);
            }
        }
    }

    catalog
}

/// Whether `effort` is a value this model will accept on the wire.
///
/// Uses the server `reasoning_efforts` menu when present; otherwise the
/// built-in low/medium/high/xhigh set (same as the pager legacy menu — no
/// `none`/`minimal`).
pub(crate) fn model_offers_reasoning_effort(
    info: &config::ModelInfo,
    effort: ReasoningEffort,
) -> bool {
    if !info.supports_reasoning_effort {
        return false;
    }
    if info.reasoning_efforts.is_empty() {
        matches!(
            effort,
            ReasoningEffort::Low
                | ReasoningEffort::Medium
                | ReasoningEffort::High
                | ReasoningEffort::Xhigh
        )
    } else {
        info.reasoning_efforts.iter().any(|opt| opt.value == effort)
    }
}

/// True when an active `allowed_models` allowlist leaves no selectable model.
/// (An excluded *default* does not count — that is recoverable by reselection.)
pub(crate) fn allowlist_matches_nothing(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
) -> bool {
    cfg.models
        .allowed_models
        .as_ref()
        .is_some_and(|a| !a.is_empty())
        && !catalog.values().any(|e| e.info.user_selectable)
}

/// Reject an `allowed_models` allowlist that leaves no selectable model, or that
/// excludes an explicitly configured default (`default`/`-m`). Run only against a
/// real catalog (cache/prefetch/fetched), not the bundled bootstrap set.
pub(crate) fn validate_selectable(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
) -> Result<(), String> {
    let Some(allowed) = cfg.models.allowed_models.as_ref().filter(|a| !a.is_empty()) else {
        return Ok(());
    };
    let patterns = allowed.join(", ");
    if !catalog.values().any(|e| e.info.user_selectable) {
        return Err(format!(
            "None of your available models match allowed_models ({patterns}). \
             Broaden the patterns or remove allowed_models, then try again."
        ));
    }
    for (src, id) in [
        ("default", cfg.models.default.as_deref()),
        ("-m flag", cfg.default_model_override.as_deref()),
    ] {
        if let Some(id) = id
            && let Some(entry) = catalog
                .get(id)
                .or_else(|| catalog.values().find(|e| e.model == id))
            && !entry.info.user_selectable
        {
            return Err(format!(
                "\"{id}\" (your {src}) isn't allowed by allowed_models ({patterns}). \
                 Add it to allowed_models, or set a different model."
            ));
        }
    }
    Ok(())
}
