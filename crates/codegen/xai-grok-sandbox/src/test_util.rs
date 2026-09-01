use crate::profiles::{ProfileConfig, ProfileName, SandboxConfig};
use std::collections::HashMap;

pub(crate) fn skip_if_host_hook_write_deny_unresolvable() -> bool {
    if !crate::hook_write_deny::profile_enforces_hook_write_deny(&ProfileName::Workspace) {
        return false;
    }
    match crate::hook_write_deny::resolve_hook_write_deny_snapshot() {
        Ok(_) => false,
        Err(e) => {
            eprintln!("skipping profile resolve test: host hook write-deny unresolvable ({e})");
            true
        }
    }
}

pub(crate) fn network_inheritance_config() -> SandboxConfig {
    SandboxConfig {
        profiles: HashMap::from([
            (
                "strict-inherited".to_string(),
                ProfileConfig {
                    extends: Some("strict".to_string()),
                    restrict_network: None,
                    read_only: vec![],
                    read_write: vec![],
                    deny: vec![],
                },
            ),
            (
                "read-only-inherited".to_string(),
                ProfileConfig {
                    extends: Some("read-only".to_string()),
                    restrict_network: None,
                    read_only: vec![],
                    read_write: vec![],
                    deny: vec![],
                },
            ),
            (
                "strict-unrestricted".to_string(),
                ProfileConfig {
                    extends: Some("strict".to_string()),
                    restrict_network: Some(false),
                    read_only: vec![],
                    read_write: vec![],
                    deny: vec![],
                },
            ),
            (
                "workspace-restricted".to_string(),
                ProfileConfig {
                    extends: Some("workspace".to_string()),
                    restrict_network: Some(true),
                    read_only: vec![],
                    read_write: vec![],
                    deny: vec![],
                },
            ),
        ]),
    }
}
