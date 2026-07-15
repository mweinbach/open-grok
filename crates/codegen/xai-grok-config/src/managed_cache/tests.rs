use super::*;

fn team(id: &str) -> ServingIdentity {
    ServingIdentity::Team(id.to_owned())
}
fn dkey(fp: &str) -> ServingIdentity {
    ServingIdentity::DeploymentKey {
        fingerprint: fp.to_owned(),
    }
}

/// The authoritative signed verdict wins over the marker BOTH ways: `Compromised`
/// refuses where the marker alone would pass, `Trusted` proceeds over marker tamper.
#[test]
fn signed_verdict_overrides_marker_both_ways() {
    use crate::signed_policy::SignedVerdict;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // Marker: opted-in, served requirements now MISSING on disk (marker alone would refuse).
    let cache = ManagedConfigCache {
        principal: Some("team-007".into()),
        had_requirements: true,
        fail_closed: true,
        ..Default::default()
    };
    // Signed says NOT compromised → proceed, overriding the marker's tamper signal.
    assert!(!managed_policy_compromised_decision(
        SignedVerdict::Trusted,
        false,
        Some(&cache),
        home,
        &team("team-007")
    ));
    // Signed says compromised → refuse, though this intact marker alone would pass.
    let intact = ManagedConfigCache {
        principal: Some("team-007".into()),
        fail_closed: true,
        ..Default::default()
    };
    assert!(managed_policy_compromised_decision(
        SignedVerdict::Compromised,
        false,
        Some(&intact),
        home,
        &team("team-007")
    ));
}

/// `Trusted` must NOT short-circuit past the deploy-key fingerprint check (the signature
/// can't attest the local key): a mismatch falls through to the marker decision, while
/// `Compromised` refuses regardless of the fingerprint.
#[test]
fn signed_verdict_does_not_skip_deploy_key_fingerprint() {
    use crate::signed_policy::SignedVerdict;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // Marker recorded fingerprint "fp-cache"; the host is now configured with "fp-local".
    let opted_in = ManagedConfigCache {
        principal: Some("dep-1".into()),
        key_fingerprint: Some("fp-cache".into()),
        fail_closed: true,
        ..Default::default()
    };
    // Trusted + fingerprint mismatch forces the marker path, which refuses an
    // opted-in cache.
    assert!(managed_policy_compromised_decision(
        SignedVerdict::Trusted,
        true, // deploy-key fingerprint mismatch
        Some(&opted_in),
        home,
        &dkey("fp-local")
    ));
    // A matching fingerprint trusts the signed verdict as before.
    assert!(!managed_policy_compromised_decision(
        SignedVerdict::Trusted,
        false,
        Some(&opted_in),
        home,
        &dkey("fp-cache")
    ));
    // Opted-OUT deploy host with a key change → not refused (preserves non-fail-closed behavior).
    let opted_out = ManagedConfigCache {
        principal: Some("dep-1".into()),
        key_fingerprint: Some("fp-cache".into()),
        fail_closed: false,
        ..Default::default()
    };
    assert!(!managed_policy_compromised_decision(
        SignedVerdict::Trusted,
        true,
        Some(&opted_out),
        home,
        &dkey("fp-local")
    ));
    // Compromised refuses EVEN with a fingerprint mismatch — never falls through to
    // this opted-OUT marker.
    assert!(managed_policy_compromised_decision(
        SignedVerdict::Compromised,
        true,
        Some(&opted_out),
        home,
        &dkey("fp-local")
    ));
}

