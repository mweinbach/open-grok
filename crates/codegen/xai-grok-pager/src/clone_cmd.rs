use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueHint};
use serde::Deserialize;

#[derive(Debug, Clone, Args)]
pub struct CloneArgs {
    #[arg(value_name = "URL", help = "Remote repository URL")]
    url: String,
    #[arg(value_name = "DIR", value_hint = ValueHint::DirPath, help = "Destination directory (default: URL basename)")]
    dir: Option<PathBuf>,
    #[arg(
        short,
        long,
        value_name = "BRANCH",
        help = "Branch or ref to check out"
    )]
    branch: Option<String>,
    #[arg(
        long,
        value_name = "PATH",
        help = "Sparse-checkout cone path (repeatable)"
    )]
    cone: Vec<String>,
    #[arg(
        long,
        help = "Fetch complete history, tags, and all remote branches instead of the depth-1, blob:none bootstrap of the selected branch"
    )]
    full_history: bool,
    #[arg(
        long,
        help = "Use the optional Grove daemon and projected filesystem instead of Git"
    )]
    grove: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum History {
    Shallow,
    Full,
}

impl History {
    fn operation(self) -> &'static str {
        match self {
            Self::Shallow => "clone_shallow",
            Self::Full => "clone",
        }
    }
}

#[derive(Debug)]
struct ClonePlan {
    url: String,
    dest: PathBuf,
    branch: Option<String>,
    cone: Vec<String>,
    history: History,
}

impl CloneArgs {
    fn plan(self, cwd: &Path, data_dir: Option<&Path>) -> Result<ClonePlan> {
        ensure!(
            !self.url.trim().is_empty()
                && !self.url.starts_with('-')
                && !self.url.chars().any(char::is_control),
            "repository URL must be nonempty and contain no control characters or leading dash"
        );
        if let Some(branch) = &self.branch {
            ensure!(
                !branch.is_empty()
                    && !branch.starts_with('-')
                    && !branch.chars().any(char::is_control)
                    && git2::Reference::is_valid_name(&format!("refs/heads/{branch}")),
                "branch must be a valid Git branch or tag name, not an option or revision expression"
            );
        }
        for cone in &self.cone {
            ensure!(
                !cone.is_empty()
                    && !cone.chars().any(char::is_control)
                    && !cone.contains('\\')
                    && !cone.contains(':')
                    && !cone.starts_with('-')
                    && Path::new(cone).components().all(|component| matches!(
                        component,
                        Component::Normal(_) | Component::CurDir
                    )),
                "cone paths must be repository-relative paths without parent traversal"
            );
        }
        let dir = match self.dir {
            Some(dir) => dir,
            None => default_directory(&self.url)?,
        };
        let dest = validate_destination(&dir, cwd, data_dir)?;
        Ok(ClonePlan {
            url: self.url,
            dest,
            branch: self.branch,
            cone: self.cone,
            history: if self.full_history {
                History::Full
            } else {
                History::Shallow
            },
        })
    }
}

fn default_directory(source: &str) -> Result<PathBuf> {
    let parsed = source.contains("://").then(|| url::Url::parse(source));
    let source_path = match &parsed {
        Some(parsed) => parsed
            .as_ref()
            .map_err(|_| anyhow::anyhow!("invalid repository URL"))?
            .path(),
        None => source.split_once(':').map_or(source, |(_, path)| path),
    };
    let basename = if parsed.is_none() && (Path::new(source).is_absolute() || !source.contains(':'))
    {
        Path::new(source.trim_end_matches('/'))
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    } else {
        source_path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
    };
    let basename = basename.strip_suffix(".git").unwrap_or(basename);
    ensure!(
        !basename.is_empty() && basename != "." && basename != ".." && !basename.contains('\\'),
        "cannot infer a safe destination from the URL; specify DIR explicitly"
    );
    Ok(PathBuf::from(basename))
}

