//! Core end-to-end KEYED managed-config tests — verified persist, rejected
//! persist-nothing, and the stripped-sidecar refusal. The harness (and the
//! seam/serial constraints every test here must follow) lives in
//! `signed_managed_config/common.rs`.
//!
//! Placement rule: this binary pins the review-cited security claims
//! (verify-persists / reject-persists-nothing / sidecar-deletion-refuses); new
//! keyed scenarios go in `signed_managed_config_extended.rs` unless they alter
//! one of those three claims.

#[path = "signed_managed_config/common.rs"]
mod common;

use common::{
    MANAGED, REQUIREMENTS_FAIL_CLOSED, forged_team_body, install_test_key, reset, signed_team_body,
    spawn_mock, team_identity, test_home, write_config, write_team_auth,
};
use serial_test::serial;
use xai_grok_config::signed_policy;

/// A rejected envelope persists NOTHING: the prior principal's files survive
/// (verify-before-evict), no sidecar appears, and the marker is not rewritten.
#[tokio::test]
#[serial]
async fn rejected_signature_persists_nothing_and_records_no_marker() {
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    // Prior trusted state: team-b's files + marker (as if synced earlier).
    std::fs::write(home.join("managed_config.toml"), "[cli]\nprior = true\n").unwrap();
    std::fs::write(home.join("requirements.toml"), "[features]\n").unwrap();
    xai_grok_shell::config::mark_managed_config_synced(xai_grok_shell::config::SyncMarker {
        principal: Some("team-b"),
        had_managed_config: true,
        had_requirements: true,
        key_fingerprint: None,
        fail_closed: false,
    });

    let url = spawn_mock(forged_team_body(&kp, "team-007"));
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("a rejected signature is a no-op, not a transport error");
    assert!(!wrote, "nothing may be persisted for a rejected envelope");

    assert_eq!(
        std::fs::read_to_string(home.join("managed_config.toml")).unwrap(),
        "[cli]\nprior = true\n",
        "verify-before-evict: the prior policy must survive the identity switch"
    );
    assert!(home.join("requirements.toml").exists());
    assert!(
        !home.join("managed_config.sig.json").exists(),
        "no sidecar may be written for a rejected envelope"
    );
    let marker = std::fs::read_to_string(home.join("managed_config_cache.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(
        v["principal"].as_str(),
        Some("team-b"),
        "the marker must not be rewritten for a rejected fetch: {marker}"
    );
}

/// A good envelope persists the policy files AND a sidecar that verifies over the
/// exact on-disk bytes; the cache then reads fresh and the gate allows.
#[tokio::test]
#[serial]
async fn verified_envelope_persists_policy_and_sidecar() {
    let home = test_home().clone();
    reset(&home);
    let (kp, pubkey) = install_test_key();

    let url = spawn_mock(signed_team_body(
        &kp,
        "team-007",
        Some(MANAGED),
        Some(REQUIREMENTS_FAIL_CLOSED),
    ));
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("a verified sync should succeed");
    assert!(wrote);

    let on_disk_managed = std::fs::read_to_string(home.join("managed_config.toml")).unwrap();
    let on_disk_requirements = std::fs::read_to_string(home.join("requirements.toml")).unwrap();
    let sidecar: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("managed_config.sig.json")).unwrap(),
    )
    .unwrap();
    let payload = signed_policy::verify_signed_payload(
        sidecar["signed_payload"].as_str().unwrap(),
        sidecar["signature"].as_str().unwrap(),
        &[("v1", &pubkey)],
    )
    .expect("the persisted sidecar must verify");
    assert_eq!(
        payload.managed_config.as_deref(),
        Some(on_disk_managed.as_str()),
        "the sidecar covers the exact on-disk managed_config bytes"
    );
    assert_eq!(
        payload.requirements.as_deref(),
        Some(on_disk_requirements.as_str()),
        "the sidecar covers the exact on-disk requirements bytes"
    );
    assert!(payload.fail_closed, "the signed opt-in is carried");

    assert!(
        !xai_grok_shell::config::is_managed_config_hard_stale_for(&team_identity("team-007")),
        "a covered cache is not hard-stale"
    );
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "an intact verified policy must not be refused"
    );
}

