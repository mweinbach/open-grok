//! Extended KEYED managed-config scenarios. Harness + seam/serial constraints:
//! `signed_managed_config/common.rs`.
//!
//! Placement rule: new keyed scenarios land HERE; `signed_managed_config.rs`
//! stays fixed to the review-cited security claims (verify-persists /
//! reject-persists-nothing / sidecar-deletion-refuses).

#[path = "signed_managed_config/common.rs"]
mod common;

#[cfg(unix)]
use common::skip_as_root;
use common::{
    MANAGED, REQUIREMENTS_FAIL_CLOSED, TEST_EXPIRES_AT, TEST_KEY_ID, forged_team_body,
    install_test_key, reset, sign_envelope, signed_team_body, spawn_mock, team_identity, test_home,
    write_config, write_dk_config, write_team_auth,
};
use serial_test::serial;
use xai_grok_config::signed_policy::{self, SignedPayload};

/// The healthy fail-closed starting state the tamper/heal scenarios mutate;
/// the mock keeps serving the same body, so a healing sync can refetch it.
async fn sync_fail_closed_policy(home: &std::path::Path, kp: &ring::signature::Ed25519KeyPair) {
    let url = spawn_mock(signed_team_body(
        kp,
        "team-007",
        Some(MANAGED),
        Some(REQUIREMENTS_FAIL_CLOSED),
    ));
    write_config(home, &url);
    write_team_auth(home, "team-007");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("initial sync should succeed");
    assert!(xai_grok_shell::managed_config::managed_policy_gate().is_ok());
}

/// The signed-empty deployment response: a `{}` body (no legacy fields) whose
/// envelope binds ABSENCE to `deployment_id` — what the server serves for a
/// provisioned key with no config row.
fn signed_dk_empty_body(kp: &ring::signature::Ed25519KeyPair, deployment_id: &str) -> String {
    let payload = SignedPayload {
        version: prod_mc_cli_chat_proxy_types::SIGNED_PAYLOAD_VERSION,
        deployment_id: Some(deployment_id.to_owned()),
        team_id: None,
        managed_config: None,
        requirements: None,
        fail_closed: false,
        expires_at: TEST_EXPIRES_AT,
        key_id: TEST_KEY_ID.into(),
    };
    serde_json::json!({ "signatures": [sign_envelope(kp, &payload)] }).to_string()
}

/// The marker principal for an applied signed-EMPTY dk response comes from the
/// VERIFIED payload's deployment_id (the `{}` body carries none), so the gate's
/// cross-tenant binding holds even on an unprovisioned dk machine.
#[tokio::test]
#[serial]
async fn empty_dk_response_marker_binds_the_verified_deployment_id() {
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    let url = spawn_mock(signed_dk_empty_body(&kp, "dep-42"));
    write_dk_config(&home, &url, "dep-key-1");
    // No team auth: the empty dk body is applied (converges), not fallen through.

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("signed-empty dk sync should succeed");
    assert!(!wrote, "nothing to write for an empty row");

    let marker = std::fs::read_to_string(home.join("managed_config_cache.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(
        v["principal"].as_str(),
        Some("dep-42"),
        "the marker must bind the VERIFIED deployment id: {marker}"
    );
    assert!(
        home.join("managed_config.sig.json").exists(),
        "the absence envelope is persisted"
    );
    assert!(xai_grok_shell::managed_config::managed_policy_gate().is_ok());
}

/// A signature-rejected sync surfaces as failure in BOTH `grok setup` and the
/// post-login sync — never as Installed/NoChange while nothing was persisted.
#[tokio::test]
#[serial]
async fn rejected_signature_surfaces_as_setup_and_login_failure() {
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    let url = spawn_mock(forged_team_body(&kp, "team-007"));
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let outcome = xai_grok_shell::managed_config::run_setup().await;
    assert!(
        matches!(
            outcome,
            xai_grok_shell::managed_config::SetupOutcome::Failed(
                xai_grok_shell::managed_config::ManagedConfigError::SignatureRejected
            )
        ),
        "setup must surface the signature rejection, got {outcome:?}"
    );

    let login = xai_grok_shell::managed_config::post_login_sync(None).await;
    assert_eq!(
        login,
        xai_grok_shell::managed_config::ManagedConfigSync::Failed,
        "post-login sync must report Failed, not NoChange"
    );
}

/// A response that stops serving requirements deletes the on-disk file, and the
/// NEW sidecar (written after the deletion) covers the absence — the converged cache
/// reads fresh and the gate allows.
#[tokio::test]
#[serial]
async fn withdrawn_requirements_is_deleted_and_covered_by_the_new_sidecar() {
    let home = test_home().clone();
    reset(&home);
    let (kp, pubkey) = install_test_key();

    sync_fail_closed_policy(&home, &kp).await;
    assert!(home.join("requirements.toml").exists());

    let url_partial = spawn_mock(signed_team_body(&kp, "team-007", Some(MANAGED), None));
    write_config(&home, &url_partial);
    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("withdrawing sync should succeed");
    assert!(wrote, "the deletion is a change");

    assert!(
        !home.join("requirements.toml").exists(),
        "the withdrawn artifact is removed"
    );
    let sidecar: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("managed_config.sig.json")).unwrap(),
    )
    .unwrap();
    let payload = signed_policy::verify_signed_payload(
        sidecar["signed_payload"].as_str().unwrap(),
        sidecar["signature"].as_str().unwrap(),
        &[(TEST_KEY_ID, &pubkey)],
    )
    .expect("the refreshed sidecar must verify");
    assert!(
        payload.requirements.is_none(),
        "the new sidecar covers the absence"
    );
    assert!(
        !xai_grok_shell::config::is_managed_config_hard_stale_for(&team_identity("team-007")),
        "the converged, covered cache is not hard-stale"
    );
    assert!(xai_grok_shell::managed_config::managed_policy_gate().is_ok());
}

