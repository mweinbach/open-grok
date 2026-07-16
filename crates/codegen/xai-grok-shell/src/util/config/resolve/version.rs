use toml::Value as TomlValue;

/// Read the minimum-version floor from one TOML layer.
pub fn minimum_version_from_toml(root: &TomlValue) -> Option<String> {
    root.get("cli")?
        .get("minimum_version")?
        .as_str()
        .map(str::to_owned)
}

/// Semver-max across candidates. Fails closed on any unparseable input so a
/// typo in one layer can't silently disable enforcement.
pub fn pick_max_minimum_version(
    candidates: &[&str],
) -> Result<Option<String>, (String, semver::Error)> {
    let mut best: Option<semver::Version> = None;
    for raw in candidates {
        let parsed = semver::Version::parse(raw).map_err(|e| ((*raw).to_string(), e))?;
        match best.as_ref() {
            Some(cur) if cur >= &parsed => {}
            _ => best = Some(parsed),
        }
    }
    Ok(best.map(|v| v.to_string()))
}

/// Effective `cli.minimum_version`: semver-max across all layers so managed
/// floors can't be lowered by user/project pins.
pub fn resolve_minimum_version() -> Result<Option<String>, (String, semver::Error)> {
    let layers = match crate::config::ConfigLayers::load() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "minimum_version: failed to load config layers");
            return Ok(None);
        }
    };
    resolve_minimum_version_from_layers(&layers)
}

/// Semver-max of `cli.minimum_version` across every layer (incl. the macOS MDM
/// floor) so a managed floor can't be lowered by a user/project pin. Split from
/// the disk load so the layer set can be injected in tests.
fn resolve_minimum_version_from_layers(
    layers: &crate::config::ConfigLayers,
) -> Result<Option<String>, (String, semver::Error)> {
    let candidates: Vec<String> = [
        minimum_version_from_toml(&layers.system_managed),
        minimum_version_from_toml(&layers.managed),
        minimum_version_from_toml(&layers.user),
        layers
            .user_requirements
            .as_ref()
            .and_then(minimum_version_from_toml),
        layers
            .system_requirements
            .as_ref()
            .and_then(minimum_version_from_toml),
        layers
            .mdm_requirements
            .as_ref()
            .and_then(minimum_version_from_toml),
    ]
    .into_iter()
    .flatten()
    .collect();

    let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
    pick_max_minimum_version(&refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_max_minimum_version_picks_max_and_fails_closed_on_typos() {
        assert_eq!(
            pick_max_minimum_version(&["0.1.200", "0.1.100"])
                .unwrap()
                .as_deref(),
            Some("0.1.200")
        );
        let (bad, _) = pick_max_minimum_version(&["not-a-version", "0.1.150"]).unwrap_err();
        assert_eq!(bad, "not-a-version");
    }

    #[test]
    fn minimum_version_includes_the_mdm_layer() {
        // The MDM floor must win the semver-max so a managed minimum can't be
        // lowered by a user pin.
        let layers = crate::config::ConfigLayers {
            system_managed: TomlValue::Table(Default::default()),
            managed: TomlValue::Table(Default::default()),
            user: toml::from_str("[cli]\nminimum_version = \"0.1.100\"\n").unwrap(),
            user_requirements: None,
            system_requirements: None,
            mdm_requirements: Some(
                toml::from_str("[cli]\nminimum_version = \"0.1.200\"\n").unwrap(),
            ),
            ..Default::default()
        };
        assert_eq!(
            resolve_minimum_version_from_layers(&layers)
                .unwrap()
                .as_deref(),
            Some("0.1.200"),
        );
    }
}