/// Deleting the sidecar under a fail-closed marker REFUSES at the gate (stripping it
/// must not downgrade enforcement to the forgeable marker path); the refetch triggers
/// fire so an online start self-heals.
#[tokio::test]
#[serial]
async fn deleted_sidecar_under_fail_closed_marker_refuses_at_gate() {
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    let url = spawn_mock(signed_team_body(
        &kp,
        "team-007",
        Some(MANAGED),
        Some(REQUIREMENTS_FAIL_CLOSED),
    ));
    write_config(&home, &url);
    write_team_auth(&home, "team-007");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("initial sync should succeed");
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "the covered fail-closed policy is allowed"
    );

    std::fs::remove_file(home.join("managed_config.sig.json")).unwrap();

    assert!(
        xai_grok_shell::config::is_managed_config_hard_stale_for(&team_identity("team-007")),
        "a stripped sidecar must trigger the session-start refetch"
    );
    assert!(
        xai_grok_shell::config::is_managed_config_stale_for(&team_identity("team-007")),
        "the TIMER staleness sibling must fire too (background tick self-heal), even though the marker is timer-fresh"
    );
    let gate = xai_grok_shell::managed_config::managed_policy_gate();
    assert!(
        gate.is_err(),
        "a fail-closed policy without its sidecar must refuse offline"
    );
    assert!(
        gate.unwrap_err()
            .contains("Managed policy is required for this account"),
        "the refusal is the managed-policy gate message"
    );
}

/// The keyed availability fix: after a fail_closed team-A install (signed sidecar + marker), an
/// OFFLINE switch to team B previously read Compromised (the authentic sidecar is bound to A) and
/// refused a legitimate switch. The gate's identity-change purge must shed team A's artifacts
/// INCLUDING the sidecar, PERMIT team B, and leave the cache hard-stale so the next online start
/// fetches team B's own policy.
#[tokio::test]
#[serial]
async fn offline_team_switch_purges_sidecar_and_permits_new_team() {
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    let url = spawn_mock(signed_team_body(
        &kp,
        "team-a",
        Some(MANAGED),
        Some(REQUIREMENTS_FAIL_CLOSED),
    ));
    write_config(&home, &url);
    write_team_auth(&home, "team-a");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("team A keyed sync should succeed");
    assert!(
        home.join("managed_config.sig.json").exists(),
        "the keyed sync persists a sidecar"
    );
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "team A's verified fail_closed policy must start"
    );

    // Switch the signed-in team to B; the gate is sync, so no fetch can rebind first.
    write_team_auth(&home, "team-b");

    // The bug this fixes: without the purge, team B evaluates against team A's
    // foreign-bound sidecar → Compromised → a legitimate switch refused startup.
    assert!(
        xai_grok_shell::config::managed_policy_compromised_for(&team_identity("team-b")),
        "pre-purge, the foreign-bound sidecar must read compromised for team B"
    );

    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "the gate must purge team A and permit the legitimate offline switch to team B"
    );
    for f in [
        "requirements.toml",
        "managed_config.toml",
        "managed_config_cache.json",
        "managed_config.sig.json",
    ] {
        assert!(
            !home.join(f).exists(),
            "{f} must be purged on the identity change"
        );
    }
    assert!(
        xai_grok_shell::config::is_managed_config_hard_stale_for(&team_identity("team-b")),
        "the purged cache must read hard-stale so the next online start fetches team B's policy"
    );
}

/// A blank `team_id` in `auth.json` (a parse blip) over an authentic team-A-bound fail_closed
/// sidecar: the blank→None filter resolves the identity to None, the marker principal backstops
/// the signed binding (team-a vs team-a → Trusted), so the KEYED gate PERMITS — instead of
/// binding to "" and refusing as Compromised — and nothing is purged.
#[tokio::test]
#[serial]
async fn keyed_blank_team_id_is_not_refused_and_does_not_purge() {
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    let url = spawn_mock(signed_team_body(
        &kp,
        "team-a",
        Some(MANAGED),
        Some(REQUIREMENTS_FAIL_CLOSED),
    ));
    write_config(&home, &url);
    write_team_auth(&home, "team-a");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("team A keyed sync should succeed");
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "team A's verified fail_closed policy must start"
    );

    // auth.json now carries a team principal with a BLANK team_id.
    write_team_auth(&home, "");

    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "a blank team_id must read as unknown, not a foreign binding that reads compromised"
    );
    for f in [
        "requirements.toml",
        "managed_config.toml",
        "managed_config_cache.json",
        "managed_config.sig.json",
    ] {
        assert!(
            home.join(f).exists(),
            "{f} must be retained on a blank team_id (a parse blip is not an identity change)"
        );
    }
}