/// A directory squatting at a signed artifact path reads COMPROMISED at the gate
/// (not lenient-unreadable), and an online sync converges over it — clearing the
/// directory, rewriting the file, and restoring enforcement.
#[tokio::test]
#[serial]
async fn directory_squat_reads_compromised_and_online_sync_heals() {
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    sync_fail_closed_policy(&home, &kp).await;

    // Dir-squat the enforced artifact (with a child, like a real squat).
    std::fs::remove_file(home.join("requirements.toml")).unwrap();
    std::fs::create_dir(home.join("requirements.toml")).unwrap();
    std::fs::write(home.join("requirements.toml").join("junk"), "x").unwrap();

    let gate = xai_grok_shell::managed_config::managed_policy_gate();
    assert!(
        gate.is_err(),
        "a directory squat on a fail-closed policy must refuse offline"
    );
    // The gate verdict, not an incidental error; classification is unit-pinned
    // in signed_policy::directory_squat_is_tamper_not_unreadable.
    assert!(
        gate.unwrap_err()
            .contains("Managed policy is required for this account"),
        "the refusal is the managed-policy gate message"
    );
    assert!(
        xai_grok_shell::config::is_managed_config_hard_stale_for(&team_identity("team-007")),
        "the squat must trigger the refetch"
    );

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("healing sync should succeed");
    assert!(wrote, "the healing sync must rewrite the squatted artifact");
    assert_eq!(
        std::fs::read_to_string(home.join("requirements.toml")).unwrap(),
        REQUIREMENTS_FAIL_CLOSED,
        "the served file replaces the squatting directory"
    );
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "enforcement is restored after the heal"
    );
}

/// A sidecar read blip (chmod 000) is not tamper: the gate allows while the
/// refetch trigger fires — mirroring the artifact-slot blip semantics.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn sidecar_read_blip_allows_session_and_triggers_refetch() {
    use std::os::unix::fs::PermissionsExt;
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    sync_fail_closed_policy(&home, &kp).await;

    let sidecar_path = home.join("managed_config.sig.json");
    std::fs::set_permissions(&sidecar_path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if skip_as_root(
        &sidecar_path,
        "sidecar_read_blip_allows_session_and_triggers_refetch",
    ) {
        let _ = std::fs::set_permissions(&sidecar_path, std::fs::Permissions::from_mode(0o600));
        return;
    }

    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "a transient sidecar read blip must not refuse the session"
    );
    assert!(
        xai_grok_shell::config::is_managed_config_hard_stale_for(&team_identity("team-007")),
        "the blip must trigger the refetch so the self-heal runs"
    );
    // Restore so the tempdir (and later tests) stay clean.
    std::fs::set_permissions(&sidecar_path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// A directory squatting at the SIDECAR path refuses at the gate, and the online
/// sync clears it — a bare rename would error forever.
#[tokio::test]
#[serial]
async fn sidecar_directory_squat_refuses_then_online_sync_heals() {
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    sync_fail_closed_policy(&home, &kp).await;

    // Dir-squat the sidecar (with a child, like a real squat).
    let sidecar_path = home.join("managed_config.sig.json");
    std::fs::remove_file(&sidecar_path).unwrap();
    std::fs::create_dir(&sidecar_path).unwrap();
    std::fs::write(sidecar_path.join("junk"), "x").unwrap();

    let gate = xai_grok_shell::managed_config::managed_policy_gate();
    assert!(
        gate.is_err(),
        "an unreadable (squatted) sidecar under a fail-closed marker must refuse offline"
    );
    // The gate verdict, not an incidental error; classification is unit-pinned
    // in signed_policy::sidecar_directory_squat_is_absence_not_a_blip.
    assert!(
        gate.unwrap_err()
            .contains("Managed policy is required for this account"),
        "the refusal is the managed-policy gate message"
    );
    assert!(
        xai_grok_shell::config::is_managed_config_hard_stale_for(&team_identity("team-007")),
        "the squat must trigger the refetch"
    );

    xai_grok_shell::managed_config::sync()
        .await
        .expect("healing sync should succeed");
    assert!(
        sidecar_path.is_file(),
        "the rewrite must replace the squatting directory with a sidecar FILE"
    );
    // Under a fail-closed marker the gate requires an authentic sidecar, so
    // allowing here also pins that the healed sidecar verifies.
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "enforcement is restored after the heal"
    );
}
