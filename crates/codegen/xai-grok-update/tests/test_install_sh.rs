//! End-to-end contracts for the Open Grok shell installers.
//!
//! The tests execute the shipped scripts against a fake downloader and an
//! isolated `OPENGROK_HOME`. They verify that failed checksum validation keeps
//! an existing binary intact and that neither installer creates upstream
//! `grok` or `agent` aliases.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const GOOD_SCRIPT: &str = "#!/bin/sh\necho open-grok 0.1.220-open-grok.3\nexit 0\n";
const GOOD_SHA256: &str = "8c7fa98272d8df5f93e938d7b7a6498cb4dd07f05f5ff78824895c3b21237364";
const WRONG_VERSION_SCRIPT: &str = "#!/bin/sh\necho open-grok 0.1.220-open-grok.30\nexit 0\n";
const WRONG_VERSION_SHA256: &str =
    "ce1ef8cd4edbb40f831b28028f794bcffff0a649702eb2cb389508f088372529";
const VERSION: &str = "0.1.220-open-grok.3";
const INSTALLER_BLOCK_START: &str = "# >>> open-grok installer >>>";

fn script_path(name: &str) -> Option<PathBuf> {
    let path = if name == "install.sh" {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../install.sh")
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../xai-grok-pager/scripts/{name}"))
    };
    dunce::canonicalize(path).ok().filter(|path| path.exists())
}

fn host_platform() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "aarch64"
    };
    format!("{os}-{arch}")
}

fn write_fake_curl(dir: &Path) {
    let body = format!(
        r#"#!/bin/bash
mode="${{FAKE_MODE:-full}}"
fullsize={fullsize}
head_request=0
out=""
want_code=0
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    --head) head_request=1 ;;
    -o) shift; out="$1" ;;
    -w) shift; [ "$1" = '%{{http_code}}' ] && want_code=1 ;;
    -*) : ;;
    *) url="$1" ;;
  esac
  shift
done
if [ "$head_request" = 1 ]; then
  if [ "$want_code" = 1 ]; then
    printf '200'
  else
    printf 'HTTP/1.1 200 OK\r\nContent-Length: %s\r\n\r\n' "$fullsize"
  fi
  exit 0
fi
if [ -n "$out" ]; then
  case "$url" in
    *.sha256)
      if [ "$mode" = wrong_version ]; then
        printf '%s  open-grok-macos-aarch64\n' '{wrong_sha256}' > "$out"
      else
        printf '%s  open-grok-macos-aarch64\n' '{sha256}' > "$out"
      fi
      ;;
    *)
      case "$mode" in
        full) printf '%s' '{good}' > "$out" ;;
        wrong_version) printf '%s' '{wrong_version}' > "$out" ;;
        truncate) printf '\0\0\0\0' > "$out" ;;
        garbage) head -c "$fullsize" /dev/zero | tr '\0' 'X' > "$out" ;;
      esac
      ;;
  esac
  exit 0
fi
printf '%s' '{version}'
"#,
        fullsize = GOOD_SCRIPT.len(),
        sha256 = GOOD_SHA256,
        good = GOOD_SCRIPT,
        wrong_version = WRONG_VERSION_SCRIPT,
        wrong_sha256 = WRONG_VERSION_SHA256,
        version = VERSION,
    );
    let path = dir.join("curl");
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn isolated_path(fake_bin: &Path) -> String {
    format!("{}:/usr/bin:/bin", fake_bin.display())
}