fn validate_destination(dir: &Path, cwd: &Path, data_dir: Option<&Path>) -> Result<PathBuf> {
    ensure!(
        !dir.as_os_str().is_empty(),
        "clone destination must not be empty"
    );
    ensure!(
        !dir.components()
            .any(|component| matches!(component, Component::ParentDir)),
        "clone destination must not contain parent traversal"
    );
    let dest = if dir.is_absolute() {
        dir.to_owned()
    } else {
        cwd.join(dir)
    };
    let name = dest
        .file_name()
        .context("clone destination must have a directory name")?;
    let parent = dest
        .parent()
        .context("clone destination has no parent directory")?;
    let parent_metadata =
        std::fs::symlink_metadata(parent).context("inspect clone destination parent")?;
    ensure!(
        parent_metadata.is_dir() && !parent_metadata.file_type().is_symlink(),
        "clone destination parent must be an existing directory, not a symlink"
    );
    let dest = dunce::canonicalize(parent)
        .context("resolve clone destination parent")?
        .join(name);
    match std::fs::symlink_metadata(&dest) {
        Ok(_) => bail!("clone destination already exists; refusing to replace it"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect clone destination"),
    }
    if let Some(data_dir) = data_dir {
        let data_dir = resolve_existing_ancestor(data_dir)?;
        ensure!(
            !dest.starts_with(&data_dir) && !data_dir.starts_with(&dest),
            "clone destination must not overlap the Grove data directory"
        );
    }
    let dest_text = dest
        .to_str()
        .context("clone destination must be valid UTF-8")?;
    ensure!(
        !dest_text.chars().any(char::is_control),
        "clone destination must not contain control characters"
    );
    Ok(dest)
}

fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "Grove data directory must be absolute");
    ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "Grove data directory must not contain parent traversal"
    );
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match dunce::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for name in suffix.into_iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    std::fs::symlink_metadata(ancestor).is_err(),
                    "Grove data directory contains an unresolved symlink"
                );
                suffix.push(
                    ancestor
                        .file_name()
                        .context("invalid Grove data directory")?,
                );
                ancestor = ancestor.parent().context("invalid Grove data directory")?;
            }
            Err(error) => return Err(error).context("resolve Grove data directory"),
        }
    }
}

#[derive(Default, Deserialize)]
struct GroveConfig {
    #[serde(default)]
    clone: CloneConfig,
    runtime_dir: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
struct CloneConfig {
    #[serde(default)]
    enabled: bool,
}

fn read_config(path: &Path) -> Result<GroveConfig> {
    match std::fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents).context("parse Grove configuration"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(GroveConfig::default()),
        Err(error) => Err(error).context("read Grove configuration"),
    }
}

pub fn run_clone(args: CloneArgs) -> Result<()> {
    if !args.grove {
        return git_backend::run(args);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        backend::run(args)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = args;
        bail!("The --grove backend requires macOS or Linux; omit --grove to clone with Git")
    }
}

mod git_backend {
    use std::process::{Command, Stdio};

    use super::*;

    fn validate_source(source: &str) -> Result<()> {
        if source.contains("://") {
            let url =
                url::Url::parse(source).map_err(|_| anyhow::anyhow!("invalid repository URL"))?;
            ensure!(
                matches!(url.scheme(), "https" | "http" | "ssh" | "git" | "file"),
                "unsupported repository URL scheme; use HTTPS, SSH, Git, or a local path"
            );
        } else if let Some((helper, _)) = source.split_once("::") {
            ensure!(
                helper.contains('/') || helper.contains('['),
                "Git remote-helper URLs are not supported"
            );
        }
        Ok(())
    }

