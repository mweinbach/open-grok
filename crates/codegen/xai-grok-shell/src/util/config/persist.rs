use super::load::load_config_from_toml;
use super::mcp::{Config, user_config_path};
use anyhow::Result;
use std::path::Path;
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;
use xai_grok_agent::prompt::skills::SkillsConfig;
use xai_grok_config::fs_atomic::BoundDest;

/// Process-wide write lock for `~/.opengrok/config.toml`.
///
/// Serializes the read-modify-write in `save_config` so two rapid
/// settings toggles can't interleave and clobber each other.
static SAVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
pub(crate) fn parse_existing_config_toml(s: &str) -> Result<TomlValue, toml::de::Error> {
    if s.trim().is_empty() {
        return Ok(TomlValue::Table(TomlMap::new()));
    }
    toml::from_str(s)
}
/// Settings-save body. Caller must hold [`ConfigWriteGuard`].
/// Re-resolve `dest` before publish. A retarget must not merge A onto B.
async fn save_config_locked(
    guard: ConfigWriteGuard,
    slot: &Path,
    dest: BoundDest,
    config: &Config,
) -> Result<()> {
    let dest = require_same_user_config_dest(slot, &dest)?;
    let mut root: TomlValue = match tokio::fs::read_to_string(dest.as_path()).await {
        Ok(s) => match parse_existing_config_toml(&s) {
            Ok(v) => v,
            Err(parse_err) => {
                return Err(anyhow::anyhow!(
                    "refusing to overwrite unparseable {}: {}; save a backup \
                         and fix the syntax error before retrying",
                    slot.display(),
                    parse_err,
                ));
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TomlValue::Table(TomlMap::new()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "refusing to overwrite unreadable {}: {e}",
                slot.display()
            ));
        }
    };
    if !matches!(root, TomlValue::Table(_)) {
        root = TomlValue::Table(TomlMap::new());
    }
    let table = root.as_table_mut().expect("root must be a table");
    merge_section(table, "cli", &config.cli);
    merge_section(table, "models", &config.models);
    // `merge_section` deliberately preserves absent serialized fields so
    // unmodeled future keys survive a settings write. These two fields are
    // modeled `Option`s with an explicit Settings clear action, so `None` is a
    // tombstone rather than "leave the old value alone".
    if let Some(TomlValue::Table(models)) = table.get_mut("models") {
        if config.models.recap.is_none() {
            models.remove("recap");
        }
        if config.models.memory.is_none() {
            models.remove("memory");
        }
    }
    merge_section(table, "ui", &config.ui);
    merge_section(table, "harness", &config.harness);
    merge_section(table, "session", &config.session);
    merge_ask_user_question_section(table, &config.ask_user_question);
    merge_perplexity_web_search_section(table, &config.perplexity_web_search);
    merge_toolset_subsection(table, "web_search_source", &config.web_search_source);
    merge_toolset_subsection(table, "x_search", &config.x_search);
    if config.privacy == super::mcp::PrivacyConfig::default() {
        table.remove("privacy");
    } else {
        merge_section(table, "privacy", &config.privacy);
    }

    if config.skills == SkillsConfig::default() {
        table.remove("skills");
    } else {
        merge_section(table, "skills", &config.skills);
    }
    let toml_str = toml::to_string_pretty(&root)?;
    let dest = require_same_user_config_dest(slot, &dest)?;
    guard
        .run_blocking(move || atomic_write_resolved_string(&dest, &toml_str))
        .await
        .map_err(|e| anyhow::anyhow!("config write task failed: {e}"))??;
    Ok(())
}
/// Guard for a user `config.toml` read-modify-write: [`SAVE_LOCK`] plus the config-init flock —
/// without the flock leg, a SAVE_LOCK writer and a flock writer silently drop each other's edits.
#[must_use]
pub(crate) struct ConfigWriteGuard {
    _save: tokio::sync::MutexGuard<'static, ()>,
    _flock: std::fs::File,
}
impl ConfigWriteGuard {
    /// Run `f` on the blocking pool with this guard living in that task.
    /// Cancelling the returned future must not drop the guard in the async frame.
    pub(crate) async fn run_blocking<T, F>(self, f: F) -> Result<T, tokio::task::JoinError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            let _guard = self;
            f()
        })
        .await
    }
}
/// Acquire the user `config.toml` write guard (SAVE_LOCK ⊃ init flock) on the blocking pool;
/// fails closed — callers must not fall back to an unguarded write.
/// `SAVE_LOCK` moves into that task so a cancelled await cannot release it while flock is still being acquired.
pub(crate) async fn lock_config_writes() -> std::io::Result<ConfigWriteGuard> {
    let save = SAVE_LOCK.lock().await;
    let grok_home = crate::util::grok_home::grok_home();
    tokio::task::spawn_blocking(move || {
        let flock = acquire_init_lock(&grok_home)?;
        Ok(ConfigWriteGuard {
            _save: save,
            _flock: flock,
        })
    })
    .await
    .map_err(|e| std::io::Error::other(format!("config lock task failed: {e}")))?
}
/// Exclusive advisory `flock` on `<grok_home>/.config-init.lock`, retried briefly, serializing
/// `config.toml` read-modify-writes; only `WouldBlock` retries, and the file is never removed.
pub fn acquire_init_lock(grok_home: &std::path::Path) -> std::io::Result<std::fs::File> {
    use fs2::FileExt;
    let _ = std::fs::create_dir_all(grok_home);
    let lock_path = grok_home.join(".config-init.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    for _ in 0..50 {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!("timed out waiting for {} after 1s", lock_path.display()),
    ))
}
/// Read a file, treating only `NotFound` as empty. Hard read errors (EACCES,
/// EIO) propagate so callers don't clobber an unreadable file on the next write.
pub(crate) fn read_to_string_or_empty(path: &std::path::Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}
/// Bind the follow dest then read it. Pair with [`atomic_write_follow_bound`].
pub fn read_follow_bound(path: &std::path::Path) -> std::io::Result<(BoundDest, String)> {
    let dest = bind_user_config_dest(path)?;
    let content = read_to_string_or_empty(dest.as_path())?;
    Ok((dest, content))
}
fn bind_user_config_dest(path: &std::path::Path) -> std::io::Result<BoundDest> {
    bind_user_config_dest_with(path, true, xai_grok_config::user_grok_home().is_some())
}
fn bind_user_config_dest_with(
    path: &std::path::Path,
    follow_leaf: bool,
    has_user_home: bool,
) -> std::io::Result<BoundDest> {
    if follow_leaf && has_user_home {
        xai_grok_config::fs_atomic::bind_follow_destination(path)
    } else {
        xai_grok_config::fs_atomic::bind_slot_destination(path)
    }
}
/// Re-resolve with the same follow/slot policy used to bind `dest`.
pub(crate) fn require_same_user_config_dest(
    slot: &std::path::Path,
    dest: &BoundDest,
) -> std::io::Result<BoundDest> {
    require_same_user_config_dest_with(slot, dest, xai_grok_config::user_grok_home().is_some())
}
fn require_same_user_config_dest_with(
    slot: &std::path::Path,
    dest: &BoundDest,
    has_user_home: bool,
) -> std::io::Result<BoundDest> {
    let now = bind_user_config_dest_with(slot, true, has_user_home)?;
    if now != *dest {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "config destination for {} changed from {} to {}",
                slot.display(),
                dest.as_path().display(),
                now.as_path().display()
            ),
        ));
    }
    Ok(now)
}
/// Publish onto `dest` only if `slot` still resolves to it.
pub fn atomic_write_follow_bound(
    slot: &std::path::Path,
    dest: &BoundDest,
    content: &str,
) -> std::io::Result<()> {
    let dest = require_same_user_config_dest(slot, dest)?;
    atomic_write_resolved_string(&dest, content)
}
/// Atomic write via temp file then `rename`. Follows a leaf symlink.
/// Project `.opengrok/config.toml` must use [`atomic_replace_string`].
/// User-config RMW must bind dest before load ([`read_follow_bound`] + [`atomic_write_follow_bound`]).
pub fn atomic_write_string(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    atomic_write_string_inner(path, content, true)
}
/// Like [`atomic_write_string`], but `rename` replaces a leaf symlink inode.
pub fn atomic_replace_string(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    atomic_write_string_inner(path, content, false)
}
/// Follow-leaf bind + read. Caller already classified `path` as user `config.toml`.
pub fn read_follow_leaf(path: &std::path::Path) -> std::io::Result<(BoundDest, String)> {
    let dest = xai_grok_config::fs_atomic::bind_follow_destination(path)?;
    let content = read_to_string_or_empty(dest.as_path())?;
    Ok((dest, content))
}
/// Follow-leaf publish if `slot` still resolves to `dest`.
pub fn atomic_write_follow_leaf(
    slot: &std::path::Path,
    dest: &BoundDest,
    content: &str,
) -> std::io::Result<()> {
    let dest = xai_grok_config::fs_atomic::require_same_bound_destination(slot, dest)?;
    atomic_write_resolved_string(&dest, content)
}
fn atomic_write_string_inner(
    path: &std::path::Path,
    content: &str,
    follow_leaf: bool,
) -> std::io::Result<()> {
    let dest = if follow_leaf {
        let first = xai_grok_config::fs_atomic::bind_follow_destination(path)?;
        xai_grok_config::fs_atomic::require_same_bound_destination(path, &first)?
    } else {
        xai_grok_config::fs_atomic::bind_slot_destination(path)?
    };
    atomic_write_resolved_string(&dest, content)
}
/// Publish onto an already-bound destination. Temp inherits the current dest mode.
pub(crate) fn atomic_write_resolved_string(dest: &BoundDest, content: &str) -> std::io::Result<()> {
    if let Some(parent) = dest.as_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    xai_grok_config::fs_atomic::write_atomically_bound(dest, content, None)
}
/// Merge `[toolset.ask_user_question]` into the root table. `[toolset]` is
/// deliberately NOT merged wholesale — it carries runtime-only structs
/// (`web_search` sampler etc.) whose serialized defaults must never land in
/// the user file — so only this settings-writable sub-table round-trips.
fn merge_ask_user_question_section(
    table: &mut TomlMap<String, TomlValue>,
    ask: &crate::tools::config::AskUserQuestionToolConfig,
) {
    if ask.timeout_enabled.is_none() && ask.timeout_secs.is_none() {
        return;
    }
    let toolset = table
        .entry("toolset".to_string())
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    if !matches!(toolset, TomlValue::Table(_)) {
        *toolset = TomlValue::Table(TomlMap::new());
    }
    if let TomlValue::Table(toolset_table) = toolset {
        merge_section(toolset_table, "ask_user_question", ask);
    }
}

fn merge_perplexity_web_search_section(
    table: &mut TomlMap<String, TomlValue>,
    perplexity: &crate::tools::config::PerplexityWebSearchToolConfig,
) {
    merge_toolset_subsection(table, "perplexity_web_search", perplexity);
}

/// Merge a serializable value into `[toolset.<key>]`, recovering from a
/// non-table `toolset` scalar the same way `merge_section` does.
fn merge_toolset_subsection<T: serde::Serialize>(
    table: &mut TomlMap<String, TomlValue>,
    key: &str,
    value: &T,
) {
    let toolset = table
        .entry("toolset".to_string())
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    if !matches!(toolset, TomlValue::Table(_)) {
        *toolset = TomlValue::Table(TomlMap::new());
    }
    if let TomlValue::Table(toolset_table) = toolset {
        merge_section(toolset_table, key, value);
    }
}