fn seed_previous_good(open_grok_home: &Path) {
    let bin = open_grok_home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let executable = bin.join("open-grok");
    std::fs::write(&executable, GOOD_SCRIPT).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn assert_active_open_grok_runs(open_grok_home: &Path) {
    let executable = open_grok_home.join("bin/open-grok");
    assert!(executable.exists(), "missing {}", executable.display());
    let status = Command::new(&executable)
        .arg("--version")
        .status()
        .unwrap_or_else(|error| panic!("run {}: {error}", executable.display()));
    assert!(status.success(), "{} must run", executable.display());
}

fn assert_no_upstream_aliases(open_grok_home: &Path) {
    for alias in ["grok", "agent", "grok.exe", "agent.exe"] {
        assert!(
            !open_grok_home.join("bin").join(alias).exists(),
            "installer must not create {}",
            open_grok_home.join("bin").join(alias).display()
        );
    }
}

fn run_standard_installer(script: &Path, home: &Path, fake_bin: &Path, mode: &str) -> bool {
    Command::new("/bin/bash")
        .arg(script)
        .arg(VERSION)
        .env_clear()
        .env("HOME", home)
        .env("PATH", isolated_path(fake_bin))
        .env("OPENGROK_HOME", home.join(".opengrok"))
        .env(
            "OPEN_GROK_RELEASE_BASE_URL",
            "https://fixture.invalid/release",
        )
        .env("FAKE_MODE", mode)
        .status()
        .expect("spawn Open Grok install.sh")
        .success()
}

fn run_standard_installer_with_bin_override(
    script: &Path,
    home: &Path,
    fake_bin: &Path,
    exposed_bin: &Path,
) -> bool {
    Command::new("/bin/bash")
        .arg(script)
        .arg(VERSION)
        .env_clear()
        .env("HOME", home)
        .env("PATH", isolated_path(fake_bin))
        .env("OPENGROK_HOME", home.join(".opengrok"))
        .env("OPEN_GROK_BIN_DIR", exposed_bin)
        .env(
            "OPEN_GROK_RELEASE_BASE_URL",
            "https://fixture.invalid/release",
        )
        .env("FAKE_MODE", "full")
        .status()
        .expect("spawn Open Grok install.sh with bin override")
        .success()
}

fn run_enterprise_installer(script: &Path, home: &Path, fake_bin: &Path, shell: &str) -> bool {
    Command::new("/bin/bash")
        .arg(script)
        .env_clear()
        .env("HOME", home)
        .env("PATH", isolated_path(fake_bin))
        .env("SHELL", shell)
        .env("OPENGROK_HOME", home.join(".opengrok"))
        .env(
            "OPEN_GROK_ENTERPRISE_BASE_URL",
            "https://fixture.invalid/enterprise",
        )
        .env(
            "OPEN_GROK_ENTERPRISE_FALLBACK_URL",
            "https://fixture.invalid/enterprise",
        )
        .env("FAKE_MODE", "full")
        .status()
        .expect("spawn Open Grok enterprise installer")
        .success()
}

#[test]
fn release_installer_preserves_previous_binary_when_checksum_fails() {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        eprintln!("skipping: prebuilt release installer currently targets Apple Silicon macOS");
        return;
    }
    let Some(script) = script_path("install.sh") else {
        eprintln!("skipping: install.sh not found");
        return;
    };
    let fake_bin = tempfile::tempdir().unwrap();
    write_fake_curl(fake_bin.path());

    for (mode, should_succeed) in [
        ("full", true),
        ("truncate", false),
        ("garbage", false),
        ("wrong_version", false),
    ] {
        let home = tempfile::tempdir().unwrap();
        let open_grok_home = home.path().join(".opengrok");
        seed_previous_good(&open_grok_home);

        assert_eq!(
            run_standard_installer(&script, home.path(), fake_bin.path(), mode),
            should_succeed,
            "unexpected install result for {mode}"
        );
        assert_active_open_grok_runs(&open_grok_home);
        assert_no_upstream_aliases(&open_grok_home);
    }
}

