use std::path::PathBuf;
use std::process::Command;

fn git_output(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_string())
        .filter(|output| !output.is_empty())
}

fn git_path(path: &str) -> Option<PathBuf> {
    git_output(&["rev-parse", "--path-format=absolute", "--git-path", path])
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

fn watch_git_head() {
    if let Some(head) = git_path("HEAD") {
        println!("cargo:rerun-if-changed={}", head.display());
    }

    let Some(reference) = git_output(&["symbolic-ref", "-q", "HEAD"]) else {
        return;
    };
    if let Some(reference) = git_path(&reference) {
        println!("cargo:rerun-if-changed={}", reference.display());
    } else if let Some(packed_refs) = git_path("packed-refs") {
        println!("cargo:rerun-if-changed={}", packed_refs.display());
    }
}

fn main() {
    watch_git_head();
    println!("cargo:rerun-if-env-changed=GROK_VERSION");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // The debug CLI's startup can exceed Windows' default 1 MiB stack.
        println!("cargo:rustc-link-arg-bin=open-grok=/STACK:8388608");
    }

    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let version = std::env::var("GROK_VERSION")
        .or_else(|_| std::env::var("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| "0.0.0".to_string());

    println!(
        "cargo:rustc-env=VERSION_WITH_COMMIT={} ({})",
        version, commit
    );
}