/// Merge serialized fields of `value` into `table[key]`, preserving any
/// existing keys not present in the serialized output. This prevents
/// unmodeled fields (e.g. pager-written `show_timestamps`, `auto_dark_theme`)
/// from being silently dropped when `save_config` round-trips the struct.
/// Deep-merge `incoming` into `existing`: nested tables recurse; scalars replace.
fn merge_toml_tables(
    existing: &mut TomlMap<String, TomlValue>,
    incoming: TomlMap<String, TomlValue>,
) {
    for (field_key, field_val) in incoming {
        match (existing.get_mut(&field_key), field_val) {
            (Some(TomlValue::Table(dst)), TomlValue::Table(src)) => {
                merge_toml_tables(dst, src);
            }
            (_, v) => {
                existing.insert(field_key, v);
            }
        }
    }
}
fn merge_section<T: serde::Serialize>(
    table: &mut TomlMap<String, TomlValue>,
    key: &str,
    value: &T,
) {
    match TomlValue::try_from(value) {
        Ok(TomlValue::Table(new_fields)) if !new_fields.is_empty() => {
            let section = table
                .entry(key.to_string())
                .or_insert_with(|| TomlValue::Table(TomlMap::new()));
            if let TomlValue::Table(existing) = section {
                merge_toml_tables(existing, new_fields);
            } else {
                *section = TomlValue::Table(new_fields);
            }
        }
        Ok(TomlValue::Table(_)) => {}
        Ok(_) | Err(_) => {
            table.remove(key);
        }
    }
}
/// Update settings with a read-modify-write, preserving unrelated fields.
pub async fn update_config<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Config),
{
    let guard = lock_config_writes().await?;
    let path = user_config_path();
    let dest = bind_user_config_dest(&path)?;
    let root: TomlValue = crate::config::load_config_file(dest.as_path())?;
    let mut cfg = load_config_from_toml(&root);
    f(&mut cfg);
    save_config_locked(guard, &path, dest, &cfg).await
}

fn parse_user_config_root(contents: &str, path: &std::path::Path) -> Result<TomlValue> {
    if contents.trim().is_empty() {
        return Ok(TomlValue::Table(TomlMap::new()));
    }
    let mut root = toml::from_str::<TomlValue>(contents).map_err(|parse_err| {
        anyhow::anyhow!(
            "refusing to overwrite unparseable {}: {}; save a backup and fix the syntax error before retrying",
            path.display(),
            parse_err,
        )
    })?;
    if !root.is_table() {
        root = TomlValue::Table(TomlMap::new());
    }
    Ok(root)
}

/// Merge `incoming` into `[model.<key>]` only. Sibling model tables and
/// unrelated root keys are left alone.
pub(crate) fn upsert_config_model_table(
    root: &mut TomlValue,
    key: &str,
    incoming: TomlMap<String, TomlValue>,
) {
    if !root.is_table() {
        *root = TomlValue::Table(TomlMap::new());
    }
    let table = root.as_table_mut().expect("root must be a table");
    let model_section = table
        .entry("model".to_string())
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    if !model_section.is_table() {
        *model_section = TomlValue::Table(TomlMap::new());
    }
    let Some(TomlValue::Table(models)) = table.get_mut("model") else {
        return;
    };
    let drop_api_key = incoming.get("env_key").is_some();
    match models.get_mut(key) {
        Some(TomlValue::Table(existing)) => {
            merge_toml_tables(existing, incoming);
            if drop_api_key {
                existing.remove("api_key");
            }
        }
        _ => {
            models.insert(key.to_string(), TomlValue::Table(incoming));
        }
    }
}

/// Remove `[model.<key>]`. An empty `[model]` section is dropped.
pub(crate) fn delete_config_model_table(root: &mut TomlValue, key: &str) -> bool {
    let Some(table) = root.as_table_mut() else {
        return false;
    };
    let Some(TomlValue::Table(models)) = table.get_mut("model") else {
        return false;
    };
    let removed = models.remove(key).is_some();
    if models.is_empty() {
        table.remove("model");
    }
    removed
}

pub(crate) fn persist_custom_model_upsert_to_root(
    root: &mut TomlValue,
    record: &crate::custom_models::CustomModelRecord,
) -> Result<crate::agent::config::ConfigModelOverride> {
    upsert_config_model_table(root, &record.key, record.to_toml_table());
    if let Some(table) = root
        .get_mut("model")
        .and_then(|section| section.get_mut(&record.key))
        .and_then(TomlValue::as_table_mut)
    {
        if record.max_context_window.is_some() && record.context_window.is_none() {
            table.remove("context_window");
        } else if record.context_window.is_some() && record.max_context_window.is_none() {
            table.remove("max_context_window");
        }
    }
    let table = root
        .get("model")
        .and_then(|section| section.get(&record.key))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("failed to write [model.{}]", record.key))?;
    table
        .try_into()
        .map_err(|error| anyhow::anyhow!("failed to parse written [model.{}]: {error}", record.key))
}

/// Write one `[model.<key>]` table to `path` without going through
/// [`save_config_locked`].
pub(crate) fn persist_custom_model_upsert_at(
    path: &std::path::Path,
    record: &crate::custom_models::CustomModelRecord,
) -> Result<crate::agent::config::ConfigModelOverride> {
    let contents = read_to_string_or_empty(path)?;
    let mut root = parse_user_config_root(&contents, path)?;
    let model = persist_custom_model_upsert_to_root(&mut root, record)?;
    atomic_write_string(path, &toml::to_string_pretty(&root)?)?;
    Ok(model)
}

/// Write several `[model.<key>]` tables to `path` in one read-modify-write.
///
/// The wizard saves a whole endpoint's model list at once, so N rows must land
/// as one atomic change: a half-written list would leave the user with models
/// whose credentials were never stored.
pub(crate) fn persist_custom_model_upserts_at(
    path: &std::path::Path,
    records: &[crate::custom_models::CustomModelRecord],
) -> Result<Vec<(String, crate::agent::config::ConfigModelOverride)>> {
    let contents = read_to_string_or_empty(path)?;
    let mut root = parse_user_config_root(&contents, path)?;
    let mut written = Vec::with_capacity(records.len());
    for record in records {
        let model = persist_custom_model_upsert_to_root(&mut root, record)?;
        written.push((record.key.clone(), model));
    }
    atomic_write_string(path, &toml::to_string_pretty(&root)?)?;
    Ok(written)
}