    fn command(cwd: &Path, factory: &mut impl FnMut() -> Command) -> Command {
        let mut command = factory();
        command
            .current_dir(cwd)
            .args([
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "submodule.recurse=false",
            ])
            .args([
                "-c",
                if cfg!(windows) {
                    "core.hooksPath=NUL"
                } else {
                    "core.hooksPath=/dev/null"
                },
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
            "GIT_CONFIG",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_TEMPLATE_DIR",
        ] {
            command.env_remove(name);
        }
        command
    }

    fn check(command: &mut Command, operation: &str) -> Result<()> {
        let status = command
            .status()
            .context("could not run Git; ensure it is installed and on PATH")?;
        ensure!(
            status.success(),
            "Git {operation} failed; check the repository URL, branch, and Git credentials. Any partial destination is retained; no fallback or cleanup was attempted"
        );
        Ok(())
    }

    fn execute(plan: &ClonePlan, cwd: &Path, mut factory: impl FnMut() -> Command) -> Result<()> {
        validate_source(&plan.url)?;
        std::fs::create_dir(&plan.dest)
            .context("reserve clone destination without replacing existing files")?;
        let mut clone = command(cwd, &mut factory);
        clone.args([
            "clone",
            "--no-checkout",
            "--no-local",
            "--no-recurse-submodules",
            "--template=",
            "--origin=origin",
        ]);
        if plan.history == History::Shallow {
            clone.args([
                "--depth=1",
                "--single-branch",
                "--no-tags",
                "--filter=blob:none",
            ]);
        } else {
            clone.args(["--no-single-branch", "--tags"]);
        }
        if let Some(branch) = &plan.branch {
            clone.arg("--branch").arg(branch);
        }
        clone.arg("--").arg(&plan.url).arg(&plan.dest);
        check(&mut clone, "clone")?;
        if !plan.cone.is_empty() {
            let mut sparse = command(&plan.dest, &mut factory);
            sparse
                .args(["sparse-checkout", "set", "--cone", "--"])
                .args(&plan.cone);
            check(&mut sparse, "sparse checkout")?;
        }
        let mut checkout = command(&plan.dest, &mut factory);
        checkout.args(["reset", "--hard", "HEAD"]);
        check(&mut checkout, "checkout")?;
        Ok(())
    }

    pub(super) fn run(args: CloneArgs) -> Result<()> {
        let cwd = std::env::current_dir().context("read current directory")?;
        let plan = args.plan(&cwd, None)?;
        execute(&plan, &cwd, || Command::new("git"))?;
        println!("Cloned repository into {} (Git)", plan.dest.display());
        match plan.history {
            History::Shallow => println!(
                "  history: depth 1, selected branch only (blob:none). Deepen with `git fetch --deepen=N origin` or `git fetch --unshallow origin`. Other branches require an explicit depth-limited refspec."
            ),
            History::Full => println!("  history: complete"),
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct Fixture {
            root: tempfile::TempDir,
            source: PathBuf,
            home: PathBuf,
        }

        impl Fixture {
            fn new() -> Self {
                let root = tempfile::tempdir().unwrap();
                let source = root.path().join("source repo");
                let home = root.path().join("home");
                std::fs::create_dir(&home).unwrap();
                let fixture = Self { root, source, home };
                fixture.git(
                    fixture.root.path(),
                    &["init", "--initial-branch=main", "source repo"],
                );
                fixture.git(
                    &fixture.source,
                    &["config", "uploadpack.allowFilter", "true"],
                );
                for (path, contents) in [
                    ("README", "root\n"),
                    ("src/app", "first\n"),
                    ("docs/guide", "guide\n"),
                    ("other/hidden", "other\n"),
                ] {
                    let path = fixture.source.join(path);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, contents).unwrap();
                }
                fixture.git(&fixture.source, &["add", "."]);
                fixture.git(&fixture.source, &["commit", "-m", "first"]);
                fixture.git(&fixture.source, &["tag", "v1"]);
                fixture.git(&fixture.source, &["branch", "extra"]);
                std::fs::write(fixture.source.join("src/app"), "second\n").unwrap();
                fixture.git(&fixture.source, &["commit", "-am", "second"]);
                fixture.git(&fixture.source, &["tag", "v2"]);
                fixture
            }

            fn command(&self) -> Command {
                let mut command = Command::new("git");
                command.env_clear();
                for name in ["PATH", "SystemRoot", "TEMP", "TMP"] {
                    if let Some(value) = std::env::var_os(name) {
                        command.env(name, value);
                    }
                }
                command
                    .env("HOME", &self.home)
                    .env("USERPROFILE", &self.home)
                    .env("XDG_CONFIG_HOME", self.home.join("config"))
                    .env("OPENGROK_HOME", self.home.join("opengrok"))
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env(
                        "GIT_CONFIG_GLOBAL",
                        if cfg!(windows) { "NUL" } else { "/dev/null" },
                    )
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .env("GIT_AUTHOR_NAME", "Clone Test")
                    .env("GIT_AUTHOR_EMAIL", "clone@example.invalid")
                    .env("GIT_COMMITTER_NAME", "Clone Test")
                    .env("GIT_COMMITTER_EMAIL", "clone@example.invalid");
                command
            }

            fn git(&self, cwd: &Path, args: &[&str]) -> String {
                let output = self.command().current_dir(cwd).args(args).output().unwrap();
                assert!(
                    output.status.success(),
                    "test Git command failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                String::from_utf8(output.stdout).unwrap().trim().to_owned()
            }

            fn plan(
                &self,
                dest: &str,
                history: History,
                branch: Option<&str>,
                cone: &[&str],
            ) -> ClonePlan {
                CloneArgs {
                    url: self.source.to_str().unwrap().to_owned(),
                    dir: Some(PathBuf::from(dest)),
                    branch: branch.map(str::to_owned),
                    cone: cone.iter().map(|path| (*path).to_owned()).collect(),
                    full_history: history == History::Full,
                    grove: false,
                }
                .plan(self.root.path(), None)
                .unwrap()
            }

            fn clone_repo(&self, plan: &ClonePlan) {
                execute(plan, self.root.path(), || self.command()).unwrap();
            }
        }

        #[test]
        fn git_default_clone_is_shallow_branch_only_and_can_unshallow() {
            let fixture = Fixture::new();
            let plan = fixture.plan("clone with spaces", History::Shallow, None, &[]);
            fixture.clone_repo(&plan);
            assert_eq!(
                fixture.git(&plan.dest, &["rev-parse", "--is-shallow-repository"]),
                "true"
            );
            assert_eq!(
                fixture.git(&plan.dest, &["rev-list", "--count", "HEAD"]),
                "1"
            );
            assert_eq!(
                fixture.git(&plan.dest, &["config", "--get", "remote.origin.fetch"]),
                "+refs/heads/main:refs/remotes/origin/main"
            );
            assert_eq!(
                fixture.git(
                    &plan.dest,
                    &["config", "--get", "remote.origin.partialclonefilter"]
                ),
                "blob:none"
            );
            assert_eq!(fixture.git(&plan.dest, &["tag", "--list"]), "");
            assert_eq!(
                std::fs::read_to_string(plan.dest.join("src/app")).unwrap(),
                "second\n"
            );
            fixture.git(&plan.dest, &["fetch", "--unshallow", "origin"]);
            assert_eq!(
                fixture.git(&plan.dest, &["rev-list", "--count", "HEAD"]),
                "2"
            );
            let remotes = fixture.git(
                &plan.dest,
                &["for-each-ref", "--format=%(refname)", "refs/remotes/origin"],
            );
            assert!(!remotes.contains("extra"));
        }

        #[test]
        fn git_clone_selects_only_requested_branch() {
            let fixture = Fixture::new();
            let plan = fixture.plan("selected", History::Shallow, Some("extra"), &[]);
            fixture.clone_repo(&plan);
            assert_eq!(
                fixture.git(&plan.dest, &["symbolic-ref", "--short", "HEAD"]),
                "extra"
            );
            assert_eq!(
                fixture.git(&plan.dest, &["rev-list", "--count", "HEAD"]),
                "1"
            );
            assert_eq!(
                fixture.git(&plan.dest, &["config", "--get", "remote.origin.fetch"]),
                "+refs/heads/extra:refs/remotes/origin/extra"
            );
            assert_eq!(
                std::fs::read_to_string(plan.dest.join("src/app")).unwrap(),
                "first\n"
            );
        }

        #[test]
        fn git_full_history_includes_branches_and_tags() {
            let fixture = Fixture::new();
            let plan = fixture.plan("full", History::Full, None, &[]);
            fixture.clone_repo(&plan);
            assert_eq!(
                fixture.git(&plan.dest, &["rev-parse", "--is-shallow-repository"]),
                "false"
            );
            assert_eq!(
                fixture.git(&plan.dest, &["rev-list", "--count", "HEAD"]),
                "2"
            );
            assert_eq!(
                fixture.git(&plan.dest, &["config", "--get", "remote.origin.fetch"]),
                "+refs/heads/*:refs/remotes/origin/*"
            );
            assert_eq!(fixture.git(&plan.dest, &["tag", "--list"]), "v1\nv2");
            assert!(
                fixture
                    .git(
                        &plan.dest,
                        &["for-each-ref", "--format=%(refname)", "refs/remotes/origin"]
                    )
                    .contains("origin/extra")
            );
        }

        #[test]
        fn git_clone_applies_repeatable_sparse_cones_before_checkout() {
            let fixture = Fixture::new();
            let plan = fixture.plan("sparse", History::Shallow, None, &["src", "docs"]);
            fixture.clone_repo(&plan);
            assert!(plan.dest.join("README").exists());
            assert!(plan.dest.join("src/app").exists());
            assert!(plan.dest.join("docs/guide").exists());
            assert!(!plan.dest.join("other").exists());
            assert_eq!(
                fixture.git(&plan.dest, &["sparse-checkout", "list"]),
                "docs\nsrc"
            );
            assert_eq!(
                fixture.git(&plan.dest, &["config", "--get", "core.sparseCheckoutCone"]),
                "true"
            );
        }

        #[test]
        fn git_clone_reservation_preserves_racing_destination() {
            let fixture = Fixture::new();
            let plan = fixture.plan("raced", History::Shallow, None, &[]);
            std::fs::create_dir(&plan.dest).unwrap();
            std::fs::write(plan.dest.join("keep"), "preserved").unwrap();
            assert!(execute(&plan, fixture.root.path(), || panic!("Git must not start")).is_err());
            assert_eq!(
                std::fs::read_to_string(plan.dest.join("keep")).unwrap(),
                "preserved"
            );
        }

        #[test]
        fn git_clone_failure_retains_destination_and_hides_source() {
            let fixture = Fixture::new();
            let mut plan = fixture.plan("failed", History::Shallow, None, &[]);
            plan.url = fixture
                .root
                .path()
                .join("missing-secret-token")
                .to_str()
                .unwrap()
                .to_owned();
            let mut calls = 0;
            let error = execute(&plan, fixture.root.path(), || {
                calls += 1;
                fixture.command()
            })
            .unwrap_err();
            assert_eq!(calls, 1);
            assert!(plan.dest.is_dir());
            assert!(!format!("{error:#}").contains("secret-token"));
        }

        #[test]
        fn git_clone_rejects_remote_helpers_before_creation() {
            let fixture = Fixture::new();
            for source in ["ext::sh -c malicious", "helper://example.invalid/repo"] {
                let mut plan = fixture.plan("rejected", History::Shallow, None, &[]);
                plan.url = source.into();
                assert!(
                    execute(&plan, fixture.root.path(), || panic!("Git must not start")).is_err()
                );
                assert!(!plan.dest.exists());
            }
            for source in [
                "git@example.invalid:team/repo.git",
                "ssh://git@[::1]/repo",
                "file:///tmp/repo",
                "./local::path",
            ] {
                assert!(validate_source(source).is_ok());
            }
        }

        #[test]
        fn git_subprocesses_clear_inherited_repository_overrides() {
            let fixture = Fixture::new();
            let plan = fixture.plan("isolated", History::Shallow, None, &[]);
            execute(&plan, fixture.root.path(), || {
                let mut command = fixture.command();
                command
                    .env("GIT_DIR", fixture.source.join(".git"))
                    .env("GIT_WORK_TREE", &fixture.source)
                    .env("GIT_INDEX_FILE", fixture.source.join(".git/index"));
                command
            })
            .unwrap();
            assert_eq!(fixture.git(&fixture.source, &["status", "--porcelain"]), "");
            assert_eq!(
                fixture.git(&plan.dest, &["rev-parse", "--is-shallow-repository"]),
                "true"
            );
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod backend {
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use xai_fast_worktree::{NfsWorktreeClient, NfsWorktreeOpts};

    use super::*;

    const RPC_TIMEOUT: Duration = Duration::from_secs(5);
    const CLONE_TIMEOUT: Duration = Duration::from_secs(1800);
    const POLL_INTERVAL: Duration = Duration::from_millis(250);

    #[derive(Debug, Deserialize)]
    struct CloneMount {
        transport: String,
        history: Option<History>,
        source_mode: Option<String>,
    }

    #[derive(Default, Deserialize)]
    struct Reply {
        #[serde(default)]
        pong: bool,
        status: Option<DaemonStatus>,
        clone_mount: Option<CloneMount>,
        declined: Option<String>,
    }

    #[derive(Deserialize)]
    struct DaemonStatus {
        #[serde(default)]
        capabilities: Vec<String>,
    }

    fn request(client: &NfsWorktreeClient, request: Value, timeout: Duration) -> Result<Reply> {
        decode_reply(client.call_json(&request, timeout)?)
    }

    fn decode_reply(response: Value) -> Result<Reply> {
        let data = response.get("data").context("Grove response has no data")?;
        ensure!(
            data.get("v").and_then(Value::as_u64) == Some(1),
            "incompatible Grove protocol version"
        );
        match response.get("status").and_then(Value::as_str) {
            Some("ok") => {
                let reply: Reply =
                    serde_json::from_value(data.clone()).context("invalid Grove response")?;
                ensure!(
                    reply.declined.is_none(),
                    "Grove daemon declined the clone request"
                );
                Ok(reply)
            }
            Some("err") => {
                bail!(
                    "Grove daemon rejected the operation; inspect its local logs for details (destination is retained)"
                )
            }
            _ => bail!("invalid Grove response status"),
        }
    }

    fn clone_request(plan: &ClonePlan) -> Value {
        json!({
            "op": plan.history.operation(),
            "v": 1,
            "url": plan.url,
            "dir": plan.dest,
            "branch": plan.branch,
            "cone": plan.cone,
            "redirects": {"enabled": null, "extra_allowlist": []}
        })
    }

    fn clone_repository(
        client: &NfsWorktreeClient,
        plan: &ClonePlan,
        timeout: Duration,
    ) -> Result<CloneMount> {
        let ping = request(client, json!({"op": "ping", "v": 1}), Duration::from_millis(250))
            .context("Grove daemon is unavailable; start it with clone enabled and verify GROVE_CONTROL_SOCK")?;
        ensure!(ping.pong, "Grove daemon did not acknowledge ping");
        if plan.history == History::Shallow {
            let status = request(
                client,
                json!({"op": "status", "v": 1, "dir": null}),
                RPC_TIMEOUT,
            )?;
            ensure!(
                status.status.is_some_and(|status| status
                    .capabilities
                    .iter()
                    .any(|capability| capability == "clone_shallow")),
                "Grove daemon does not support depth-1 clone; restart or update it, or pass --full-history"
            );
        }
        let deadline = Instant::now() + timeout;
        let reply = request(client, clone_request(plan), RPC_TIMEOUT.min(timeout));
        let mut reply = match reply {
            Ok(reply) => reply,
            Err(error) if is_transport_error(&error) => Reply::default(),
            Err(error) => return Err(error),
        };
        loop {
            if let Some(mount) = reply.clone_mount {
                validate_mount(&mount, plan.history)?;
                return Ok(mount);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "Grove clone timed out; destination is retained and may still be mounting; inspect the daemon before retrying"
            );
            std::thread::sleep(POLL_INTERVAL.min(remaining));
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "Grove clone timed out; destination is retained and may still be mounting; inspect the daemon before retrying"
            );
            reply = request(client, json!({"op": "query_clone", "v": 1, "dir": plan.dest}), RPC_TIMEOUT.min(remaining))
                .context("Grove clone outcome is uncertain; destination is retained; inspect the daemon before retrying")?;
        }
    }

    fn is_transport_error(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
            || error.to_string().contains("empty response")
    }

    fn validate_mount(mount: &CloneMount, requested: History) -> Result<()> {
        ensure!(
            mount.history.is_none_or(|history| history == requested),
            "Grove daemon returned a different history mode than requested; destination is retained; restart or update the daemon"
        );
        let expected = if cfg!(target_os = "macos") {
            "nfs"
        } else {
            "fuse"
        };
        ensure!(
            mount.transport == expected,
            "Grove daemon returned an incompatible mount transport; destination is retained"
        );
        Ok(())
    }

    pub(super) fn run(args: CloneArgs) -> Result<()> {
        let home = dirs::home_dir().context("cannot locate the home directory")?;
        let config_root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let config = read_config(&config_root.join("grove/config.toml"))?;
        ensure!(
            config.clone.enabled || std::env::var("GROVE_CLONE").is_ok_and(|value| value == "1"),
            "Open Grok clone is disabled; set [clone] enabled = true in Grove's config.toml and restart the daemon"
        );
        let data_dir = std::env::var_os("GROVE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/share"))
                    .join("grove")
            });
        let runtime_dir = config.runtime_dir.unwrap_or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(|path| PathBuf::from(path).join("grove"))
                .unwrap_or_else(|| data_dir.join("run"))
        });
        let socket = std::env::var_os("GROVE_CONTROL_SOCK")
            .map(PathBuf::from)
            .unwrap_or_else(|| runtime_dir.join("control.sock"));
        ensure!(
            socket.is_absolute(),
            "Grove control socket must be an absolute path"
        );
        let client = NfsWorktreeClient::from_opts(&NfsWorktreeOpts {
            enabled: true,
            control_sock: Some(socket),
            runtime_dir: Some(runtime_dir),
            data_dir: Some(data_dir.clone()),
            ..Default::default()
        });
        let cwd = std::env::current_dir().context("read current directory")?;
        let plan = args.plan(&cwd, Some(&data_dir))?;
        let mount = clone_repository(&client, &plan, CLONE_TIMEOUT)?;
        ensure!(
            xai_fast_worktree::dest_is_mountpoint(&plan.dest),
            "Grove reported completion but the destination is not a verified mount; nothing was removed"
        );
        println!(
            "Cloned repository into {} ({})",
            plan.dest.display(),
            mount.transport
        );
        if mount.source_mode.as_deref() == Some("local") {
            println!("  objects: local (shared with an existing checkout)");
        }
        match plan.history {
            History::Shallow => println!(
                "  history: depth 1, selected branch only (blob:none). Deepen with `git fetch --deepen=N origin` or `git fetch --unshallow origin`. Other branches require an explicit depth-limited refspec."
            ),
            History::Full => println!("  history: complete"),
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        fn plan(history: History) -> ClonePlan {
            ClonePlan {
                url: "https://example.invalid/team/repo.git".into(),
                dest: "/tmp/clone-protocol-only".into(),
                branch: Some("feature/demo".into()),
                cone: vec!["src".into(), "docs".into()],
                history,
            }
        }

        fn scripted_client(
            responses: Vec<Value>,
        ) -> (
            tempfile::TempDir,
            NfsWorktreeClient,
            std::thread::JoinHandle<Vec<Value>>,
        ) {
            let temp = tempfile::tempdir().unwrap();
            let socket = temp.path().join("control.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let thread = std::thread::spawn(move || {
                let mut requests = Vec::new();
                for response in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut line = String::new();
                    BufReader::new(&stream).read_line(&mut line).unwrap();
                    requests.push(serde_json::from_str(&line).unwrap());
                    if !response.is_null() {
                        writeln!(stream, "{response}").unwrap();
                    }
                }
                requests
            });
            let client = NfsWorktreeClient::from_opts(&NfsWorktreeOpts {
                control_sock: Some(socket),
                ..Default::default()
            });
            (temp, client, thread)
        }

        fn ok(data: Value) -> Value {
            let mut data = data;
            data["v"] = json!(1);
            json!({"status": "ok", "data": data})
        }

        #[test]
        fn clone_wire_matches_official_1_0_13_capture() {
            let shallow_plan = plan(History::Shallow);
            assert_eq!(
                clone_request(&shallow_plan),
                json!({
                    "op":"clone_shallow", "v":1, "url":"https://example.invalid/team/repo.git",
                    "dir":"/tmp/clone-protocol-only", "branch":"feature/demo", "cone":["src","docs"],
                    "redirects":{"enabled":null,"extra_allowlist":[]}
                })
            );
            assert_eq!(clone_request(&plan(History::Full))["op"], "clone");
        }

        #[test]
        fn missing_daemon_never_creates_destination() {
            let temp = tempfile::tempdir().unwrap();
            let client = NfsWorktreeClient::from_opts(&NfsWorktreeOpts {
                control_sock: Some(temp.path().join("missing.sock")),
                ..Default::default()
            });
            let mut clone_plan = plan(History::Shallow);
            clone_plan.dest = temp.path().join("untouched");
            let error = clone_repository(&client, &clone_plan, Duration::from_secs(1)).unwrap_err();
            assert!(error.to_string().contains("unavailable"));
            assert!(!clone_plan.dest.exists());
        }

        #[test]
        fn uncertain_clone_timeout_does_not_remove_destination() {
            let (temp, client, server) = scripted_client(vec![
                ok(json!({"pong":true})),
                ok(json!({"clone_phase":"fetching"})),
            ]);
            let mut clone_plan = plan(History::Full);
            clone_plan.dest = temp.path().join("in-flight");
            std::fs::create_dir(&clone_plan.dest).unwrap();
            std::fs::write(clone_plan.dest.join("keep"), "retained").unwrap();
            let error =
                clone_repository(&client, &clone_plan, Duration::from_millis(1)).unwrap_err();
            assert!(error.to_string().contains("retained"));
            assert_eq!(
                std::fs::read_to_string(clone_plan.dest.join("keep")).unwrap(),
                "retained"
            );
            assert_eq!(server.join().unwrap().len(), 2);
        }

        #[test]
        fn shallow_clone_refuses_daemon_without_capability_before_creation() {
            let (_temp, client, server) = scripted_client(vec![
                ok(json!({"pong":true})),
                ok(json!({"status":{"capabilities":[]}})),
            ]);
            let error = clone_repository(&client, &plan(History::Shallow), Duration::from_secs(1))
                .unwrap_err();
            assert!(error.to_string().contains("does not support depth-1"));
            assert_eq!(server.join().unwrap().len(), 2);
        }

        #[test]
        fn lost_clone_reply_queries_without_resubmitting_creation() {
            let transport = if cfg!(target_os = "macos") {
                "nfs"
            } else {
                "fuse"
            };
            let (_temp, client, server) = scripted_client(vec![
                ok(json!({"pong":true})),
                Value::Null,
                ok(
                    json!({"clone_mount":{"transport":transport,"history":"full","source_mode":"local"}}),
                ),
            ]);
            let mount =
                clone_repository(&client, &plan(History::Full), Duration::from_secs(2)).unwrap();
            assert_eq!(mount.source_mode.as_deref(), Some("local"));
            let requests = server.join().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request["op"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["ping", "clone", "query_clone"]
            );
            assert_eq!(
                requests[2],
                json!({"op":"query_clone","v":1,"dir":"/tmp/clone-protocol-only"})
            );
        }

        #[test]
        fn history_mismatch_and_wrong_transport_fail_closed() {
            let expected = if cfg!(target_os = "macos") {
                "nfs"
            } else {
                "fuse"
            };
            let mount = CloneMount {
                transport: expected.into(),
                history: Some(History::Full),
                source_mode: None,
            };
            assert!(validate_mount(&mount, History::Shallow).is_err());
            let mount = CloneMount {
                transport: "copy".into(),
                history: Some(History::Shallow),
                source_mode: None,
            };
            assert!(validate_mount(&mount, History::Shallow).is_err());
        }

        #[test]
        fn unknown_reply_version_and_declines_fail_closed() {
            assert!(decode_reply(json!({"status":"ok","data":{"v":2,"pong":true}})).is_err());
            assert!(decode_reply(ok(json!({"declined":"disabled"}))).is_err());
            assert!(decode_reply(json!({"status":"unknown","data":{"v":1}})).is_err());
        }

        #[test]
        fn daemon_error_redacts_url_credentials_and_query() {
            let error = decode_reply(json!({"status":"err","data":{"v":1,"error":"fetch https://user:password@example.invalid/repo?token=secret failed"}})).err().unwrap();
            assert!(!error.to_string().contains("password"));
            assert!(!error.to_string().contains("secret"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: CloneArgs,
    }

    #[test]
    fn clone_cli_accepts_documented_flags() {
        let cli = Cli::try_parse_from([
            "clone",
            "git@example.invalid:team/repo.git",
            "dest",
            "--branch",
            "main",
            "--cone",
            "src",
            "--cone",
            "docs",
            "--full-history",
        ])
        .unwrap();
        assert!(cli.args.full_history);
        assert!(!cli.args.grove);
        assert_eq!(cli.args.cone, ["src", "docs"]);
        assert_eq!(cli.args.branch.as_deref(), Some("main"));
        assert!(
            !Cli::try_parse_from(["clone", "https://example.invalid/repo"])
                .unwrap()
                .args
                .full_history
        );
        assert!(Cli::try_parse_from(["clone"]).is_err());
        assert!(
            Cli::try_parse_from(["clone", "https://example.invalid/repo", "--grove"])
                .unwrap()
                .args
                .grove
        );
    }

    #[test]
    fn clone_default_directory_handles_urls_and_scp_without_credentials() {
        for source in [
            "https://user:password@example.invalid/team/repo.git?token=secret",
            "ssh://git@example.invalid/team/repo.git/",
            "git@example.invalid:team/repo.git",
            "/local/repo.git",
        ] {
            assert_eq!(default_directory(source).unwrap(), PathBuf::from("repo"));
        }
        assert!(default_directory("https://example.invalid/").is_err());
        assert!(default_directory(".").is_err());
    }

    #[test]
    fn clone_validation_rejects_option_and_revision_branch_names() {
        let temp = tempfile::tempdir().unwrap();
        for branch in ["-option", "main~1", "a..b", "@{-1}", "bad name", ""] {
            let cli = Cli::try_parse_from([
                "clone",
                "https://example.invalid/repo",
                &format!("--branch={branch}"),
            ])
            .unwrap();
            assert!(cli.args.plan(temp.path(), None).is_err());
        }
        assert!(!temp.path().join("repo").exists());
    }

    #[test]
    fn clone_validation_never_replaces_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("repo");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("keep"), "preserved").unwrap();
        assert!(validate_destination(&dest, temp.path(), Some(&temp.path().join("data"))).is_err());
        assert_eq!(
            std::fs::read_to_string(dest.join("keep")).unwrap(),
            "preserved"
        );
    }

    #[test]
    fn clone_validation_rejects_traversal_and_data_overlap() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            validate_destination(
                Path::new("../escape"),
                temp.path(),
                Some(&temp.path().join("data"))
            )
            .is_err()
        );
        assert!(
            validate_destination(
                Path::new("data"),
                temp.path(),
                Some(&temp.path().join("data"))
            )
            .is_err()
        );
        assert!(
            validate_destination(
                Path::new("data"),
                temp.path(),
                Some(&temp.path().join("data/nested"))
            )
            .is_err()
        );
        assert!(!temp.path().join("data").exists());
    }

    #[cfg(unix)]
    #[test]
    fn clone_validation_rejects_dangling_symlink_and_symlink_parent() {
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("repo");
        std::os::unix::fs::symlink(temp.path().join("missing"), &link).unwrap();
        assert!(validate_destination(&link, temp.path(), Some(&temp.path().join("data"))).is_err());
        let parent = temp.path().join("parent");
        std::os::unix::fs::symlink(temp.path(), &parent).unwrap();
        assert!(
            validate_destination(
                &parent.join("new"),
                temp.path(),
                Some(&temp.path().join("data"))
            )
            .is_err()
        );
    }

    #[test]
    fn clone_cones_cannot_escape_repository() {
        let temp = tempfile::tempdir().unwrap();
        for cone in [
            "../secret",
            "/absolute",
            "src/../../escape",
            "C:\\outside",
            "--option",
            "",
        ] {
            let cli = Cli::try_parse_from([
                "clone",
                "https://example.invalid/repo",
                &format!("--cone={cone}"),
            ])
            .unwrap();
            assert!(
                cli.args
                    .plan(temp.path(), Some(&temp.path().join("data")))
                    .is_err()
            );
        }
    }

    #[test]
    fn clone_config_is_read_only_and_disabled_by_default() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        assert!(!read_config(&config_path).unwrap().clone.enabled);
        assert!(!config_path.exists());
        std::fs::write(&config_path, "[clone]\nenabled = true\n").unwrap();
        assert!(read_config(&config_path).unwrap().clone.enabled);
        std::fs::write(&config_path, "[clone]\nenabled = 'invalid'\n").unwrap();
        assert!(read_config(&config_path).is_err());
    }
}
