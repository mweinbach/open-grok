//! Shared take/apply for `[[version_overrides]]` / `[[campaigns]]` arrays.

use serde::de::DeserializeOwned;

use crate::deep_merge_toml;

pub type PatchPath = &'static [&'static str];

#[derive(Debug, Clone)]
pub struct ConfigOverrideEntry<M> {
    pub meta: M,
    pub patch: toml::Table,
}

/// Strip `key` from the root table; each element is `M` + remaining keys as patch.
pub fn take_patch_array<M>(
    config: &mut toml::Value,
    key: &str,
) -> Result<Vec<ConfigOverrideEntry<M>>, toml::de::Error>
where
    M: DeserializeOwned,
{
    let Some(table) = config.as_table_mut() else {
        return Ok(Vec::new());
    };
    let Some(array_value) = table.remove(key) else {
        return Ok(Vec::new());
    };

    #[derive(serde::Deserialize)]
    struct FlatEntry<M> {
        #[serde(flatten)]
        meta: M,
        #[serde(flatten)]
        patch: toml::Table,
    }

    let entries: Vec<FlatEntry<M>> = array_value.try_into()?;
    Ok(entries
        .into_iter()
        .map(|e| ConfigOverrideEntry {
            meta: e.meta,
            patch: e.patch,
        })
        .collect())
}

/// Whether `patch` affects the value at `path`: it sets a value there (any leaf
/// under it counts), **or** it sets a non-table ancestor — deep-merge replaces
/// the whole subtree in that case, so every leaf beneath is touched (a patch
/// like `models = "oops"` wipes `models.default` and must still be dismissable
/// / flagged as driving it).
pub fn patch_touches_path(patch: &toml::Table, path: PatchPath) -> bool {
    let Some(first) = path.first() else {
        return false;
    };
    let Some(mut cur) = patch.get(*first) else {
        return false;
    };
    for seg in path.iter().skip(1) {
        match cur.as_table() {
            Some(t) => match t.get(*seg) {
                Some(v) => cur = v,
                None => return false,
            },
            // Non-table ancestor: the merge replaces this subtree wholesale.
            None => return true,
        }
    }
    true
}

/// Whether `patch` touches any of `paths`.
pub fn patch_touches_any(patch: &toml::Table, paths: &[PatchPath]) -> bool {
    paths.iter().any(|p| patch_touches_path(patch, p))
}

/// Keys stripped from every applied patch: an override cannot re-inject nested
/// `version_overrides`/`campaigns` or define `[auth_provider.*]` /
/// `[model_providers.*]` command tables.
pub const PATCH_STRIP_KEYS: &[&str] = &[
    "version_overrides",
    "campaigns",
    "auth_provider",
    "model_providers",
    "mcp_servers",
];

/// Commands in remote patches must never become local executable configuration.
pub const PATCH_STRIP_PATHS: &[PatchPath] =
    &[&["ui", "status_line"], &["ui", "notifications", "hooks"]];

/// Deep-merge each patch in iteration order (later wins on a leaf), stripping
/// `strip_keys` (top level) first.
///
/// A patch is another input to the same merge as the disk layers, so it gets the same pre-merge
/// normalization ([`crate::loader::normalize_config_layer`]). Otherwise a patch that flips
/// `[toolset.web_search]` from an allowlist to a blocklist would leave both keys set, and the
/// resolver would drop the blocklist and let the layer the patch overlays win.
pub fn apply_patches(
    config: &mut toml::Value,
    patches: impl IntoIterator<Item = toml::Table>,
    strip_keys: &[&str],
) {
    for mut patch in patches {
        for key in strip_keys {
            patch.remove(*key);
        }
        for path in PATCH_STRIP_PATHS {
            strip_path(&mut patch, path);
        }
        let mut patch = toml::Value::Table(patch);
        crate::loader::normalize_config_layer(&mut patch);
        deep_merge_toml(config, &patch);
    }
}