/// Delete one `[model.<key>]` table at `path`.
pub(crate) fn persist_custom_model_delete_at(path: &std::path::Path, key: &str) -> Result<bool> {
    let contents = read_to_string_or_empty(path)?;
    let mut root = parse_user_config_root(&contents, path)?;
    let removed = delete_config_model_table(&mut root, key);
    atomic_write_string(path, &toml::to_string_pretty(&root)?)?;
    Ok(removed)
}
#[cfg(test)]
mod tests {
    #[test]
    fn codex_context_save_replaces_the_previous_override_coordinate() {
        let mut root: toml::Value =
            toml::from_str("[model.test]\ncontext_window = 200000\nname = 'keep me'\n").unwrap();
        let mut record = crate::custom_models::CustomModelRecord {
            key: "test".into(),
            model: "catalog-model".into(),
            provider: Some("codex".into()),
            max_context_window: Some(1_000_000),
            ..Default::default()
        };
        let parsed = super::persist_custom_model_upsert_to_root(&mut root, &record).unwrap();
        assert_eq!(parsed.context_window, None);
        assert_eq!(parsed.max_context_window, Some(1_000_000));
        assert_eq!(parsed.name.as_deref(), Some("keep me"));
        record.max_context_window = None;
        record.context_window = Some(800_000);
        let parsed = super::persist_custom_model_upsert_to_root(&mut root, &record).unwrap();
        assert_eq!(parsed.context_window, Some(800_000));
        assert_eq!(parsed.max_context_window, None);
    }
    use super::super::load::load_config_from_toml;
    use super::super::mcp::{McpConfig, parse_mcp_config_with_oauth};
    use super::super::settings_writes::{write_dashboard_preview, write_feature_override};
    use super::*;
    use toml::Value as TomlValue;
    use toml::map::Map as TomlMap;
    /// The `[toolset.ask_user_question]` settings write merges only that
    /// sub-table: the toggled field lands, hand-written sibling keys survive,
    /// and no other `[toolset]` defaults (bash/web_search) are splatted into
    /// the user file. All-None leaves the file untouched.
    #[test]
    fn ask_user_question_merge_writes_subtable_without_splatting_toolset() {
        let root_val: TomlValue =
            toml::from_str("[toolset.ask_user_question]\ntimeout_secs = 30\n").unwrap();
        let mut root = root_val.as_table().unwrap().clone();
        let ask = crate::tools::config::AskUserQuestionToolConfig {
            timeout_enabled: Some(false),
            ..Default::default()
        };
        merge_ask_user_question_section(&mut root, &ask);
        let toolset = root.get("toolset").and_then(|v| v.as_table()).unwrap();
        assert_eq!(toolset.len(), 1, "only ask_user_question may be written");
        let ask_tbl = toolset
            .get("ask_user_question")
            .and_then(|v| v.as_table())
            .unwrap();
        assert_eq!(
            ask_tbl.get("timeout_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            ask_tbl.get("timeout_secs").and_then(|v| v.as_integer()),
            Some(30),
            "hand-written sibling keys must survive the merge"
        );
        let reparsed = load_config_from_toml(&TomlValue::Table(root.clone()));
        assert_eq!(reparsed.ask_user_question.timeout_enabled, Some(false));
        assert_eq!(reparsed.ask_user_question.timeout_secs, Some(30));
        let mut empty_root: TomlMap<String, TomlValue> = TomlMap::new();
        merge_ask_user_question_section(
            &mut empty_root,
            &crate::tools::config::AskUserQuestionToolConfig::default(),
        );
        assert!(
            empty_root.is_empty(),
            "all-None must not create an empty [toolset] header"
        );
        let mut scalar_root: TomlMap<String, TomlValue> = TomlMap::new();
        scalar_root.insert("toolset".into(), TomlValue::String("bogus".into()));
        merge_ask_user_question_section(&mut scalar_root, &ask);
        assert_eq!(
            scalar_root
                .get("toolset")
                .and_then(|v| v.get("ask_user_question"))
                .and_then(|a| a.get("timeout_enabled"))
                .and_then(|v| v.as_bool()),
            Some(false),
            "scalar [toolset] must be replaced so the write lands"
        );
    }
    #[test]
    fn web_search_source_and_x_search_round_trip() {
        let root_val: TomlValue = toml::from_str(
            "[toolset.web_search_source]\ncodex = \"xai\"\nkimi_code = \"perplexity\"\n",
        )
        .unwrap();
        let config = crate::util::config::load_config_from_toml(&root_val);
        assert_eq!(
            config.web_search_source.codex,
            Some(crate::tools::config::WebSearchSource::Xai)
        );
        assert_eq!(
            config.web_search_source.kimi_code,
            Some(crate::tools::config::WebSearchSource::Perplexity)
        );
        assert_eq!(config.web_search_source.xai, None);
        assert!(config.x_search.enabled, "x_search defaults on");

        let mut root = root_val.as_table().unwrap().clone();
        let mut updated = config;
        updated.web_search_source.set_for(
            crate::tools::config::WebSearchSourceTarget::Xai,
            Some(crate::tools::config::WebSearchSource::Perplexity),
        );
        updated.x_search.enabled = false;
        merge_toolset_subsection(&mut root, "web_search_source", &updated.web_search_source);
        merge_toolset_subsection(&mut root, "x_search", &updated.x_search);
        let reparsed = crate::util::config::load_config_from_toml(&TomlValue::Table(root));
        assert_eq!(
            reparsed.web_search_source.xai,
            Some(crate::tools::config::WebSearchSource::Perplexity)
        );
        assert_eq!(
            reparsed.web_search_source.codex,
            Some(crate::tools::config::WebSearchSource::Xai),
            "existing selections survive a settings write"
        );
        assert!(!reparsed.x_search.enabled);
    }

    #[test]
    fn perplexity_merge_preserves_unrelated_toolset_entries() {
        let root_val: TomlValue =
            toml::from_str("[toolset.bash]\nenabled = false\n[unrelated]\nvalue = 7\n").unwrap();
        let mut root = root_val.as_table().unwrap().clone();
        merge_perplexity_web_search_section(
            &mut root,
            &crate::tools::config::PerplexityWebSearchToolConfig { enabled: true },
        );
        assert_eq!(
            root.get("toolset")
                .and_then(|value| value.get("bash"))
                .and_then(|value| value.get("enabled"))
                .and_then(TomlValue::as_bool),
            Some(false)
        );
        assert_eq!(
            root.get("toolset")
                .and_then(|value| value.get("perplexity_web_search"))
                .and_then(|value| value.get("enabled"))
                .and_then(TomlValue::as_bool),
            Some(true)
        );
        assert_eq!(
            root.get("unrelated")
                .and_then(|value| value.get("value"))
                .and_then(TomlValue::as_integer),
            Some(7)
        );
        let reparsed = load_config_from_toml(&TomlValue::Table(root));
        assert!(reparsed.perplexity_web_search.enabled);
    }

    #[test]
    fn transport_oauth_client_id_takes_priority_over_block() {
        let json = r#"{
            "mcpServers": {
                "svc": {
                    "type": "http",
                    "url": "https://svc.example/mcp",
                    "oauth_client_id": "transport-client",
                    "oauth": { "clientId": "block-client" }
                }
            }
        }"#;
        let config: McpConfig = serde_json::from_str(json).expect("parse .mcp.json");
        let svc = config.mcp_servers.get("svc").unwrap();
        let oauth = svc.oauth_config().expect("oauth_config");
        assert_eq!(oauth.client_id.as_deref(), Some("transport-client"));
    }
    #[test]
    fn parse_mcp_config_with_oauth_extracts_byo_client_id() {
        let json = r#"{
            "mcpServers": {
                "slack": {
                    "type": "http",
                    "url": "https://mcp.slack.example/mcp",
                    "oauth": { "clientId": "slack-byo-client" }
                },
                "plain": {
                    "type": "http",
                    "url": "https://plain.example/mcp"
                }
            }
        }"#;
        let config: McpConfig = serde_json::from_str(json).expect("parse .mcp.json");
        let (servers, oauth) = parse_mcp_config_with_oauth(&config, "test", &|s| s.to_string());
        assert_eq!(servers.len(), 2);
        assert_eq!(oauth.len(), 1);
        assert_eq!(
            oauth.get("slack").unwrap().client_id.as_deref(),
            Some("slack-byo-client")
        );
        assert!(!oauth.contains_key("plain"));
    }
    #[test]
    fn merge_section_preserves_unmodeled_fields() {
        let mut table = TomlMap::new();
        let mut ui = TomlMap::new();
        ui.insert("show_timestamps".into(), TomlValue::Boolean(true));
        ui.insert(
            "auto_dark_theme".into(),
            TomlValue::String("tokyonight".into()),
        );
        ui.insert("custom_user_key".into(), TomlValue::Integer(42));
        table.insert("ui".into(), TomlValue::Table(ui));
        let cfg = crate::agent::config::UiConfig::default();
        merge_section(&mut table, "ui", &cfg);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("show_timestamps").and_then(|v| v.as_bool()),
            Some(true),
            "pre-existing show_timestamps should survive merge with default struct"
        );
        assert_eq!(
            ui.get("auto_dark_theme").and_then(|v| v.as_str()),
            Some("tokyonight"),
            "pre-existing auto_dark_theme should survive merge with default struct"
        );
        assert_eq!(
            ui.get("custom_user_key").and_then(|v| v.as_integer()),
            Some(42),
            "truly unmodeled user-added key should survive merge"
        );
    }
    #[test]
    fn merge_section_nested_display_refresh_preserves_future_knob() {
        let mut table = TomlMap::new();
        let mut ui = TomlMap::new();
        let mut dr = TomlMap::new();
        dr.insert("probe_enabled".into(), TomlValue::Boolean(true));
        dr.insert("future_knob".into(), TomlValue::Integer(42));
        ui.insert("display_refresh".into(), TomlValue::Table(dr));
        table.insert("ui".into(), TomlValue::Table(ui));
        let mut cfg = crate::agent::config::UiConfig::default();
        cfg.display_refresh.probe_enabled = Some(false);
        merge_section(&mut table, "ui", &cfg);
        let nested = table
            .get("ui")
            .and_then(|v| v.as_table())
            .and_then(|u| u.get("display_refresh"))
            .and_then(|v| v.as_table())
            .expect("display_refresh table");
        assert_eq!(
            nested.get("probe_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            nested.get("future_knob").and_then(|v| v.as_integer()),
            Some(42),
            "unknown nested keys must survive shallow-looking settings writes"
        );
    }
    #[test]
    fn merge_section_updates_modeled_fields_preserving_unmodeled() {
        let mut table = TomlMap::new();
        let mut ui = TomlMap::new();
        ui.insert("yolo".into(), TomlValue::Boolean(false));
        ui.insert("show_timestamps".into(), TomlValue::Boolean(true));
        ui.insert(
            "auto_light_theme".into(),
            TomlValue::String("grokday".into()),
        );
        table.insert("ui".into(), TomlValue::Table(ui));
        let cfg = crate::agent::config::UiConfig {
            yolo: true,
            ..Default::default()
        };
        merge_section(&mut table, "ui", &cfg);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("yolo").and_then(|v| v.as_bool()),
            Some(true),
            "modeled field yolo should be updated"
        );
        assert_eq!(
            ui.get("show_timestamps").and_then(|v| v.as_bool()),
            Some(true),
            "pre-existing field not in serialized output should be preserved"
        );
        assert_eq!(
            ui.get("auto_light_theme").and_then(|v| v.as_str()),
            Some("grokday"),
            "pre-existing field not in serialized output should be preserved"
        );
    }
    #[test]
    fn merge_section_creates_new_section() {
        let mut table = TomlMap::new();
        assert!(table.get("ui").is_none());
        let cfg = crate::agent::config::UiConfig {
            yolo: true,
            ..Default::default()
        };
        merge_section(&mut table, "ui", &cfg);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(ui.get("yolo").and_then(|v| v.as_bool()), Some(true));
    }
    /// Regression test: pager-side commits of a
    /// [session] field (e.g., `auto_compact_threshold_percent`) must
    /// NOT inject `load_envrc` into the user's config when the user
    /// has never set it. Before the fix, `SessionConfig::load_envrc`
    /// was plain `bool` with default `true` and no
    /// `skip_serializing_if`, so EVERY pager save wrote
    /// `[session].load_envrc = true` to disk — silently overriding any
    /// managed-config `load_envrc = false` policy.
    ///
    /// The fix widens `load_envrc` to `Option<bool>` with
    /// `skip_serializing_if = "Option::is_none"`. After the fix, a
    /// `SessionConfig::default()` (load_envrc: None,
    /// auto_compact_threshold_percent: None) merges into a
    /// pre-existing `[session]` table WITHOUT touching the user's
    /// `load_envrc` key.
    #[test]
    fn merge_section_session_default_does_not_leak_load_envrc() {
        let mut table = TomlMap::new();
        assert!(table.get("session").is_none());
        let cfg = crate::agent::config::SessionConfig::default();
        merge_section(&mut table, "session", &cfg);
        if let Some(session) = table.get("session").and_then(|v| v.as_table()) {
            assert!(
                session.get("load_envrc").is_none(),
                "PR 12 R1 Bug 1: pager-side save with default SessionConfig must \
                 NOT serialize load_envrc — that would override managed-config \
                 policy. Found: {:?}",
                session.get("load_envrc"),
            );
            assert!(
                session.get("auto_compact_threshold_percent").is_none(),
                "default auto_compact_threshold_percent must not be serialized either"
            );
        }
    }
    /// Companion to the above: when the user explicitly commits a
    /// non-default `auto_compact_threshold_percent`, the field is
    /// serialized but `load_envrc` (still default None) is NOT.
    /// Pins the asymmetry — committing one [session] field does not
    /// "claim" the rest.
    #[test]
    fn merge_section_session_explicit_value_does_not_drag_load_envrc() {
        let mut table = TomlMap::new();
        let mut session = TomlMap::new();
        session.insert("load_envrc".into(), TomlValue::Boolean(false));
        table.insert("session".into(), TomlValue::Table(session));
        let cfg = crate::agent::config::SessionConfig {
            auto_compact_threshold_percent: Some(70),
            load_envrc: None,
        };
        merge_section(&mut table, "session", &cfg);
        let session = table.get("session").unwrap().as_table().unwrap();
        assert_eq!(
            session
                .get("auto_compact_threshold_percent")
                .and_then(|v| v.as_integer()),
            Some(70),
        );
        assert_eq!(
            session.get("load_envrc").and_then(|v| v.as_bool()),
            Some(false),
            "pre-existing load_envrc must survive a partial settings save"
        );
    }
    /// Follow-on: when the user DOES explicitly set
    /// `load_envrc = false` via TOML, the value round-trips through
    /// `load_config_from_toml` → mutate → `merge_section` correctly.
    /// `None` means "absent on disk"; `Some(false)` means "user
    /// explicitly disabled". The distinction must survive a save.
    #[test]
    fn session_load_envrc_explicit_false_round_trips() {
        let raw_config: TomlValue = toml::from_str(
            r#"
            [session]
            load_envrc = false
            "#,
        )
        .unwrap();
        let cfg = load_config_from_toml(&raw_config);
        assert_eq!(
            cfg.session.load_envrc,
            Some(false),
            "explicit load_envrc = false on disk must load as Some(false), not None"
        );
        let mut table = TomlMap::new();
        merge_section(&mut table, "session", &cfg.session);
        let session = table.get("session").unwrap().as_table().unwrap();
        assert_eq!(
            session.get("load_envrc").and_then(|v| v.as_bool()),
            Some(false),
            "explicit load_envrc = false must survive a save"
        );
    }
    #[test]
    fn merge_section_empty_struct_preserves_existing_section() {
        let mut table = TomlMap::new();
        let mut harness = TomlMap::new();
        harness.insert("custom_key".into(), TomlValue::Boolean(true));
        harness.insert("another_key".into(), TomlValue::String("value".into()));
        table.insert("harness".into(), TomlValue::Table(harness));
        let cfg = crate::agent::config::HarnessConfig::default();
        merge_section(&mut table, "harness", &cfg);
        let harness = table.get("harness").unwrap().as_table().unwrap();
        assert_eq!(
            harness.get("custom_key").and_then(|v| v.as_bool()),
            Some(true),
            "existing fields must survive when struct serializes empty"
        );
        assert_eq!(
            harness.get("another_key").and_then(|v| v.as_str()),
            Some("value"),
        );
    }
    #[test]
    fn dashboard_preview_writer_accepts_loader_syntax_and_preserves_other_fields() {
        for input in [
            "",
            "ui = { dashboard_preview = true, custom = 42, }",
            r#"
ui = {
    dashboard_preview = true,
    custom = 42,
}
"#,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.toml");
            std::fs::write(&path, input).unwrap();
            for value in [false, true] {
                write_dashboard_preview(&path, value, atomic_write_follow_bound).unwrap();
                let saved: toml::Value =
                    toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                assert_eq!(
                    saved
                        .get("ui")
                        .and_then(|ui| ui.get("dashboard_preview"))
                        .and_then(toml::Value::as_bool),
                    Some(value)
                );
                if !input.is_empty() {
                    assert_eq!(
                        saved
                            .get("ui")
                            .and_then(|ui| ui.get("custom"))
                            .and_then(toml::Value::as_integer),
                        Some(42)
                    );
                }
            }
        }
    }
    #[test]
    fn dashboard_preview_writer_preserves_invalid_files_and_identifies_the_error() {
        for (input, operation) in [("[ui", "parse"), ("ui = false", "[ui] must be a table")] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.toml");
            std::fs::write(&path, input).unwrap();
            let error = write_dashboard_preview(&path, false, atomic_write_follow_bound)
                .unwrap_err()
                .to_string();
            assert!(error.contains(operation), "{error}");
            assert!(error.contains(path.to_str().unwrap()), "{error}");
            if operation == "parse" {
                assert!(error.contains("line 1, column"), "{error}");
            }
            assert_eq!(std::fs::read_to_string(&path).unwrap(), input);
        }
    }
    #[test]
    fn dashboard_preview_writer_read_error_names_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();
        let cause = read_follow_bound(&path).unwrap_err().to_string();
        let error = write_dashboard_preview(&path, false, atomic_write_follow_bound)
            .unwrap_err()
            .to_string();
        assert!(error.contains("read"), "{error}");
        assert!(error.contains(path.to_str().unwrap()), "{error}");
        assert!(error.contains(&cause), "{error}");
        assert!(path.is_dir());
    }
    #[test]
    fn dashboard_preview_write_failure_preserves_existing_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let input = "ui = { dashboard_preview = true }";
        std::fs::write(&path, input).unwrap();
        let error = write_dashboard_preview(&path, false, |destination, _bound, content| {
            assert_eq!(destination, path);
            let parsed: toml::Value = toml::from_str(content).unwrap();
            assert_eq!(
                parsed
                    .get("ui")
                    .and_then(|ui| ui.get("dashboard_preview"))
                    .and_then(toml::Value::as_bool),
                Some(false)
            );
            Err(std::io::Error::other("disk full"))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("write"), "{error}");
        assert!(error.contains(path.to_str().unwrap()), "{error}");
        assert!(error.contains("disk full"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), input);
    }
    #[cfg(unix)]
    #[test]
    fn dashboard_preview_writer_refuses_a_retargeted_symlink() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.toml");
        let second = directory.path().join("second.toml");
        let slot = directory.path().join("config.toml");
        let first_content = "ui = { dashboard_preview = true, custom = 1 }";
        let second_content = "ui = { dashboard_preview = true, custom = 2 }";
        std::fs::write(&first, first_content).unwrap();
        std::fs::write(&second, second_content).unwrap();
        symlink(&first, &slot).unwrap();
        let error = write_dashboard_preview(&slot, false, |slot, bound, content| {
            std::fs::remove_file(slot).unwrap();
            symlink(&second, slot).unwrap();
            atomic_write_follow_bound(slot, bound, content)
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("changed"), "{error}");
        assert_eq!(first_content, std::fs::read_to_string(&first).unwrap());
        assert_eq!(second_content, std::fs::read_to_string(&second).unwrap());
        assert_eq!(second, std::fs::read_link(&slot).unwrap());
    }
    /// `Some` inserts or replaces the key, `None` deletes it (and an emptied `[features]` table), and sibling keys survive every step.
    #[test]
    fn feature_override_writer_inserts_replaces_and_removes_the_key() {
        use xai_grok_config_types::Feature;
        let feature = Feature::SubagentModelInheritance;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "features = { web_fetch = true }\n").unwrap();
        let read_features = |path: &std::path::Path| -> Option<TomlValue> {
            toml::from_str::<TomlValue>(&std::fs::read_to_string(path).unwrap())
                .unwrap()
                .get("features")
                .cloned()
        };
        for value in [Some(true), Some(false), None] {
            write_feature_override(&path, feature, value, atomic_write_follow_bound).unwrap();
            let features = read_features(&path).expect("sibling key keeps the table");
            assert_eq!(
                value,
                features.get(feature.key()).and_then(TomlValue::as_bool)
            );
            assert_eq!(
                Some(true),
                features.get("web_fetch").and_then(TomlValue::as_bool)
            );
        }
        std::fs::write(&path, "").unwrap();
        write_feature_override(&path, feature, Some(true), atomic_write_follow_bound).unwrap();
        assert_eq!(
            Some(true),
            read_features(&path).and_then(|f| f.get(feature.key()).and_then(TomlValue::as_bool))
        );
        write_feature_override(&path, feature, None, atomic_write_follow_bound).unwrap();
        assert_eq!(
            None,
            read_features(&path),
            "an emptied [features] table is dropped"
        );
    }
    #[test]
    fn unrelated_ui_write_preserves_managed_dashboard_preview_default() {
        let mut managed: TomlValue = toml::from_str(
            r#"
[ui]
dashboard_preview = false
"#,
        )
        .unwrap();
        let mut user = TomlMap::new();
        let mut config = load_config_from_toml(&TomlValue::Table(user.clone()));
        assert!(config.ui.dashboard_preview.is_none());
        config.ui.show_timestamps = Some(false);
        merge_section(&mut user, "ui", &config.ui);
        assert!(
            user.get("ui")
                .and_then(|ui| ui.get("dashboard_preview"))
                .is_none()
        );
        merge_toml_tables(managed.as_table_mut().unwrap(), user);
        let effective = load_config_from_toml(&managed);
        assert!(!effective.ui.dashboard_preview_enabled());
        assert_eq!(effective.ui.show_timestamps, Some(false));
    }
    #[test]
    fn ui_config_round_trip_preserves_pager_fields() {
        let toml_str = r#"
[ui]
yolo = true
show_timestamps = false
dashboard_preview = false
auto_dark_theme = "tokyonight"
auto_light_theme = "grokday"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        let cfg = load_config_from_toml(&root);
        assert!(cfg.ui.yolo);
        assert_eq!(cfg.ui.show_timestamps, Some(false));
        assert!(!cfg.ui.dashboard_preview_enabled());
        assert_eq!(cfg.ui.auto_dark_theme.as_deref(), Some("tokyonight"));
        assert_eq!(cfg.ui.auto_light_theme.as_deref(), Some("grokday"));
        let mut table = root.as_table().unwrap().clone();
        merge_section(&mut table, "ui", &cfg.ui);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("show_timestamps").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            ui.get("auto_dark_theme").and_then(|v| v.as_str()),
            Some("tokyonight")
        );
        assert_eq!(
            ui.get("auto_light_theme").and_then(|v| v.as_str()),
            Some("grokday")
        );
        assert_eq!(ui.get("yolo").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            ui.get("dashboard_preview").and_then(|v| v.as_bool()),
            Some(false)
        );
    }
    #[test]
    fn ui_config_hunk_tracker_mode_round_trips() {
        let root: TomlValue = toml::from_str("[ui]\nhunk_tracker_mode = \"off\"\n").unwrap();
        let cfg = load_config_from_toml(&root);
        assert_eq!(cfg.ui.hunk_tracker_mode.as_deref(), Some("off"));
        let mut table = root.as_table().unwrap().clone();
        merge_section(&mut table, "ui", &cfg.ui);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("hunk_tracker_mode").and_then(|v| v.as_str()),
            Some("off")
        );
        let serialized = TomlValue::try_from(crate::agent::config::UiConfig::default()).unwrap();
        assert!(
            serialized
                .as_table()
                .unwrap()
                .get("hunk_tracker_mode")
                .is_none(),
            "hunk_tracker_mode=None must not appear in serialized output"
        );
    }
    #[test]
    fn ui_config_serialization_behavior() {
        let cfg = crate::agent::config::UiConfig::default();
        let val = TomlValue::try_from(&cfg).unwrap();
        let table = val.as_table().unwrap();
        assert!(
            table.get("yolo").is_some(),
            "yolo must always serialize so revert-to-default persists"
        );
        assert!(
            table.get("compact_mode").is_some(),
            "compact_mode must always serialize so revert-to-default persists"
        );
        assert!(
            table.get("max_thoughts_width").is_some(),
            "max_thoughts_width must always serialize so revert-to-default persists"
        );
        assert!(
            table.get("show_timestamps").is_none(),
            "show_timestamps=None should not appear in serialized output"
        );
        assert!(
            table.get("auto_dark_theme").is_none(),
            "auto_dark_theme=None should not appear in serialized output"
        );
        assert!(
            table.get("theme").is_none(),
            "theme=None should not appear in serialized output"
        );
    }
    /// The settings-modal helpers in the parent module are 3-line
    /// wrappers around `update_config(|cfg| cfg.ui.<field> = ...)`. To
    /// guard against future drift between the wrapper and the schema
    /// field, this test simulates each helper's closure against an
    /// in-memory `Config` and asserts the field was set correctly. We
    /// deliberately avoid disk I/O so the test stays hermetic.
    ///
    /// The pattern mirrors exactly what `update_config` does internally:
    /// `let mut cfg = load_config_from_toml(...); f(&mut cfg);`.
    #[test]
    fn merge_section_full_save_config_simulation() {
        let original = r#"
[ui]
show_timestamps = true
auto_dark_theme = "tokyonight"
auto_light_theme = "grokday"

[models]
default = "grok-3"

[cli]
auto_update = true
"#;
        let root: TomlValue = toml::from_str(original).unwrap();
        let mut cfg = load_config_from_toml(&root);
        cfg.models.default = Some("grok-4".to_string());
        let mut table = root.as_table().unwrap().clone();
        merge_section(&mut table, "cli", &cfg.cli);
        merge_section(&mut table, "models", &cfg.models);
        merge_section(&mut table, "ui", &cfg.ui);
        merge_section(&mut table, "harness", &cfg.harness);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("show_timestamps").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            ui.get("auto_dark_theme").and_then(|v| v.as_str()),
            Some("tokyonight")
        );
        assert_eq!(
            ui.get("auto_light_theme").and_then(|v| v.as_str()),
            Some("grokday")
        );
        let models = table.get("models").unwrap().as_table().unwrap();
        assert_eq!(
            models.get("default").and_then(|v| v.as_str()),
            Some("grok-4")
        );
    }
    #[test]
    fn merge_section_revert_to_default_overwrites_old_value() {
        let mut table = TomlMap::new();
        let mut ui = TomlMap::new();
        ui.insert("yolo".into(), TomlValue::Boolean(true));
        ui.insert("compact_mode".into(), TomlValue::Boolean(true));
        table.insert("ui".into(), TomlValue::Table(ui));
        let cfg = crate::agent::config::UiConfig::default();
        merge_section(&mut table, "ui", &cfg);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("yolo").and_then(|v| v.as_bool()),
            Some(false),
            "yolo=false must overwrite the old yolo=true"
        );
        assert_eq!(
            ui.get("compact_mode").and_then(|v| v.as_bool()),
            Some(false),
            "compact_mode=false must overwrite the old compact_mode=true"
        );
    }
    #[test]
    fn merge_section_replaces_non_table_section() {
        let mut table = TomlMap::new();
        table.insert("ui".into(), TomlValue::String("garbage".into()));
        let cfg = crate::agent::config::UiConfig {
            yolo: true,
            ..Default::default()
        };
        merge_section(&mut table, "ui", &cfg);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("yolo").and_then(|v| v.as_bool()),
            Some(true),
            "non-table section should be replaced with proper table"
        );
    }
    #[test]
    fn models_config_serializes_only_some_fields() {
        let m = crate::agent::config::ModelsConfig {
            default: Some("grok-3".to_string()),
            ..Default::default()
        };
        let v = TomlValue::try_from(&m).expect("serialize ModelsConfig");
        if let TomlValue::Table(t) = v {
            assert_eq!(t.len(), 2);
            assert!(t.contains_key("default"));
            assert_eq!(
                t.get("kimi_endpoint").and_then(TomlValue::as_str),
                Some("platform")
            );
            assert!(!t.contains_key("web_search"));
            assert!(!t.contains_key("session_summary"));
            assert!(!t.contains_key("image_description"));
            assert!(!t.contains_key("hidden_models"));
            assert!(!t.contains_key("disabled_models"));
            assert!(!t.contains_key("allowed_models"));
            assert!(!t.contains_key("agent_type"));
            assert_eq!(t.get("default").and_then(|x| x.as_str()), Some("grok-3"));
        } else {
            panic!("expected table from serialization");
        }
    }
    /// Canonical list of every `Option<T>` field in [`CliConfig`].  Kept in one
    /// place so both serialization and merge-section tests automatically cover
    /// newly-added fields without copy-pasting assertion lists.
    const CLI_CONFIG_OPTION_FIELDS: &[&str] = &[
        "auto_update",
        "dismissed_version",
        "installer",
        "npm_registry",
        "channel",
        "use_leader",
        "show_tips",
        "worktree_type",
        "session_registry",
        "minimum_version",
        "maximum_version",
        "required_minimum_version",
        "required_maximum_version",
    ];
    /// Assert that every `CliConfig` `Option<T>` field NOT in `present` is
    /// absent from `table`.
    fn assert_cli_option_fields_absent(table: &TomlMap<String, TomlValue>, present: &[&str]) {
        for field in CLI_CONFIG_OPTION_FIELDS {
            if !present.contains(field) {
                assert!(
                    !table.contains_key(*field),
                    "expected Option field `{field}` to be absent when not set",
                );
            }
        }
    }
    #[test]
    fn cli_config_serializes_only_some_fields() {
        let c = crate::agent::config::CliConfig {
            auto_update: Some(true),
            channel: Some("beta".to_string()),
            ..Default::default()
        };
        let v = TomlValue::try_from(&c).expect("serialize CliConfig");
        if let TomlValue::Table(t) = v {
            assert_eq!(t.len(), 2);
            assert!(t.contains_key("auto_update"));
            assert!(t.contains_key("channel"));
            assert_cli_option_fields_absent(&t, &["auto_update", "channel"]);
            assert_eq!(t.get("auto_update").and_then(|x| x.as_bool()), Some(true));
            assert_eq!(t.get("channel").and_then(|x| x.as_str()), Some("beta"));
        } else {
            panic!("expected table from serialization");
        }
    }
    #[test]
    fn merge_section_cli_only_updates_set_fields_preserves_unmodeled() {
        let mut table = TomlMap::new();
        let mut cli = TomlMap::new();
        cli.insert("use_leader".into(), TomlValue::Boolean(true));
        cli.insert("show_tips".into(), TomlValue::Boolean(false));
        cli.insert(
            "custom_pager_key".into(),
            TomlValue::String("keep-this".into()),
        );
        table.insert("cli".into(), TomlValue::Table(cli));
        let cfg = crate::agent::config::CliConfig {
            auto_update: Some(false),
            dismissed_version: Some("v1.2.3".to_string()),
            ..Default::default()
        };
        merge_section(&mut table, "cli", &cfg);
        let c = table.get("cli").unwrap().as_table().unwrap();
        assert_eq!(c.get("auto_update").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            c.get("dismissed_version").and_then(|v| v.as_str()),
            Some("v1.2.3")
        );
        assert_eq!(c.get("use_leader").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(c.get("show_tips").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            c.get("custom_pager_key").and_then(|v| v.as_str()),
            Some("keep-this")
        );
        assert_cli_option_fields_absent(
            c,
            &[
                "auto_update",
                "dismissed_version",
                "use_leader",
                "show_tips",
            ],
        );
    }
    #[test]
    fn merge_section_models_only_updates_set_fields_preserves_others() {
        let mut table = TomlMap::new();
        let mut models = TomlMap::new();
        models.insert("web_search".into(), TomlValue::String("old-search".into()));
        models.insert("unmodeled_foo".into(), TomlValue::String("keep-me".into()));
        table.insert("models".into(), TomlValue::Table(models));
        let cfg = crate::agent::config::ModelsConfig {
            default: Some("grok-new".to_string()),
            ..Default::default()
        };
        merge_section(&mut table, "models", &cfg);
        let m = table.get("models").unwrap().as_table().unwrap();
        assert_eq!(m.get("default").and_then(|v| v.as_str()), Some("grok-new"));
        assert_eq!(
            m.get("web_search").and_then(|v| v.as_str()),
            Some("old-search")
        );
        assert_eq!(
            m.get("unmodeled_foo").and_then(|v| v.as_str()),
            Some("keep-me")
        );
        assert!(!m.contains_key("session_summary"));
    }
    #[test]
    fn persist_preferred_model_flow_roundtrips_via_load_and_new_from_toml_cfg() {
        let original = "[models]\ndefault = \"grok-old\"\nweb_search = \"some-search\"\n";
        let root: TomlValue = toml::from_str(original).unwrap();
        let mut cfg = load_config_from_toml(&root);
        cfg.models.default = Some("grok-persisted".to_string());
        let mut table = if let TomlValue::Table(t) = root {
            t
        } else {
            TomlMap::new()
        };
        merge_section(&mut table, "models", &cfg.models);
        let reloaded_root = TomlValue::Table(table);
        let reloaded = load_config_from_toml(&reloaded_root);
        assert_eq!(reloaded.models.default.as_deref(), Some("grok-persisted"));
        let cfg2 = crate::agent::config::Config::new_from_toml_cfg(&reloaded_root)
            .expect("new_from_toml_cfg");
        assert_eq!(cfg2.models.default.as_deref(), Some("grok-persisted"));
    }
    #[test]
    fn merge_section_cli_show_tips_writes_under_cli_section() {
        let mut table = TomlMap::new();
        let cfg = crate::agent::config::CliConfig {
            show_tips: Some(false),
            ..Default::default()
        };
        merge_section(&mut table, "cli", &cfg);
        let c = table.get("cli").unwrap().as_table().unwrap();
        assert_eq!(
            c.get("show_tips").and_then(|v| v.as_bool()),
            Some(false),
            "set_show_tips must persist Some(false) at `[cli].show_tips`"
        );
    }
    #[test]
    fn merge_section_cli_show_tips_none_does_not_serialize() {
        let mut table = TomlMap::new();
        let cfg = crate::agent::config::CliConfig::default();
        assert!(cfg.show_tips.is_none());
        merge_section(&mut table, "cli", &cfg);
        if let Some(c) = table.get("cli").and_then(|v| v.as_table()) {
            assert!(
                c.get("show_tips").is_none(),
                "default show_tips: None must not serialize — \
                 managed-config layering depends on absent-means-defer"
            );
        }
    }
    #[test]
    fn merge_section_cli_session_picker_grouped_writes_under_cli_section() {
        let mut table = TomlMap::new();
        let cfg = crate::agent::config::CliConfig {
            session_picker_grouped: Some(false),
            ..Default::default()
        };
        merge_section(&mut table, "cli", &cfg);
        let c = table.get("cli").unwrap().as_table().unwrap();
        assert_eq!(
            c.get("session_picker_grouped").and_then(|v| v.as_bool()),
            Some(false),
            "Some(false) must round-trip to `[cli].session_picker_grouped`"
        );
    }
    #[test]
    fn merge_section_cli_auto_update_writes_under_cli_section() {
        let mut table = TomlMap::new();
        let cfg = crate::agent::config::CliConfig {
            auto_update: Some(false),
            ..Default::default()
        };
        merge_section(&mut table, "cli", &cfg);
        let c = table.get("cli").unwrap().as_table().unwrap();
        assert_eq!(
            c.get("auto_update").and_then(|v| v.as_bool()),
            Some(false),
            "set_auto_update must persist Some(false) at `[cli].auto_update`"
        );
    }
    #[test]
    fn merge_section_cli_use_leader_writes_under_cli_section() {
        let mut table = TomlMap::new();
        let cfg = crate::agent::config::CliConfig {
            use_leader: Some(true),
            ..Default::default()
        };
        merge_section(&mut table, "cli", &cfg);
        let c = table.get("cli").unwrap().as_table().unwrap();
        assert_eq!(
            c.get("use_leader").and_then(|v| v.as_bool()),
            Some(true),
            "Some(true) must round-trip to `[cli].use_leader`"
        );
    }
    /// Verify `Option<bool>` + `skip_serializing_if` prevents one
    /// `[session]` field from dragging unrelated fields.
    #[test]
    fn merge_section_session_load_envrc_writes_under_session_section() {
        let mut table = TomlMap::new();
        let cfg = crate::agent::config::SessionConfig {
            load_envrc: Some(false),
            ..Default::default()
        };
        merge_section(&mut table, "session", &cfg);
        let s = table.get("session").unwrap().as_table().unwrap();
        assert_eq!(
            s.get("load_envrc").and_then(|v| v.as_bool()),
            Some(false),
            "Some(false) must round-trip to `[session].load_envrc`"
        );
    }
    /// Committing `load_envrc` alone must not inject `auto_compact_threshold_percent`.
    #[test]
    fn merge_section_session_load_envrc_does_not_drag_auto_compact() {
        let mut table = TomlMap::new();
        let cfg = crate::agent::config::SessionConfig {
            load_envrc: Some(true),
            auto_compact_threshold_percent: None,
        };
        merge_section(&mut table, "session", &cfg);
        let s = table.get("session").unwrap().as_table().unwrap();
        assert_eq!(s.get("load_envrc").and_then(|v| v.as_bool()), Some(true));
        assert!(
            s.get("auto_compact_threshold_percent").is_none(),
            "default auto_compact_threshold_percent: None must not serialize \
             when only load_envrc is being committed"
        );
    }
    mod resolve_auto_compact {
        use super::super::super::RemoteSettings;
        use super::super::super::resolve::{
            DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT, ENV_AUTO_COMPACT_THRESHOLD_PERCENT,
            resolve_auto_compact_threshold_percent,
        };
        use crate::agent::config::{Config, ConfigModelOverride, ModelInfo};
        use std::sync::Mutex;
        const TEST_MODEL: &str = "grok-4.5";
        const OTHER_MODEL: &str = "grok-4.3";
        /// Serialize tests that mutate `GROK_AUTO_COMPACT_THRESHOLD_PERCENT`.
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        /// Build a `Config` populated with optional per-source values for the
        /// `TEST_MODEL`. Any `None` argument means "that source is unset".
        fn make_cfg(
            user_session: Option<u8>,
            user_per_model: Option<u8>,
            gb_global: Option<u8>,
        ) -> Config {
            let mut cfg = Config::default();
            cfg.session.auto_compact_threshold_percent = user_session;
            if let Some(v) = user_per_model {
                cfg.config_models.insert(
                    TEST_MODEL.to_owned(),
                    ConfigModelOverride {
                        auto_compact_threshold_percent: Some(v),
                        ..ConfigModelOverride::default()
                    },
                );
            }
            if let Some(v) = gb_global {
                cfg.remote_settings = Some(RemoteSettings {
                    auto_compact_threshold_percent: Some(v),
                    ..RemoteSettings::default()
                });
            }
            cfg
        }
        /// ModelInfo populated with the GB per-model value (or none).
        fn model_info(gb_per_model: Option<u8>) -> ModelInfo {
            let mut info = ModelInfo::fallback(TEST_MODEL);
            info.auto_compact_threshold_percent = gb_per_model;
            info
        }
        /// Run the resolver against the assembled inputs.
        fn resolve(cfg: &Config, gb_per_model: Option<u8>) -> u8 {
            let info = model_info(gb_per_model);
            resolve_auto_compact_threshold_percent(cfg, TEST_MODEL, Some(&info))
        }
        /// RAII guard that swaps the env var for the duration of a test and
        /// restores the previous value on drop. Acquires `ENV_LOCK` so two
        /// env-var tests never run concurrently.
        struct EnvVarGuard {
            _lock: std::sync::MutexGuard<'static, ()>,
            prev: Option<String>,
        }
        impl EnvVarGuard {
            fn set(value: &str) -> Self {
                let lock = ENV_LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let prev = std::env::var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT).ok();
                unsafe { std::env::set_var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT, value) };
                Self { _lock: lock, prev }
            }
            fn unset() -> Self {
                let lock = ENV_LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let prev = std::env::var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT).ok();
                unsafe { std::env::remove_var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT) };
                Self { _lock: lock, prev }
            }
        }
        impl Drop for EnvVarGuard {
            fn drop(&mut self) {
                match self.prev.take() {
                    Some(v) => unsafe { std::env::set_var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT, v) },
                    None => unsafe { std::env::remove_var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT) },
                }
            }
        }
        #[test]
        fn all_unset_returns_default_85() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(None, None, None);
            assert_eq!(resolve(&cfg, None), DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT);
        }
        #[test]
        fn all_unset_no_model_info_returns_default_85() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(None, None, None);
            assert_eq!(
                resolve_auto_compact_threshold_percent(&cfg, TEST_MODEL, None),
                DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT
            );
        }
        #[test]
        fn gb_global_only() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(None, None, Some(40));
            assert_eq!(resolve(&cfg, None), 40);
        }
        #[test]
        fn gb_per_model_beats_gb_global() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(None, None, Some(40));
            assert_eq!(resolve(&cfg, Some(90)), 90);
        }
        #[test]
        fn user_session_beats_gb_per_model() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(Some(75), None, None);
            assert_eq!(resolve(&cfg, Some(90)), 75);
        }
        #[test]
        fn user_session_beats_gb_global() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(Some(75), None, Some(40));
            assert_eq!(resolve(&cfg, None), 75);
        }
        #[test]
        fn user_per_model_beats_user_session() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(Some(75), Some(70), None);
            assert_eq!(resolve(&cfg, None), 70);
        }
        #[test]
        fn user_per_model_beats_gb_per_model() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(None, Some(70), None);
            assert_eq!(resolve(&cfg, Some(90)), 70);
        }
        #[test]
        fn user_per_model_beats_gb_global() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(None, Some(70), Some(40));
            assert_eq!(resolve(&cfg, None), 70);
        }
        #[test]
        fn user_per_model_beats_everything_below_env() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(Some(75), Some(70), Some(40));
            assert_eq!(resolve(&cfg, Some(90)), 70);
        }
        #[test]
        fn env_beats_user_per_model() {
            let _g = EnvVarGuard::set("50");
            let cfg = make_cfg(Some(75), Some(70), Some(40));
            assert_eq!(resolve(&cfg, Some(90)), 50);
        }
        #[test]
        fn env_at_lower_bound_is_honored() {
            let _g = EnvVarGuard::set("0");
            let cfg = make_cfg(Some(75), None, None);
            assert_eq!(resolve(&cfg, None), 0);
        }
        #[test]
        fn env_at_upper_bound_is_honored() {
            let _g = EnvVarGuard::set("100");
            let cfg = make_cfg(Some(75), None, None);
            assert_eq!(resolve(&cfg, None), 100);
        }
        #[test]
        fn env_out_of_range_high_falls_through() {
            let _g = EnvVarGuard::set("101");
            let cfg = make_cfg(Some(75), None, None);
            assert_eq!(resolve(&cfg, None), 75);
        }
        #[test]
        fn env_out_of_range_negative_falls_through() {
            let _g = EnvVarGuard::set("-1");
            let cfg = make_cfg(Some(75), None, None);
            assert_eq!(resolve(&cfg, None), 75);
        }
        #[test]
        fn env_unparseable_falls_through() {
            let _g = EnvVarGuard::set("not-a-number");
            let cfg = make_cfg(Some(75), None, None);
            assert_eq!(resolve(&cfg, None), 75);
        }
        #[test]
        fn env_empty_falls_through_to_default() {
            let _g = EnvVarGuard::set("");
            let cfg = make_cfg(None, None, None);
            assert_eq!(resolve(&cfg, None), DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT);
        }
        #[test]
        fn user_per_model_for_other_model_does_not_match() {
            let _g = EnvVarGuard::unset();
            let mut cfg = Config::default();
            cfg.session.auto_compact_threshold_percent = None;
            cfg.config_models.insert(
                OTHER_MODEL.to_owned(),
                ConfigModelOverride {
                    auto_compact_threshold_percent: Some(70),
                    ..ConfigModelOverride::default()
                },
            );
            assert_eq!(resolve(&cfg, None), DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT);
        }
        #[test]
        fn user_per_model_for_other_model_falls_through_to_user_session() {
            let _g = EnvVarGuard::unset();
            let mut cfg = Config::default();
            cfg.session.auto_compact_threshold_percent = Some(75);
            cfg.config_models.insert(
                OTHER_MODEL.to_owned(),
                ConfigModelOverride {
                    auto_compact_threshold_percent: Some(70),
                    ..ConfigModelOverride::default()
                },
            );
            assert_eq!(resolve(&cfg, None), 75);
        }
        #[test]
        fn missing_model_info_falls_through_to_gb_global() {
            let _g = EnvVarGuard::unset();
            let cfg = make_cfg(None, None, Some(40));
            assert_eq!(
                resolve_auto_compact_threshold_percent(&cfg, TEST_MODEL, None),
                40
            );
        }
        #[test]
        fn no_remote_settings_falls_through_to_default() {
            let _g = EnvVarGuard::unset();
            let cfg = Config {
                remote_settings: None,
                ..Config::default()
            };
            assert_eq!(resolve(&cfg, None), DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT);
        }
        #[test]
        fn apply_does_not_merge_auto_compact_threshold_percent_into_model_info() {
            use crate::agent::config::{EndpointsConfig, ModelEntry};
            let endpoints = EndpointsConfig::default();
            let base = ModelEntry::fallback(TEST_MODEL, &endpoints);
            let over = ConfigModelOverride {
                auto_compact_threshold_percent: Some(42),
                ..ConfigModelOverride::default()
            };
            let merged = over.apply(TEST_MODEL, Some(base), &endpoints);
            assert_eq!(
                merged.info.auto_compact_threshold_percent, None,
                "ConfigModelOverride::apply must NOT merge `auto_compact_threshold_percent` \
                 into ModelInfo — the resolver depends on the field staying empty so \
                 user-per-model and GB-per-model remain distinguishable tiers"
            );
        }
    }
    #[test]
    fn settings_helpers_target_correct_ui_fields() {
        fn apply<F: FnOnce(&mut Config)>(f: F) -> Config {
            let mut cfg = load_config_from_toml(&TomlValue::Table(TomlMap::new()));
            f(&mut cfg);
            cfg
        }
        let cfg = apply(|cfg| cfg.ui.compact_mode = true);
        assert!(cfg.ui.compact_mode, "set_compact_mode must set bool field");
        let cfg = apply(|cfg| cfg.ui.compact_mode = false);
        assert!(!cfg.ui.compact_mode);
        let cfg = apply(|cfg| cfg.ui.show_timestamps = Some(true));
        assert_eq!(cfg.ui.show_timestamps, Some(true));
        let cfg = apply(|cfg| cfg.ui.show_timestamps = Some(false));
        assert_eq!(cfg.ui.show_timestamps, Some(false));
        let cfg = apply(|cfg| cfg.ui.simple_mode = Some(true));
        assert_eq!(cfg.ui.simple_mode, Some(true));
        let cfg = apply(|cfg| cfg.ui.simple_mode = Some(false));
        assert_eq!(cfg.ui.simple_mode, Some(false));

        // set_code_mode wraps `cfg.ui.code_mode = Some(value)`.
        let cfg = apply(|cfg| {
            cfg.ui.code_mode = Some(crate::agent::config::ToolModePreference::CodeMode)
        });
        assert_eq!(
            cfg.ui.code_mode,
            Some(crate::agent::config::ToolModePreference::CodeMode)
        );
        let cfg =
            apply(|cfg| cfg.ui.code_mode = Some(crate::agent::config::ToolModePreference::Direct));
        assert_eq!(
            cfg.ui.code_mode,
            Some(crate::agent::config::ToolModePreference::Direct)
        );

        // set_theme wraps `cfg.ui.theme = Some(value)` (canonical name).
        let cfg = apply(|cfg| cfg.ui.theme = Some("tokyonight".to_string()));
        assert_eq!(cfg.ui.theme, Some("tokyonight".to_string()));
        let cfg = apply(|cfg| cfg.ui.theme = Some("auto".to_string()));
        assert_eq!(cfg.ui.theme, Some("auto".to_string()));
        let cfg = apply(|cfg| cfg.ui.auto_dark_theme = Some("tokyonight".to_string()));
        assert_eq!(cfg.ui.auto_dark_theme, Some("tokyonight".to_string()));
        let cfg = apply(|cfg| cfg.ui.auto_light_theme = Some("grokday".to_string()));
        assert_eq!(cfg.ui.auto_light_theme, Some("grokday".to_string()));
        let cfg = apply(|cfg| cfg.ui.hunk_tracker_mode = Some("off".to_string()));
        assert_eq!(cfg.ui.hunk_tracker_mode, Some("off".to_string()));
        let cfg = apply(|cfg| cfg.ui.stream_tool_calls = Some(true));
        assert_eq!(cfg.ui.stream_tool_calls, Some(true));
        let cfg = apply(|cfg| cfg.ui.stream_tool_calls = Some(false));
        assert_eq!(cfg.ui.stream_tool_calls, Some(false));

        // set_screen_mode wraps `cfg.ui.screen_mode = Some(value)` (the sticky
        // minimal/fullscreen preference written on explicit CLI flags).
        let cfg = apply(|cfg| cfg.ui.screen_mode = Some("minimal".to_string()));
        assert_eq!(cfg.ui.screen_mode, Some("minimal".to_string()));
        let cfg = apply(|cfg| cfg.ui.screen_mode = Some("fullscreen".to_string()));
        assert_eq!(cfg.ui.screen_mode, Some("fullscreen".to_string()));
    }
    /// Theme merge round-trip: verifies the theme field is set and
    /// unmodeled fields survive. Same pattern as `set_compact_mode_round_trips`.
    #[test]
    fn set_theme_round_trips_through_merge() {
        let original = r#"
[ui]
compact_mode = true
theme = "groknight"
auto_dark_theme = "tokyonight"
custom_user_key = "preserve-me"
"#;
        let root: TomlValue = toml::from_str(original).unwrap();
        let mut cfg = load_config_from_toml(&root);
        cfg.ui.theme = Some("tokyonight".to_string());
        let mut table = root.as_table().unwrap().clone();
        merge_section(&mut table, "ui", &cfg.ui);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("theme").and_then(|v| v.as_str()),
            Some("tokyonight"),
            "theme must be set"
        );
        assert_eq!(
            ui.get("compact_mode").and_then(|v| v.as_bool()),
            Some(true),
            "unrelated modeled field must survive"
        );
        assert_eq!(
            ui.get("auto_dark_theme").and_then(|v| v.as_str()),
            Some("tokyonight"),
            "auto_dark_theme must survive"
        );
        assert_eq!(
            ui.get("custom_user_key").and_then(|v| v.as_str()),
            Some("preserve-me"),
            "unmodeled field must survive"
        );
    }
    /// Same as above but for `set_auto_dark_theme` and `set_auto_light_theme`.
    #[test]
    fn set_auto_dark_and_light_theme_round_trip_through_merge() {
        let original = r#"
[ui]
theme = "auto"
auto_dark_theme = "groknight"
auto_light_theme = "grokday"
custom_unknown_key = 42
"#;
        let root: TomlValue = toml::from_str(original).unwrap();
        let mut cfg = load_config_from_toml(&root);
        cfg.ui.auto_dark_theme = Some("tokyonight".to_string());
        cfg.ui.auto_light_theme = Some("rosepine-moon".to_string());
        let mut table = root.as_table().unwrap().clone();
        merge_section(&mut table, "ui", &cfg.ui);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("auto_dark_theme").and_then(|v| v.as_str()),
            Some("tokyonight"),
        );
        assert_eq!(
            ui.get("auto_light_theme").and_then(|v| v.as_str()),
            Some("rosepine-moon"),
        );
        assert_eq!(
            ui.get("theme").and_then(|v| v.as_str()),
            Some("auto"),
            "theme=auto must survive"
        );
        assert_eq!(
            ui.get("custom_unknown_key").and_then(|v| v.as_integer()),
            Some(42),
            "unmodeled field must survive"
        );
    }
    /// Compact-mode merge round-trip: flipped field persists,
    /// unrelated modeled and unmodeled fields survive.
    #[test]
    fn set_compact_mode_round_trips_through_merge() {
        let original = r#"
[ui]
compact_mode = false
auto_dark_theme = "tokyonight"
show_timestamps = true
custom_user_key = "preserve-me"
"#;
        let root: TomlValue = toml::from_str(original).unwrap();
        let mut cfg = load_config_from_toml(&root);
        cfg.ui.compact_mode = true;
        let mut table = root.as_table().unwrap().clone();
        merge_section(&mut table, "ui", &cfg.ui);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("compact_mode").and_then(|v| v.as_bool()),
            Some(true),
            "compact_mode must be flipped"
        );
        assert_eq!(
            ui.get("auto_dark_theme").and_then(|v| v.as_str()),
            Some("tokyonight"),
            "unrelated UI field must survive"
        );
        assert_eq!(
            ui.get("show_timestamps").and_then(|v| v.as_bool()),
            Some(true),
            "show_timestamps must survive"
        );
        assert_eq!(
            ui.get("custom_user_key").and_then(|v| v.as_str()),
            Some("preserve-me"),
            "unmodeled (unknown to the UiConfig schema) field must survive — \
             this is the merge_section invariant the new helpers depend on"
        );
    }

    #[test]
    fn set_code_mode_round_trips_through_merge() {
        let original = r#"
[ui]
code_mode = false
show_timestamps = true
custom_user_key = "preserve-me"
"#;
        let root: TomlValue = toml::from_str(original).unwrap();
        let mut cfg = load_config_from_toml(&root);
        assert_eq!(
            cfg.ui.code_mode,
            Some(crate::agent::config::ToolModePreference::Direct),
            "legacy false must migrate to the explicit Direct preference",
        );

        cfg.ui.code_mode = Some(crate::agent::config::ToolModePreference::CodeModeOnly);

        let mut table = root.as_table().unwrap().clone();
        merge_section(&mut table, "ui", &cfg.ui);

        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("code_mode").and_then(|v| v.as_str()),
            Some("code_mode_only")
        );
        assert_eq!(
            ui.get("show_timestamps").and_then(|v| v.as_bool()),
            Some(true),
            "unrelated modeled field must survive"
        );
        assert_eq!(
            ui.get("custom_user_key").and_then(|v| v.as_str()),
            Some("preserve-me"),
            "unmodeled field must survive"
        );
    }

    /// Same merge round-trip for `show_timestamps` and `simple_mode`.
    #[test]
    fn set_show_timestamps_and_simple_mode_round_trip_through_merge() {
        let original = r#"
[ui]
compact_mode = true
custom_unknown_key = 42
"#;
        let root: TomlValue = toml::from_str(original).unwrap();
        let mut cfg = load_config_from_toml(&root);
        cfg.ui.show_timestamps = Some(false);
        cfg.ui.simple_mode = Some(false);
        let mut table = root.as_table().unwrap().clone();
        merge_section(&mut table, "ui", &cfg.ui);
        let ui = table.get("ui").unwrap().as_table().unwrap();
        assert_eq!(
            ui.get("show_timestamps").and_then(|v| v.as_bool()),
            Some(false),
        );
        assert_eq!(ui.get("simple_mode").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            ui.get("compact_mode").and_then(|v| v.as_bool()),
            Some(true),
            "unrelated modeled field must survive"
        );
        assert_eq!(
            ui.get("custom_unknown_key").and_then(|v| v.as_integer()),
            Some(42),
            "unmodeled (unknown to the schema) field must survive"
        );
    }

    fn custom_record(key: &str, model: &str) -> crate::custom_models::CustomModelRecord {
        crate::custom_models::CustomModelRecord {
            key: key.to_owned(),
            model: model.to_owned(),
            context_window: Some(200_000),
            ..crate::custom_models::CustomModelRecord::default()
        }
    }

    #[test]
    fn custom_model_table_upsert_preserves_siblings_and_extra_fields() {
        let mut root: TomlValue = toml::from_str(
            r#"
[ui]
theme = "groknight"

[model.keep-me]
model = "kept"
description = "sibling"

[model."zai:extra"]
model = "old"
description = "preserve-me"
api_key = "sk-old"
"#,
        )
        .unwrap();
        let record = crate::custom_models::CustomModelRecord {
            name: Some("Extra".into()),
            provider: Some("zai".into()),
            env_key: Some("ZAI_API_KEY".into()),
            context_window: Some(500_000),
            ..custom_record("zai:extra", "glm-extra")
        };
        let parsed = persist_custom_model_upsert_to_root(&mut root, &record).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("glm-extra"));
        assert_eq!(parsed.context_window, Some(500_000));
        assert_eq!(parsed.name.as_deref(), Some("Extra"));
        assert_eq!(
            root.get("ui")
                .and_then(|v| v.get("theme"))
                .and_then(TomlValue::as_str),
            Some("groknight")
        );
        let extra = root
            .get("model")
            .and_then(|v| v.get("zai:extra"))
            .and_then(TomlValue::as_table)
            .unwrap();
        assert_eq!(
            extra.get("description").and_then(TomlValue::as_str),
            Some("preserve-me")
        );
        assert!(
            extra.get("api_key").is_none(),
            "env_key upsert must drop a stored api_key"
        );
        assert_eq!(
            extra.get("env_key").and_then(TomlValue::as_str),
            Some("ZAI_API_KEY")
        );
        assert_eq!(
            root.get("model")
                .and_then(|v| v.get("keep-me"))
                .and_then(|v| v.get("model"))
                .and_then(TomlValue::as_str),
            Some("kept")
        );
    }

    #[test]
    fn custom_model_table_delete_removes_only_that_entry() {
        let mut root: TomlValue = toml::from_str(
            r#"
[model.keep-me]
model = "kept"
[model.drop-me]
model = "gone"
"#,
        )
        .unwrap();
        assert!(delete_config_model_table(&mut root, "drop-me"));
        assert!(root.get("model").and_then(|v| v.get("drop-me")).is_none());
        assert_eq!(
            root.get("model")
                .and_then(|v| v.get("keep-me"))
                .and_then(|v| v.get("model"))
                .and_then(TomlValue::as_str),
            Some("kept")
        );
        assert!(delete_config_model_table(&mut root, "keep-me"));
        assert!(
            root.get("model").is_none(),
            "empty [model] section must be dropped"
        );
    }

    #[test]
    fn custom_model_experimental_metadata_upserts_preserve_omissions_and_clear_explicitly() {
        let mut root: TomlValue = toml::from_str(
            r#"
[model."openai:api-test-model"]
model = "advertised-api-test-model"
provider = "codex"
api_backend = "responses"
api_key = "test-api-secret"
description = "keep this description"
"#,
        )
        .unwrap();
        let record = crate::custom_models::CustomModelRecord {
            use_responses_lite: Some(true),
            experimental_supported_tools: Some(vec!["send_user_message_async".into()]),
            ..custom_record("openai:api-test-model", "advertised-api-test-model")
        };
        let enabled = persist_custom_model_upsert_to_root(&mut root, &record).unwrap();
        assert_eq!(enabled.use_responses_lite, Some(true));
        assert_eq!(
            enabled.experimental_supported_tools,
            Some(vec!["send_user_message_async".into()])
        );

        let rename = crate::custom_models::CustomModelRecord {
            name: Some("Test API".into()),
            ..custom_record("openai:api-test-model", "advertised-api-test-model")
        };
        let renamed = persist_custom_model_upsert_to_root(&mut root, &rename).unwrap();
        assert_eq!(renamed.name.as_deref(), Some("Test API"));
        assert_eq!(renamed.use_responses_lite, enabled.use_responses_lite);
        assert_eq!(
            renamed.experimental_supported_tools,
            enabled.experimental_supported_tools
        );

        let disabled = crate::custom_models::CustomModelRecord {
            use_responses_lite: Some(false),
            experimental_supported_tools: Some(Vec::new()),
            ..custom_record("openai:api-test-model", "advertised-api-test-model")
        };
        let disabled = persist_custom_model_upsert_to_root(&mut root, &disabled).unwrap();
        let serialized = toml::to_string_pretty(&root).unwrap();
        let reloaded =
            crate::agent::config::Config::new_from_toml_cfg(&toml::from_str(&serialized).unwrap())
                .unwrap();
        let loaded = &reloaded.config_models["openai:api-test-model"];
        assert_eq!(loaded.use_responses_lite, Some(false));
        assert_eq!(loaded.experimental_supported_tools, Some(Vec::new()));
        assert_eq!(loaded.name.as_deref(), Some("Test API"));
        assert_eq!(loaded.description.as_deref(), Some("keep this description"));
        let public = crate::custom_models::override_to_public("openai:api-test-model", loaded);
        assert_eq!(
            public,
            crate::custom_models::override_to_public("openai:api-test-model", &disabled)
        );
        let json = serde_json::to_value(public).unwrap();
        assert_eq!(json["use_responses_lite"], false);
        assert_eq!(json["experimental_supported_tools"], serde_json::json!([]));
        assert_eq!(json["has_api_key"], true);
        assert!(json.get("api_key").is_none());
        assert!(!json.to_string().contains("test-api-secret"));
    }

    /// The wizard saves a whole endpoint at once: every row must land in one
    /// file write, keep unrelated config intact, and carry the credential
    /// header derived from the chosen wire format.
    #[test]
    fn custom_model_batch_upsert_writes_every_row_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[ui]\ntheme = \"groknight\"\n[model.keep-me]\nmodel = \"kept\"\n",
        )
        .unwrap();
        let records: Vec<_> = [
            ("localhost-11434:qwen3", "qwen3:latest", "responses"),
            ("localhost-11434:llama", "llama3:latest", "chat_completions"),
        ]
        .into_iter()
        .map(|(key, model, api_backend)| {
            crate::custom_models::normalize_custom_model(crate::custom_models::CustomModelRecord {
                provider: Some("custom".into()),
                base_url: Some("http://localhost:11434/v1".into()),
                api_backend: Some(api_backend.into()),
                context_window: Some(131_072),
                key: key.into(),
                model: model.into(),
                ..Default::default()
            })
            .unwrap()
            .0
        })
        .collect();

        let written = persist_custom_model_upserts_at(&path, &records).unwrap();
        assert_eq!(written.len(), 2);
        assert!(
            written
                .iter()
                .all(|(_, model)| model.base_url.as_deref() == Some("http://localhost:11434/v1")),
            "each row carries the endpoint it was discovered from"
        );

        let parsed: TomlValue = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("ui")
                .and_then(|v| v.get("theme"))
                .and_then(TomlValue::as_str),
            Some("groknight"),
            "unrelated config survives a batch write"
        );
        let models = parsed.get("model").and_then(TomlValue::as_table).unwrap();
        assert!(models.contains_key("keep-me"));
        assert_eq!(
            models
                .get("localhost-11434:qwen3")
                .and_then(TomlValue::as_table)
                .and_then(|t| t.get("model"))
                .and_then(TomlValue::as_str),
            Some("qwen3:latest")
        );
    }

    /// An Anthropic-format batch must persist the header choice, or the saved
    /// models would authenticate with the wrong header on the next start.
    #[test]
    fn custom_model_batch_upsert_persists_the_messages_auth_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (record, _) =
            crate::custom_models::normalize_custom_model(crate::custom_models::CustomModelRecord {
                key: "gateway.example.com:claude-x".into(),
                model: "claude-x".into(),
                provider: Some("custom".into()),
                api_backend: Some("messages".into()),
                base_url: Some("https://gateway.example.com/anthropic".into()),
                ..Default::default()
            })
            .unwrap();
        persist_custom_model_upserts_at(&path, std::slice::from_ref(&record)).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("auth_scheme = \"x_api_key\""), "{written}");
        assert!(
            written.contains("[model.\"gateway.example.com:claude-x\"]"),
            "a dotted catalog key must land as one quoted table name: {written}"
        );
        let parsed: TomlValue = toml::from_str(&written).unwrap();
        let entry: crate::agent::config::ConfigModelOverride = parsed
            .get("model")
            .and_then(|models| models.get("gateway.example.com:claude-x"))
            .cloned()
            .expect("written table")
            .try_into()
            .expect("[model.*] table reparses as an override");
        assert_eq!(
            entry.auth_scheme,
            Some(xai_grok_sampler::AuthScheme::XApiKey)
        );
        assert_eq!(
            entry.api_backend,
            Some(xai_grok_sampling_types::ApiBackend::Messages)
        );
    }

    #[test]
    fn custom_model_persist_round_trips_on_isolated_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[ui]\ntheme = \"groknight\"\n[model.keep-me]\nmodel = \"kept\"\n",
        )
        .unwrap();
        let (record, _) =
            crate::custom_models::normalize_custom_model(crate::custom_models::CustomModelRecord {
                provider: Some("zai".into()),
                context_window: Some(1_000_000),
                ..custom_record("zai:glm-5.2", "glm-5.2")
            })
            .unwrap();
        persist_custom_model_upsert_at(&path, &record).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("sk-"),
            "upsert must not invent secrets: {written}"
        );
        let parsed: TomlValue = toml::from_str(&written).unwrap();
        assert_eq!(
            parsed
                .get("ui")
                .and_then(|v| v.get("theme"))
                .and_then(TomlValue::as_str),
            Some("groknight")
        );
        let extra = parsed
            .get("model")
            .and_then(|v| v.get("zai:glm-5.2"))
            .and_then(TomlValue::as_table)
            .expect("quoted colon key must round-trip");
        assert_eq!(
            extra.get("model").and_then(TomlValue::as_str),
            Some("glm-5.2")
        );
        assert_eq!(
            extra.get("context_window").and_then(TomlValue::as_integer),
            Some(1_000_000)
        );
        assert_eq!(
            extra.get("base_url").and_then(TomlValue::as_str),
            Some(crate::zai_models::api_base_url().as_str())
        );
        assert_eq!(
            extra.get("env_key").and_then(TomlValue::as_str),
            Some(crate::zai_models::ZAI_API_KEY_ENV)
        );
        persist_custom_model_delete_at(&path, "zai:glm-5.2").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let parsed: TomlValue = toml::from_str(&after).unwrap();
        assert!(
            parsed
                .get("model")
                .and_then(|v| v.get("zai:glm-5.2"))
                .is_none()
        );
        assert_eq!(
            parsed
                .get("model")
                .and_then(|v| v.get("keep-me"))
                .and_then(|v| v.get("model"))
                .and_then(TomlValue::as_str),
            Some("kept")
        );
    }
    #[cfg(unix)]
    fn project_slot_symlink(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let outside = dir.join("outside.toml");
        std::fs::write(&outside, "keep\n").unwrap();
        let link = dir.join(".opengrok").join("config.toml");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        (link, outside)
    }
    #[cfg(unix)]
    fn dotfiles_config_symlink(
        dir: &std::path::Path,
        contents: Option<&str>,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let repo = dir.join("dotfiles");
        std::fs::create_dir_all(&repo).unwrap();
        let target = repo.join("config.toml");
        if let Some(contents) = contents {
            std::fs::write(&target, contents).unwrap();
        }
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        (link, target)
    }
    #[cfg(unix)]
    fn assert_still_symlink(path: &std::path::Path) {
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    /// Project `.opengrok/config.toml` must replace a leaf symlink, not follow it.
    #[cfg(unix)]
    #[test]
    fn atomic_replace_string_replaces_project_config_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let (link, outside) = project_slot_symlink(dir.path());
        atomic_replace_string(&link, "[mcp_servers]\n").unwrap();
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(
            !meta.file_type().is_symlink(),
            "project config symlink must be replaced: {:?}",
            meta.file_type()
        );
        assert_eq!("[mcp_servers]\n", std::fs::read_to_string(&link).unwrap());
        assert_eq!("keep\n", std::fs::read_to_string(&outside).unwrap());
    }
    /// No user grok home: persist must resolve the cwd `.opengrok/config.toml` as a
    /// slot (replace), not follow an external referent.
    #[cfg(unix)]
    #[test]
    fn no_home_cwd_config_resolves_slot_not_follow() {
        let dir = tempfile::tempdir().unwrap();
        let (link, outside) = project_slot_symlink(dir.path());
        let followed = bind_user_config_dest_with(&link, true, true).unwrap();
        let slot = bind_user_config_dest_with(&link, true, false).unwrap();
        assert_eq!(
            outside,
            followed.as_path(),
            "with a user home, follow the referent"
        );
        assert_eq!(
            link,
            slot.as_path(),
            "without a user home, replace the slot inode"
        );
        xai_grok_config::fs_atomic::require_same_follow_destination(&link, slot.as_path())
            .expect_err("follow check rejects a slot bind");
        let dest = require_same_user_config_dest_with(&link, &slot, false).unwrap();
        atomic_write_resolved_string(&dest, "new\n").unwrap();
        assert_eq!("new\n", std::fs::read_to_string(&link).unwrap());
        assert_eq!("keep\n", std::fs::read_to_string(&outside).unwrap());
    }
    /// A 0600 referent must not be published via a 0644 temp (chmod-ignored).
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_preserves_0600_referent_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        atomic_write_string(&path, "new\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(0o600, mode, "must not publish 0644 over a 0600 referent");
        assert_eq!("new\n", std::fs::read_to_string(&path).unwrap());
    }
    /// A retarget after bind refuses the write.
    #[cfg(unix)]
    #[test]
    fn follow_bound_rmw_refuses_retarget_between_read_and_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.toml");
        let b = dir.path().join("b.toml");
        std::fs::write(&a, "from_a = true\n").unwrap();
        std::fs::write(&b, "from_b = true\n").unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let (dest, content) = read_follow_bound(&link).unwrap();
        assert_eq!("from_a = true\n", content);
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        let err = atomic_write_follow_bound(&link, &dest, "from_a = true\nmerged = true\n")
            .expect_err("retarget");
        assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
        assert_eq!("from_a = true\n", std::fs::read_to_string(&a).unwrap());
        assert_eq!("from_b = true\n", std::fs::read_to_string(&b).unwrap());
    }
    /// Same-path inode replace after bind must refuse.
    #[cfg(unix)]
    #[test]
    fn follow_bound_rmw_refuses_same_path_inode_replace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest_file = dir.path().join("a.toml");
        std::fs::write(&dest_file, "from_a = true\n").unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&dest_file, &link).unwrap();
        let (dest, content) = read_follow_bound(&link).unwrap();
        assert_eq!("from_a = true\n", content);
        std::fs::remove_file(&dest_file).unwrap();
        std::fs::write(&dest_file, "from_b = true\n").unwrap();
        let err = atomic_write_follow_bound(&link, &dest, "from_a = true\nmerged = true\n")
            .expect_err("inode replace");
        assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
        assert_eq!(
            "from_b = true\n",
            std::fs::read_to_string(&dest_file).unwrap()
        );
    }
    /// Bind missing dest, then swap the parent directory for a symlink to B.
    #[cfg(unix)]
    #[test]
    fn follow_bound_rmw_refuses_absent_dest_ancestor_retarget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("A");
        let b = dir.path().join("B");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let dest_path = a.join("config.toml");
        let bound = bind_user_config_dest_with(&dest_path, true, true).unwrap();
        std::fs::remove_dir(&a).unwrap();
        std::os::unix::fs::symlink(&b, &a).unwrap();
        let err = atomic_write_resolved_string(&bound, "from_a\n").expect_err("ancestor");
        assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
        assert!(!b.join("config.toml").exists(), "must not write B");
    }
    /// `config.toml` as a symlink into a repo file: tmp+rename must write the referent.
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_preserves_config_toml_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (link, target) =
            dotfiles_config_symlink(dir.path(), Some("[ui]\nsimple_mode = false\n"));
        atomic_write_string(&link, "[ui]\nsimple_mode = true\n").unwrap();
        assert_still_symlink(&link);
        assert_eq!(target, std::fs::read_link(&link).unwrap());
        assert_eq!(
            "[ui]\nsimple_mode = true\n",
            std::fs::read_to_string(&target).unwrap()
        );
    }
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_creates_dangling_symlink_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (link, target) = dotfiles_config_symlink(dir.path(), None);
        atomic_write_string(&link, "created\n").unwrap();
        assert_still_symlink(&link);
        assert_eq!("created\n", std::fs::read_to_string(&target).unwrap());
    }
    /// Trailing `/` on a dangling target must not create a regular sibling that later
    /// reads through the symlink see as `ENOTDIR`.
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_refuses_trailing_slash_dangling_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing-target");
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink("missing-target/", &link).unwrap();
        let err = atomic_write_string(&link, "created\n").expect_err("ENOTDIR");
        assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
        assert!(!missing.exists(), "must not create a regular sibling");
        assert_still_symlink(&link);
    }
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_refuses_trailing_dot_dangling_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing-target");
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink("missing-target/.", &link).unwrap();
        let err = atomic_write_string(&link, "created\n").expect_err("ENOTDIR");
        assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
        assert!(!missing.exists(), "must not create a regular sibling");
    }
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_follows_relative_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("dotfiles");
        std::fs::create_dir_all(&repo).unwrap();
        let target = repo.join("config.toml");
        std::fs::write(&target, "before\n").unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink("dotfiles/config.toml", &link).unwrap();
        atomic_write_string(&link, "after\n").unwrap();
        assert_still_symlink(&link);
        assert_eq!("after\n", std::fs::read_to_string(&target).unwrap());
    }
    /// `file/../config.toml` must not resolve to the sibling (kernel `ENOTDIR`).
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_refuses_parent_through_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("file");
        let victim = dir.path().join("config.toml");
        std::fs::write(&file, "file").unwrap();
        std::fs::write(&victim, "keep\n").unwrap();
        let err = atomic_write_string(&file.join("..").join("config.toml"), "clobber\n")
            .expect_err("ENOTDIR");
        assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
        assert_eq!("keep\n", std::fs::read_to_string(&victim).unwrap());
    }
    /// `missing/../config.toml` must not create `missing` then rename over the symlink.
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_refuses_parent_through_missing_then_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing");
        let target = dir.path().join("dotfiles.toml");
        let link = dir.path().join("config.toml");
        std::fs::write(&target, "keep\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = atomic_write_string(&missing.join("..").join("config.toml"), "clobber\n")
            .expect_err("ENOENT");
        assert_eq!(std::io::ErrorKind::NotFound, err.kind());
        assert!(!missing.exists(), "must not create_dir_all the missing hop");
        assert_still_symlink(&link);
        assert_eq!("keep\n", std::fs::read_to_string(&target).unwrap());
    }
    /// Directory squat at the destination: fail closed, do not replace with a file.
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_refuses_existing_directory_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let squat = dir.path().join("config.toml");
        std::fs::create_dir(&squat).unwrap();
        let err = atomic_write_string(&squat, "clobber\n").expect_err("EISDIR");
        assert_eq!(std::io::ErrorKind::IsADirectory, err.kind());
        assert!(squat.is_dir(), "directory squat must remain a directory");
    }
    /// Trailing `/` or `/.` on the destination path itself (not a symlink target).
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_refuses_trailing_slash_on_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let victim = dir.path().join("config.toml");
        std::fs::write(&victim, "keep\n").unwrap();
        let slash = {
            let mut p = victim.clone().into_os_string();
            p.push("/");
            std::path::PathBuf::from(p)
        };
        let err = atomic_write_string(&slash, "clobber\n").expect_err("ENOTDIR");
        assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
        assert_eq!("keep\n", std::fs::read_to_string(&victim).unwrap());
        let dot = {
            let mut p = victim.into_os_string();
            p.push("/.");
            std::path::PathBuf::from(p)
        };
        let err = atomic_write_string(&dot, "clobber\n").expect_err("ENOTDIR");
        assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    }
    /// On Unix `\` is a valid filename character; save through `target\` must work.
    #[cfg(unix)]
    #[test]
    fn atomic_write_string_follows_unix_backslash_filename_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target\\");
        std::fs::write(&target, "before\n").unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        atomic_write_string(&link, "after\n").unwrap();
        assert_still_symlink(&link);
        assert_eq!("after\n", std::fs::read_to_string(&target).unwrap());
    }
    /// Cancelling the `run_blocking` await must not drop the write guard before the worker finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_blocking_save_holds_write_guard_until_worker_finishes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let guard = tokio::time::timeout(Duration::from_secs(5), lock_config_writes())
            .await
            .expect("first lock timed out")
            .expect("first lock");
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let started_w = started.clone();
        let finished_w = finished.clone();
        let task = tokio::spawn(async move {
            guard
                .run_blocking(move || {
                    started_w.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(250));
                    finished_w.store(true, Ordering::SeqCst);
                    Ok::<(), std::io::Error>(())
                })
                .await
        });
        let wait_started = tokio::time::timeout(Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(wait_started.is_ok(), "blocking worker never started");
        task.abort();
        let _ = task.await;
        let _g2 = tokio::time::timeout(Duration::from_secs(2), lock_config_writes())
            .await
            .expect("second lock timed out")
            .expect("second lock");
        assert!(
            finished.load(Ordering::SeqCst),
            "second writer acquired locks before the detached save released them"
        );
    }
}
