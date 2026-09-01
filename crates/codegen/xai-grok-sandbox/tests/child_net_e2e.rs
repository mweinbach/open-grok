#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SOCKET_ENV: &str = "CHILD_NET_E2E_SOCKET";

#[test]
#[ignore]
fn subprocess_entry() {
    let Ok(path) = std::env::var(SOCKET_ENV) else {
        return;
    };
    match std::os::unix::net::UnixStream::connect(&path) {
        Ok(_) => std::process::exit(10),
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => std::process::exit(11),
        Err(e) => {
            eprintln!("unexpected connect error: {e}");
            std::process::exit(12);
        }
    }
}

fn spawn_probe(socket: &Path, filtered: bool) -> std::process::Output {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.env(SOCKET_ENV, socket)
        .args(["--ignored", "--exact", "--nocapture", "subprocess_entry"]);
    if filtered {
        let filter = xai_grok_sandbox::child_net::prebuilt_child_network_filter();
        unsafe {
            cmd.pre_exec(move || xai_grok_sandbox::child_net::install_child_network_filter(filter));
        }
    }
    cmd.output().expect("spawn probe")
}

fn unique_socket_path() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "grok-child-net-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create socket dir");
    dir.join("api.sock")
}

#[test]
fn filtered_child_cannot_connect_to_socket_created_after_launch() {
    let sock = unique_socket_path();
    let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");

    let denied = spawn_probe(&sock, true);
    assert_eq!(
        denied.status.code(),
        Some(11),
        "filtered child must get EPERM on connect; stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );

    let allowed = spawn_probe(&sock, false);
    assert_eq!(
        allowed.status.code(),
        Some(10),
        "unfiltered control child must connect; stderr: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );

    let _ = std::fs::remove_dir_all(sock.parent().unwrap());
}

#[test]
fn restrict_child_network_std_arms_from_config_without_apply() {
    let manager = xai_grok_sandbox::SandboxManager::new(
        xai_grok_sandbox::ProfileName::ReadOnly,
        Path::new("/tmp"),
    );
    assert!(!manager.is_applied(), "no kernel apply in this test");
    manager.install();
    assert!(
        xai_grok_sandbox::should_restrict_child_network(),
        "resolved restrict_network alone must arm the filter"
    );

    let sock = unique_socket_path();
    let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.env(SOCKET_ENV, &sock)
        .args(["--ignored", "--exact", "--nocapture", "subprocess_entry"]);
    xai_grok_sandbox::child_net::restrict_child_network_std(&mut cmd);
    let denied = cmd.output().expect("spawn probe");
    assert_eq!(
        denied.status.code(),
        Some(11),
        "std-wrapped child must get EPERM on connect; stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );

    let _ = std::fs::remove_dir_all(sock.parent().unwrap());
}