fn strip_path(patch: &mut toml::Table, path: PatchPath) {
    let Some((key, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        patch.remove(*key);
        return;
    }
    match patch.get_mut(*key) {
        Some(toml::Value::Table(nested)) => strip_path(nested, rest),
        Some(_) => {
            // A scalar ancestor would replace the protected subtree wholesale.
            patch.remove(*key);
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(s: &str) -> toml::Table {
        toml::from_str(s).unwrap()
    }

    /// A patch that replaces a parent table with a scalar (`models = "oops"`)
    /// wipes every leaf beneath it on merge, so it must count as touching those
    /// leaves — otherwise the campaign that destroyed `models.default` would be
    /// neither dismissable nor flagged as driving the field.
    #[test]
    fn non_table_ancestor_counts_as_touching_leaves_beneath() {
        let patch = table("models = \"oops\"\n");
        assert!(patch_touches_path(&patch, &["models", "default"]));
        assert!(patch_touches_path(&patch, &["models"]));
        // Sibling sections are unaffected.
        assert!(!patch_touches_path(&patch, &["features", "campaigns"]));
        // A well-formed table patch still requires the leaf to be present.
        let tbl = table("[models]\ndefault = \"m\"\n");
        assert!(patch_touches_path(&tbl, &["models", "default"]));
        assert!(!patch_touches_path(&tbl, &["models", "other"]));
    }

    #[test]
    fn apply_patches_strips_requested_keys() {
        let mut cfg = toml::Value::Table(table("[models]\ndefault = \"old\"\n"));
        let patch = table("[models]\ndefault = \"new\"\n");
        apply_patches(&mut cfg, std::iter::once(patch), PATCH_STRIP_KEYS);
        assert_eq!(cfg["models"]["default"].as_str(), Some("new"));

        // Top-level strip keys are removed before merge.
        let mut cfg2 = toml::Value::Table(toml::Table::new());
        let mut p = toml::Table::new();
        p.insert("version_overrides".into(), toml::Value::Array(vec![]));
        p.insert("campaigns".into(), toml::Value::Array(vec![]));
        p.insert(
            "auth_provider".into(),
            toml::Value::Table(toml::Table::new()),
        );
        p.insert(
            "model_providers".into(),
            toml::Value::Table(toml::Table::new()),
        );
        p.insert("keep".into(), toml::Value::Boolean(true));
        apply_patches(&mut cfg2, std::iter::once(p), PATCH_STRIP_KEYS);
        assert!(cfg2.get("version_overrides").is_none());
        assert!(cfg2.get("campaigns").is_none());
        assert!(cfg2.get("auth_provider").is_none());
        assert!(cfg2.get("model_providers").is_none());
        assert_eq!(cfg2["keep"].as_bool(), Some(true));

        // Top-level strip only: a model may still reference a local provider by name.
        let mut cfg3 = toml::Value::Table(toml::Table::new());
        let p = table(
            "[auth_provider.injected]\ncommand = \"evil\"\n\
             [model_providers.injected]\nbase_url = \"https://evil.example/v1\"\n\
             [model.x]\nauth_provider = \"local-name\"\nmodel_provider = \"local-provider\"\n",
        );
        apply_patches(&mut cfg3, std::iter::once(p), PATCH_STRIP_KEYS);
        assert!(cfg3.get("auth_provider").is_none());
        assert!(cfg3.get("model_providers").is_none());
        assert_eq!(
            cfg3["model"]["x"]["auth_provider"].as_str(),
            Some("local-name")
        );
        assert_eq!(
            cfg3["model"]["x"]["model_provider"].as_str(),
            Some("local-provider")
        );
    }

    #[test]
    fn apply_patches_strip_remote_executable_paths() {
        let mut cfg = toml::Value::Table(table(
            "[ui]\ntheme = \"kanagawa\"\n[ui.notifications]\nenabled = true\n",
        ));
        let patch = table(
            "[ui]\ntheme = \"other\"\n[ui.status_line]\ncommand = \"curl evil\"\n\
             [[ui.notifications.hooks]]\ncommand = \"curl worse\"\n",
        );

        apply_patches(&mut cfg, std::iter::once(patch), PATCH_STRIP_KEYS);

        assert!(cfg["ui"].get("status_line").is_none());
        assert!(cfg["ui"]["notifications"].get("hooks").is_none());
        assert_eq!(cfg["ui"]["theme"].as_str(), Some("other"));
    }

    #[test]
    fn apply_patches_strip_remote_mcp_commands() {
        let mut cfg = toml::Value::Table(table("[ui]\ntheme = \"kanagawa\"\n"));
        let patch = table(
            "[ui]\ntheme = \"other\"\n[mcp_servers.untrusted]\ncommand = \"curl\"\nargs = [\"evil\"]\n",
        );

        apply_patches(&mut cfg, std::iter::once(patch), PATCH_STRIP_KEYS);

        assert!(cfg.get("mcp_servers").is_none());
        assert_eq!(cfg["ui"]["theme"].as_str(), Some("other"));
    }

    #[test]
    fn non_table_protected_ancestor_is_stripped() {
        let mut cfg = toml::Value::Table(table("[ui]\ntheme = \"kanagawa\"\n"));
        let mut patch = toml::Table::new();
        patch.insert("ui".into(), toml::Value::String("oops".into()));

        apply_patches(&mut cfg, std::iter::once(patch), PATCH_STRIP_KEYS);

        assert_eq!(cfg["ui"]["theme"].as_str(), Some("kanagawa"));
    }
}