/// A sidecar read BLIP is not absence: unlike NoAuthenticSidecar it never refuses on
/// its own — the marker decision stands.
#[test]
fn unreadable_sidecar_falls_back_to_marker() {
    use crate::signed_policy::SignedVerdict;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // Served artifact INTACT: the same marker refuses under NoAuthenticSidecar
    // (pinned below) but must ALLOW under a mere read blip.
    std::fs::write(home.join("requirements.toml"), "[features]\n").unwrap();
    let served_fail_closed = ManagedConfigCache {
        principal: Some("team-007".into()),
        had_requirements: true,
        fail_closed: true,
        ..Default::default()
    };
    assert!(
        !managed_policy_compromised_decision(
            SignedVerdict::SidecarUnreadable,
            false,
            Some(&served_fail_closed),
            home,
            &team("team-007")
        ),
        "a transient sidecar read blip must not refuse a session"
    );
    // Marker-grade tamper (served artifact missing on disk) still refuses.
    std::fs::remove_file(home.join("requirements.toml")).unwrap();
    assert!(managed_policy_compromised_decision(
        SignedVerdict::SidecarUnreadable,
        false,
        Some(&served_fail_closed),
        home,
        &team("team-007")
    ));
}

/// NoAuthenticSidecar under a fail-closed marker that recorded served policy refuses —
/// stripping the sidecar must not downgrade enforcement to the forgeable marker path.
/// A marker that served nothing, never opted in, or is absent keeps the marker decision.
#[test]
fn missing_sidecar_under_fail_closed_marker_refuses() {
    use crate::signed_policy::SignedVerdict;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // The served artifact is INTACT on disk — the marker path alone would allow.
    std::fs::write(home.join("requirements.toml"), "[features]\n").unwrap();
    let served_fail_closed = ManagedConfigCache {
        principal: Some("team-007".into()),
        had_requirements: true,
        fail_closed: true,
        ..Default::default()
    };
    assert!(
        managed_policy_compromised_decision(
            SignedVerdict::NoAuthenticSidecar,
            false,
            Some(&served_fail_closed),
            home,
            &team("team-007")
        ),
        "a fail-closed marker with served policy requires an authentic sidecar"
    );
    // Served nothing → nothing the sidecar must cover → marker decision (allows).
    let served_nothing = ManagedConfigCache {
        principal: Some("team-007".into()),
        fail_closed: true,
        ..Default::default()
    };
    assert!(!managed_policy_compromised_decision(
        SignedVerdict::NoAuthenticSidecar,
        false,
        Some(&served_nothing),
        home,
        &team("team-007")
    ));
    // Never opted in → marker decision (allows).
    let opted_out = ManagedConfigCache {
        principal: Some("team-007".into()),
        had_requirements: true,
        fail_closed: false,
        ..Default::default()
    };
    assert!(!managed_policy_compromised_decision(
        SignedVerdict::NoAuthenticSidecar,
        false,
        Some(&opted_out),
        home,
        &team("team-007")
    ));
    // No marker at all → nothing to enforce.
    assert!(!managed_policy_compromised_decision(
        SignedVerdict::NoAuthenticSidecar,
        false,
        None,
        home,
        &team("team-007")
    ));
}

/// The dark build (`Inactive`) falls through to the best-effort marker: opted-in +
/// a served artifact now missing refuses; opted-out or no marker proceeds.
#[test]
fn inactive_verdict_falls_through_to_marker() {
    use crate::signed_policy::SignedVerdict;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // Opted-in, recorded a requirements artifact, which is absent on disk → compromised.
    let missing = ManagedConfigCache {
        principal: Some("team-007".into()),
        had_requirements: true,
        fail_closed: true,
        ..Default::default()
    };
    assert!(managed_policy_compromised_decision(
        SignedVerdict::Inactive,
        false,
        Some(&missing),
        home,
        &team("team-007")
    ));
    // Opted-OUT marker → never refuses, even with a missing artifact.
    let optout = ManagedConfigCache {
        principal: Some("team-007".into()),
        had_requirements: true,
        fail_closed: false,
        ..Default::default()
    };
    assert!(!managed_policy_compromised_decision(
        SignedVerdict::Inactive,
        false,
        Some(&optout),
        home,
        &team("team-007")
    ));
    // No marker at all → nothing to enforce.
    assert!(!managed_policy_compromised_decision(
        SignedVerdict::Inactive,
        false,
        None,
        home,
        &team("team-007")
    ));
}

#[test]
fn managed_config_stale_at_is_false_without_user_home() {
    // No user home => nothing to refresh into => not stale (prevents a
    // perpetual sync loop).
    assert!(!managed_config_stale_at(None, &ServingIdentity::None));
}

