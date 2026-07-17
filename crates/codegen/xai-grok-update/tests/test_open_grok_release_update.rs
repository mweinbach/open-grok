#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod common;

use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;

use serial_test::serial;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use xai_grok_update::auto_update::{
    check_update_status, install_open_grok_release_from_base, run_update,
};

use common::{make_update_config, reset_home, set_test_version, test_home};

const OLD_VERSION: &str = "0.1.220-open-grok.3";
const NEW_VERSION: &str = "0.1.220-open-grok.9";
const ASSET: &str = "open-grok-macos-aarch64";

fn executable(version: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = \"--version\" ]; then\n  echo 'open-grok {version}'\n  exit 0\nfi\nexit 0\n"
    )
    .into_bytes()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

async fn mount_release(server: &MockServer, version: &str, bytes: &[u8], digest: &str) {
    Mock::given(method("GET"))
        .and(path("/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "tag_name": format!("v{version}"),
            "draft": false,
            "prerelease": false
        })))
        .mount(server)
        .await;
    let asset_path = format!("/download/v{version}/{ASSET}");
    Mock::given(method("GET"))
        .and(path(asset_path.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{asset_path}.sha256")))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{digest}  {ASSET}\n")))
        .mount(server)
        .await;
}

#[tokio::test]
#[serial]
async fn status_accepts_open_grok_release_tags_as_stable_updates() {
    let _ = test_home();
    reset_home();
    set_test_version(OLD_VERSION);
    let server = MockServer::start().await;
    let bytes = executable(NEW_VERSION);
    mount_release(&server, NEW_VERSION, &bytes, &sha256(&bytes)).await;

    let mut config = make_update_config("stable");
    config.release_api_url = format!("{}/latest", server.uri());
    config.release_download_base_url = server.uri();
    let status = check_update_status(&config).await;

    assert_eq!(status.current_version, OLD_VERSION);
    assert_eq!(status.latest_version.as_deref(), Some(NEW_VERSION));
    assert!(status.update_available);
    assert_eq!(status.installer.as_deref(), Some("open-grok"));
    assert!(
        status.error.is_none(),
        "unexpected error: {:?}",
        status.error
    );
}

#[tokio::test]
#[serial]
async fn old_regular_file_install_updates_via_verified_release_and_atomic_link() {
    let home = test_home();
    reset_home();
    set_test_version(OLD_VERSION);
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let managed = bin_dir.join("open-grok");
    std::fs::write(&managed, executable(OLD_VERSION)).unwrap();
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        !std::fs::symlink_metadata(&managed)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let server = MockServer::start().await;
    let bytes = executable(NEW_VERSION);
    mount_release(&server, NEW_VERSION, &bytes, &sha256(&bytes)).await;
    let mut config = make_update_config("stable");
    config.release_api_url = format!("{}/latest", server.uri());
    config.release_download_base_url = server.uri();

    let installed = run_update(false, None, None, &mut config).await.unwrap();
    assert_eq!(installed.as_deref(), Some(NEW_VERSION));
    let metadata = std::fs::symlink_metadata(&managed).unwrap();
    assert!(
        metadata.file_type().is_symlink(),
        "activation must be an atomic link swap"
    );
    let target = std::fs::read_link(&managed).unwrap();
    assert!(
        target.to_string_lossy().contains(NEW_VERSION),
        "target={target:?}"
    );
    let output = std::process::Command::new(&managed)
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(NEW_VERSION));
    assert!(
        home.join("downloads")
            .join(format!("open-grok-{OLD_VERSION}-macos-aarch64"))
            .exists(),
        "the prior executing inode must remain available after migration"
    );
}

#[tokio::test]
#[serial]
async fn force_reinstall_same_version_preserves_the_active_artifact_inode() {
    let home = test_home();
    reset_home();
    set_test_version(NEW_VERSION);
    let server = MockServer::start().await;
    let bytes = executable(NEW_VERSION);
    mount_release(&server, NEW_VERSION, &bytes, &sha256(&bytes)).await;
    let mut config = make_update_config("stable");
    config.release_api_url = format!("{}/latest", server.uri());
    config.release_download_base_url = server.uri();

    install_open_grok_release_from_base(NEW_VERSION, &server.uri())
        .await
        .unwrap();
    let managed = home.join("bin/open-grok");
    let first_target = std::fs::canonicalize(&managed).unwrap();
    let first_inode = std::fs::metadata(&first_target).unwrap().ino();

    let installed = run_update(true, None, None, &mut config).await.unwrap();
    assert_eq!(installed.as_deref(), Some(NEW_VERSION));
    let second_target = std::fs::canonicalize(&managed).unwrap();
    assert_ne!(
        second_target, first_target,
        "force reinstall must activate a unique artifact"
    );
    assert!(
        first_target.exists(),
        "the artifact used by the running process must remain linked"
    );
    assert_eq!(std::fs::metadata(&first_target).unwrap().ino(), first_inode);
    assert!(
        String::from_utf8_lossy(
            &std::process::Command::new(&managed)
                .arg("--version")
                .output()
                .unwrap()
                .stdout
        )
        .contains(NEW_VERSION)
    );

    let status = check_update_status(&config).await;
    assert!(
        !status.update_available,
        "the same Open Grok release must not re-trigger stable-channel updates"
    );
}

#[tokio::test]
#[serial]
async fn version_smoke_test_rejects_a_prefix_match() {
    let _ = test_home();
    reset_home();
    set_test_version(OLD_VERSION);
    let server = MockServer::start().await;
    let reported = "0.1.220-open-grok.30";
    let bytes = executable(reported);
    mount_release(&server, OLD_VERSION, &bytes, &sha256(&bytes)).await;

    let error = install_open_grok_release_from_base(OLD_VERSION, &server.uri())
        .await
        .expect_err("a reported semver that only prefixes the target must fail");
    let message = format!("{error:#}");
    assert!(message.contains(reported), "message={message}");
    assert!(message.contains(OLD_VERSION), "message={message}");
    assert!(!test_home().join("bin/open-grok").exists());
}

#[tokio::test]
#[serial]
async fn checksum_failure_keeps_existing_command_unchanged() {
    let home = test_home();
    reset_home();
    set_test_version(OLD_VERSION);
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let managed = bin_dir.join("open-grok");
    let old_bytes = executable(OLD_VERSION);
    std::fs::write(&managed, &old_bytes).unwrap();
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o755)).unwrap();

    let server = MockServer::start().await;
    let new_bytes = executable(NEW_VERSION);
    mount_release(&server, NEW_VERSION, &new_bytes, &"0".repeat(64)).await;
    let error = install_open_grok_release_from_base(NEW_VERSION, &server.uri())
        .await
        .expect_err("bad checksum must fail closed");

    assert!(format!("{error:#}").contains("SHA-256 verification failed"));
    assert_eq!(std::fs::read(&managed).unwrap(), old_bytes);
    assert!(
        !std::fs::symlink_metadata(&managed)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