#[test]
fn release_installer_keeps_override_linked_to_canonical_managed_command() {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        eprintln!("skipping: prebuilt release installer currently targets Apple Silicon macOS");
        return;
    }
    let Some(script) = script_path("install.sh") else {
        eprintln!("skipping: install.sh not found");
        return;
    };
    let fake_bin = tempfile::tempdir().unwrap();
    write_fake_curl(fake_bin.path());
    let home = tempfile::tempdir().unwrap();
    let open_grok_home = home.path().join(".opengrok");
    let exposed_bin = home.path().join(".local/bin");

    assert!(run_standard_installer_with_bin_override(
        &script,
        home.path(),
        fake_bin.path(),
        &exposed_bin,
    ));

    let canonical = open_grok_home.join("bin/open-grok");
    let exposed = exposed_bin.join("open-grok");
    assert!(
        std::fs::symlink_metadata(&canonical)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        std::fs::symlink_metadata(&exposed)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_link(&exposed).unwrap(), canonical);
    assert!(
        Command::new(&exposed)
            .arg("--version")
            .status()
            .unwrap()
            .success()
    );
    assert_no_upstream_aliases(&open_grok_home);
}

#[test]
fn release_installer_treats_a_trailing_slash_canonical_override_as_the_same_directory() {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        eprintln!("skipping: prebuilt release installer currently targets Apple Silicon macOS");
        return;
    }
    let Some(script) = script_path("install.sh") else {
        eprintln!("skipping: install.sh not found");
        return;
    };
    let fake_bin = tempfile::tempdir().unwrap();
    write_fake_curl(fake_bin.path());
    let home = tempfile::tempdir().unwrap();
    let open_grok_home = home.path().join(".opengrok");
    let trailing_slash_bin = PathBuf::from(format!("{}/", open_grok_home.join("bin").display()));

    assert!(run_standard_installer_with_bin_override(
        &script,
        home.path(),
        fake_bin.path(),
        &trailing_slash_bin,
    ));

    let canonical = open_grok_home.join("bin/open-grok");
    let target = std::fs::read_link(&canonical).unwrap();
    assert_ne!(target, canonical, "managed command must not link to itself");
    assert!(
        Command::new(&canonical)
            .arg("--version")
            .status()
            .unwrap()
            .success()
    );
    assert_no_upstream_aliases(&open_grok_home);
}

#[test]
fn enterprise_installer_uses_only_the_open_grok_namespace() {
    let Some(script) = script_path("install-enterprise.sh") else {
        eprintln!("skipping: install-enterprise.sh not found");
        return;
    };
    let fake_bin = tempfile::tempdir().unwrap();
    write_fake_curl(fake_bin.path());
    let home = tempfile::tempdir().unwrap();
    let open_grok_home = home.path().join(".opengrok");

    assert!(run_enterprise_installer(
        &script,
        home.path(),
        fake_bin.path(),
        "/bin/false",
    ));
    assert_active_open_grok_runs(&open_grok_home);
    assert_no_upstream_aliases(&open_grok_home);
    assert!(open_grok_home.join("config.toml").is_file());

    let downloaded = open_grok_home
        .join("downloads")
        .join(format!("open-grok-{}", host_platform()));
    assert!(downloaded.is_file(), "missing {}", downloaded.display());
}

#[test]
fn enterprise_installer_preserves_stowed_shell_rc_and_uses_distinct_block() {
    let Some(script) = script_path("install-enterprise.sh") else {
        eprintln!("skipping: install-enterprise.sh not found");
        return;
    };
    let fake_bin = tempfile::tempdir().unwrap();
    write_fake_curl(fake_bin.path());
    let home = tempfile::tempdir().unwrap();
    let dotfiles = home.path().join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    let target = dotfiles.join("bashrc");
    std::fs::write(&target, "# user shell rc\n").unwrap();
    let link = home.path().join(".bashrc");
    std::os::unix::fs::symlink("dotfiles/bashrc", &link).unwrap();

    for _ in 0..2 {
        assert!(run_enterprise_installer(
            &script,
            home.path(),
            fake_bin.path(),
            "/bin/bash",
        ));
    }

    assert!(link.is_symlink(), "stowed .bashrc must remain a symlink");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new("dotfiles/bashrc")
    );
    let body = std::fs::read_to_string(&target).unwrap();
    assert!(body.contains("# user shell rc"));
    assert_eq!(body.matches(INSTALLER_BLOCK_START).count(), 1, "{body}");
    assert!(!body.contains("# >>> grok installer >>>"), "{body}");
}