#[test]
fn managed_config_stale_at_is_true_without_synced_marker() {
    let dir = std::env::temp_dir().join(format!("grok-stale-nomark-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let _ = std::fs::remove_file(dir.join(MANAGED_CONFIG_CACHE_FILE));
    // No recorded sync (even if config files exist) => stale.
    assert!(managed_config_stale_at(Some(&dir), &ServingIdentity::None));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn managed_config_stale_at_is_false_after_fresh_sync() {
    let dir = std::env::temp_dir().join(format!("grok-stale-fresh-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: None,
            had_managed_config: false,
            had_requirements: false,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    // A just-recorded sync is within the default 30-minute threshold.
    assert!(!managed_config_stale_at(Some(&dir), &ServingIdentity::None));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn managed_deployment_id_at_requires_matching_fingerprint() {
    let dir = std::env::temp_dir().join(format!("grok-dep-id-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let server_dep = "37c96487-eda9-4bb2-a767-6444274423c8";
    // Deploy-key path: fingerprint set, principal = server deployment UUID.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some(server_dep),
            had_managed_config: true,
            had_requirements: false,
            key_fingerprint: Some("fp-abc"),
            fail_closed: false,
        },
    );
    assert_eq!(
        super::managed_deployment_id_at(&dir, "fp-abc").as_deref(),
        Some(server_dep)
    );
    // Rotated key: recorded fingerprint no longer matches — stale principal must not leak.
    assert_eq!(super::managed_deployment_id_at(&dir, "fp-rotated"), None);
    assert_eq!(super::managed_deployment_id_at(&dir, ""), None);
    // Team path: fingerprint absent, principal is a team id — not a deployment UUID.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-xyz"),
            had_managed_config: true,
            had_requirements: false,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    assert_eq!(super::managed_deployment_id_at(&dir, "fp-abc"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn managed_config_stale_at_is_true_for_old_sync() {
    let dir = std::env::temp_dir().join(format!("grok-stale-old-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let hour_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 60 * 60;
    std::fs::write(
        dir.join(MANAGED_CONFIG_CACHE_FILE),
        format!("{{\"synced_at\":{hour_ago}}}"),
    )
    .unwrap();
    // An hour-old sync exceeds the default 30-minute threshold.
    assert!(managed_config_stale_at(Some(&dir), &ServingIdentity::None));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A served-then-deleted artifact reads stale regardless of the timer.
#[test]
fn managed_config_stale_when_served_artifact_deleted() {
    let dir = std::env::temp_dir().join(format!("grok-stale-artgone-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-1"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();
    assert!(!managed_config_stale_at(Some(&dir), &team("team-1")));
    std::fs::remove_file(dir.join("requirements.toml")).unwrap();
    assert!(managed_config_stale_at(Some(&dir), &team("team-1")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A config-less principal that served nothing is never misread as stale.
#[test]
fn managed_config_not_stale_when_nothing_served() {
    let dir = std::env::temp_dir().join(format!("grok-stale-noart-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-1"),
            had_managed_config: false,
            had_requirements: false,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    assert!(!managed_config_stale_at(Some(&dir), &team("team-1")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cache fetched for a different principal is stale for the current one.
#[test]
fn managed_config_stale_on_identity_mismatch() {
    let dir = std::env::temp_dir().join(format!("grok-stale-ident-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-a"),
            had_managed_config: false,
            had_requirements: false,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    // Same identity => fresh; different identity => stale.
    assert!(!managed_config_stale_at(Some(&dir), &team("team-a")));
    assert!(managed_config_stale_at(Some(&dir), &team("team-b")));
    // Unknown current identity (None) never forces a refetch on identity.
    assert!(!managed_config_stale_at(Some(&dir), &ServingIdentity::None));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Legacy marker (no `had_*`) is never flagged missing-artifact-stale.
#[test]
fn managed_config_legacy_marker_is_conservative() {
    let dir = std::env::temp_dir().join(format!("grok-stale-legacy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        dir.join(MANAGED_CONFIG_CACHE_FILE),
        format!("{{\"synced_at\":{now}}}"),
    )
    .unwrap();
    assert!(!managed_config_stale_at(Some(&dir), &ServingIdentity::None));
    // A legacy marker (no principal) reads stale once via identity mismatch, so it self-upgrades next sync.
    assert!(managed_config_stale_at(Some(&dir), &team("team-x")));
    assert!(is_managed_config_hard_stale_for_at(&dir, &team("team-x")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hard-staleness: missing artifact or identity mismatch → true; a fresh same-identity cache → false.
#[test]
fn hard_stale_only_on_missing_or_identity() {
    let dir = std::env::temp_dir().join(format!("grok-hardstale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-a"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();
    // Same identity, artifact present → not hard-stale (even past the timer).
    assert!(!is_managed_config_hard_stale_for_at(&dir, &team("team-a")));
    // Different identity → hard-stale.
    assert!(is_managed_config_hard_stale_for_at(&dir, &team("team-b")));
    // Served artifact deleted → hard-stale.
    std::fs::remove_file(dir.join("requirements.toml")).unwrap();
    assert!(is_managed_config_hard_stale_for_at(&dir, &team("team-a")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// No marker → hard-stale (never synced → fetch before use).
#[test]
fn hard_stale_without_marker() {
    let dir = std::env::temp_dir().join(format!("grok-hardstale-nomark-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let _ = std::fs::remove_file(dir.join(MANAGED_CONFIG_CACHE_FILE));
    assert!(is_managed_config_hard_stale_for_at(&dir, &team("team-a")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A corrupt marker reads as "no marker": the gate ALLOWS (corruption or a torn write
/// must not lock a managed user out) and the cache is hard-stale so the next sync rewrites it.
#[test]
fn corrupt_marker_reads_as_no_marker_and_allows() {
    let dir = std::env::temp_dir().join(format!("grok-corrupt-marker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("requirements.toml"), "fail_closed = true\n").unwrap();
    std::fs::write(dir.join(MANAGED_CONFIG_CACHE_FILE), "{ not valid json").unwrap();

    assert!(read_managed_config_cache(&dir).is_none());
    // No usable marker → not compromised, so corruption can't lock a managed user out...
    assert!(!managed_policy_compromised_for_at(&dir, &team("team-a")));
    // ...but the cache reads hard-stale, so the next sync refetches and rewrites the marker.
    assert!(is_managed_config_hard_stale_for_at(&dir, &team("team-a")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A deploy-key switch is detected offline as an identity mismatch (`cache_unusable_for`) and refetched online.
#[test]
fn deployment_key_switch_is_stale_and_tampered_offline() {
    let dir = std::env::temp_dir().join(format!("grok-dk-switch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Provisioned with key A: principal = served deployment_id, fingerprint = fp-a.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("dep-A"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: Some("fp-a"),
            fail_closed: false,
        },
    );
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();

    // Same key, artifacts intact → not stale, not tampered.
    assert!(!is_managed_config_hard_stale_for_at(&dir, &dkey("fp-a")));
    assert!(!managed_config_stale_at(Some(&dir), &dkey("fp-a")));

    // Different key (fp-b) → identity mismatch: hard-stale and tampered.
    assert!(is_managed_config_hard_stale_for_at(&dir, &dkey("fp-b")));
    assert!(managed_config_stale_at(Some(&dir), &dkey("fp-b")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A pre-upgrade marker (no `key_fingerprint`) must not fire when a key is now configured — it self-upgrades next sync.
#[test]
fn pre_upgrade_marker_without_fingerprint_does_not_fire_on_key() {
    let dir = std::env::temp_dir().join(format!("grok-dk-preupgrade-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Legacy marker: synced, an artifact served, but no key_fingerprint field.
    std::fs::write(
        dir.join(MANAGED_CONFIG_CACHE_FILE),
        format!("{{\"synced_at\":{now},\"had_requirements\":true}}"),
    )
    .unwrap();
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();
    // A key is configured now but the marker has none → no key mismatch, no spurious refetch.
    assert!(!is_managed_config_hard_stale_for_at(
        &dir,
        &dkey("fp-current")
    ));
    assert!(!managed_config_stale_at(Some(&dir), &dkey("fp-current")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The team path keys on `principal` (team id), records no fingerprint, and never fires a key mismatch.
#[test]
fn team_path_keys_on_principal_not_key_fingerprint() {
    let dir = std::env::temp_dir().join(format!("grok-team-nofp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-a"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();
    // Team path: identity carries no fingerprint → never a key mismatch.
    assert!(!is_managed_config_hard_stale_for_at(&dir, &team("team-a")));
    // A team switch is still detected via principal (unchanged behavior).
    assert!(is_managed_config_hard_stale_for_at(&dir, &team("team-b")));
    // No key fingerprint is recorded on the team path.
    let marker = std::fs::read_to_string(dir.join(MANAGED_CONFIG_CACHE_FILE)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert!(
        v["key_fingerprint"].is_null(),
        "team path must not record a key fingerprint: {marker}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The eviction trigger fires only on a confirmed switch; first sync, same identity, `None`, and pre-upgrade markers never fire.
#[test]
fn identity_changed_only_on_confirmed_switch() {
    let dir = std::env::temp_dir().join(format!("grok-ident-changed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // No marker yet → first sync, nothing to evict.
    assert!(!managed_config_identity_changed_at(
        &dir,
        Some("team-a"),
        None
    ));

    // Team marker: same team → no switch; different team → switch; unknown (None) → never evicts.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-a"),
            had_managed_config: true,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    assert!(!managed_config_identity_changed_at(
        &dir,
        Some("team-a"),
        None
    ));
    assert!(managed_config_identity_changed_at(
        &dir,
        Some("team-b"),
        None
    ));
    assert!(!managed_config_identity_changed_at(&dir, None, None));

    // Deploy-key marker: same fingerprint → no switch; changed → switch (even if only the fingerprint differs).
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("dep-a"),
            had_managed_config: true,
            had_requirements: true,
            key_fingerprint: Some("fp-a"),
            fail_closed: false,
        },
    );
    assert!(!managed_config_identity_changed_at(
        &dir,
        Some("dep-a"),
        Some("fp-a")
    ));
    assert!(managed_config_identity_changed_at(
        &dir,
        Some("dep-b"),
        Some("fp-b")
    ));
    assert!(managed_config_identity_changed_at(&dir, None, Some("fp-b")));

    // Pre-upgrade team marker (no fingerprint) + a now-configured key → not a switch (key dimension has no recorded side).
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-a"),
            had_managed_config: true,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    assert!(!managed_config_identity_changed_at(
        &dir,
        Some("team-a"),
        Some("fp-current")
    ));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A blank/whitespace value is "unknown", never a distinct identity — on EITHER side of EITHER
/// dimension (principal or key fingerprint): a malformed `auth.json` or corrupt marker must not
/// make the gate purge / apply eviction shed a real tenant's policy.
#[test]
fn blank_principal_is_never_a_confirmed_switch() {
    let dir = std::env::temp_dir().join(format!("grok-ident-blank-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Real recorded team, blank current → not a switch.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-a"),
            had_managed_config: true,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    for blank in ["", "   "] {
        assert!(
            !managed_config_identity_changed_at(&dir, Some(blank), None),
            "a blank current principal ({blank:?}) must not read as a confirmed switch"
        );
    }

    // Blank recorded principal, real current → not a switch either.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("  "),
            had_managed_config: true,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    assert!(
        !managed_config_identity_changed_at(&dir, Some("team-b"), None),
        "a blank recorded principal must not read as a distinct identity"
    );

    // Fingerprint dimension, same symmetry: a blank side never confirms; two real,
    // differing fingerprints still do.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("dep-a"),
            had_managed_config: true,
            had_requirements: true,
            key_fingerprint: Some("  "),
            fail_closed: false,
        },
    );
    assert!(
        !managed_config_identity_changed_at(&dir, Some("dep-a"), Some("fp-b")),
        "a blank recorded fingerprint must not read as a distinct identity"
    );
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("dep-a"),
            had_managed_config: true,
            had_requirements: true,
            key_fingerprint: Some("fp-a"),
            fail_closed: false,
        },
    );
    assert!(
        !managed_config_identity_changed_at(&dir, Some("dep-a"), Some("")),
        "a blank current fingerprint must not read as a confirmed switch"
    );
    assert!(
        managed_config_identity_changed_at(&dir, Some("dep-a"), Some("fp-b")),
        "two real, differing fingerprints must still confirm a switch"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Compromised only when opted in AND tampered; opted-out / never-synced / config-less / intact is never flagged.
#[test]
fn compromised_only_when_opted_in_and_deleted_or_substituted() {
    let dir = std::env::temp_dir().join(format!("grok-compromised-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // No marker → not compromised.
    let _ = std::fs::remove_file(dir.join(MANAGED_CONFIG_CACHE_FILE));
    assert!(!managed_policy_compromised_for_at(&dir, &team("team-a")));

    // Opted-in + present + same identity → not compromised.
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-a"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: true,
        },
    );
    assert!(!managed_policy_compromised_for_at(&dir, &team("team-a")));

    // Served-then-deleted (admin opted in) → compromised.
    std::fs::remove_file(dir.join("requirements.toml")).unwrap();
    assert!(managed_policy_compromised_for_at(&dir, &team("team-a")));

    // Different principal, artifact still missing → compromised by the artifact, not the identity.
    assert!(managed_policy_compromised_for_at(&dir, &team("team-b")));

    // Not opted in → a deletion is NOT failed closed.
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-a"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    std::fs::remove_file(dir.join("requirements.toml")).unwrap();
    assert!(!managed_policy_compromised_for_at(&dir, &team("team-a")));

    // Config-less principal (nothing served) → never compromised.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-c"),
            had_managed_config: false,
            had_requirements: false,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    assert!(!managed_policy_compromised_for_at(&dir, &team("team-c")));

    let _ = std::fs::remove_dir_all(&dir);
}

/// The OTHER served artifact — `managed_config.toml`, not just `requirements.toml` —
/// deleted under a fail_closed marker is compromised.
#[test]
fn compromised_on_managed_config_deletion_when_fail_closed() {
    let dir = std::env::temp_dir().join(format!("grok-compromised-mc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("managed_config.toml"), "[cli]\n").unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-a"),
            had_managed_config: true,
            had_requirements: false,
            key_fingerprint: None,
            fail_closed: true,
        },
    );
    // Present → not compromised.
    assert!(!managed_policy_compromised_for_at(&dir, &team("team-a")));
    // Served-then-deleted managed_config.toml → compromised by the missing artifact.
    std::fs::remove_file(dir.join("managed_config.toml")).unwrap();
    assert!(managed_policy_compromised_for_at(&dir, &team("team-a")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Deployment-key path: an opted-in marker is compromised on an offline key switch (the fingerprint is the only offline identity).
#[test]
fn compromised_on_deployment_key_switch_when_fail_closed() {
    let dir = std::env::temp_dir().join(format!("grok-compromised-dk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Provisioned with key A (fp-a), opted into fail_closed, artifact present.
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("dep-A"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: Some("fp-a"),
            fail_closed: true,
        },
    );

    // Same key, offline → not compromised (ALLOW).
    assert!(!managed_policy_compromised_for_at(&dir, &dkey("fp-a")));

    // Different key (fp-b) → compromised (REFUSE): offline deploy-key switch.
    assert!(managed_policy_compromised_for_at(&dir, &dkey("fp-b")));

    // Not opted in (fail_closed=false): a key switch is NOT failed closed.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("dep-A"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: Some("fp-a"),
            fail_closed: false,
        },
    );
    assert!(!managed_policy_compromised_for_at(&dir, &dkey("fp-b")));

    let _ = std::fs::remove_dir_all(&dir);
}

/// The gate refuses ONLY on tamper for the CURRENT principal (artifact missing or changed key fingerprint),
/// never a pure identity mismatch; staleness still treats that mismatch as a refetch trigger (asserted alongside).
#[test]
fn gate_excludes_pure_identity_mismatch_but_keeps_artifact_and_key_tamper() {
    let dir = std::env::temp_dir().join(format!("grok-gate-fix1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // (1) Principal A (fail_closed), artifact intact; serving team-b = pure identity mismatch → ALLOWED.
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("dep-A"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: true,
        },
    );
    assert!(
        !managed_policy_compromised_for_at(&dir, &team("team-b")),
        "a foreign/stale principal's fail_closed must NOT refuse the current session"
    );
    // ...but still stale for B → the refetch path rebinds online.
    assert!(
        is_managed_config_hard_stale_for_at(&dir, &team("team-b")),
        "a pure identity mismatch must still trigger a refetch (rebind)"
    );

    // (2) Same principal, served artifact now missing → still REFUSED offline.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-b"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: true,
        },
    );
    std::fs::remove_file(dir.join("requirements.toml")).unwrap();
    assert!(
        managed_policy_compromised_for_at(&dir, &team("team-b")),
        "same-principal served-then-deleted artifact must fail closed offline"
    );

    // (3) Deploy-key fingerprint mismatch for the current key → still REFUSED.
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("dep-A"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: Some("fp-a"),
            fail_closed: true,
        },
    );
    assert!(
        managed_policy_compromised_for_at(&dir, &dkey("fp-b")),
        "a changed deployment-key fingerprint must fail closed offline"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Opt-in comes from the served response, not disk, so a no-write sync can't disarm the gate.
#[test]
fn mark_keeps_fail_closed_armed_without_on_disk_file() {
    let dir = std::env::temp_dir().join(format!("grok-mark-disarm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Opted-in policy served + present → not compromised (intact).
    std::fs::write(dir.join("requirements.toml"), "[features]\n").unwrap();
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-1"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: true,
        },
    );
    assert!(!managed_policy_compromised_for_at(&dir, &team("team-1")));

    // Tamper: delete the served file → compromised.
    std::fs::remove_file(dir.join("requirements.toml")).unwrap();
    assert!(managed_policy_compromised_for_at(&dir, &team("team-1")));

    // A no-write sync (file still absent) stays armed: opt-in is from the response.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-1"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: true,
        },
    );
    assert!(
        managed_policy_compromised_for_at(&dir, &team("team-1")),
        "a no-write sync must not disarm the fail-closed gate"
    );

    // An opt-OUT (fail_closed=false) still takes effect.
    mark_managed_config_synced_at(
        &dir,
        SyncMarker {
            principal: Some("team-1"),
            had_managed_config: false,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: false,
        },
    );
    assert!(!managed_policy_compromised_for_at(&dir, &team("team-1")));

    let _ = std::fs::remove_dir_all(&dir);
}

/// The gate's apply-race retry: a Compromised refusal that clears on the second
/// evaluation (an in-flight apply settled) allows; real tamper stays refused; and
/// non-Compromised refusals never retry.
#[test]
fn gate_retries_once_on_a_compromised_verdict() {
    use crate::signed_policy::SignedVerdict;
    // Racing apply: mismatch on the first read, settled on the second → allowed.
    let mut calls = 0;
    let allowed = !compromised_with_apply_race_retry(
        || {
            calls += 1;
            match calls {
                1 => (true, SignedVerdict::Compromised),
                _ => (false, SignedVerdict::Trusted),
            }
        },
        || {},
    );
    assert!(allowed, "a settled racing write must not refuse");
    assert_eq!(calls, 2, "exactly one retry");

    // Real tamper: Compromised on both evaluations → still refused.
    assert!(compromised_with_apply_race_retry(
        || (true, SignedVerdict::Compromised),
        || {},
    ));

    // A non-Compromised refusal (e.g. a stripped sidecar) refuses without retrying.
    let mut evals = 0;
    let refused = compromised_with_apply_race_retry(
        || {
            evals += 1;
            (true, SignedVerdict::NoAuthenticSidecar)
        },
        || panic!("no pause for non-Compromised refusals"),
    );
    assert!(refused);
    assert_eq!(evals, 1);
}
