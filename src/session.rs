use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::container::{
    self, ContainerError, ContainerRuntime, DEFAULT_SANDBOX_IMAGE, GitIdentity, RunSpec,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionError {
    #[error("git command failed: {0}")]
    GitFailed(String),
    #[error("Sandbox directory '{0}' already exists and is not empty")]
    SandboxDirNotEmpty(PathBuf),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Container(#[from] ContainerError),
}

/// Pure description of where a session lives, computed before anything is
/// created. Kept separate from `Session` so the command sequence is testable
/// without a container runtime.
#[derive(Debug, Clone)]
pub(crate) struct SandboxPlan {
    pub(crate) git_enabled: bool,
    pub(crate) repo_root: PathBuf,
    pub(crate) sandbox_dir: PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) container_name: String,
}

impl SandboxPlan {
    /// Host git commands that prepare the disposable clone. Empty in fallback
    /// (non-git) mode.
    pub(crate) fn git_commands(&self) -> Vec<Vec<String>> {
        let (Some(branch), true) = (&self.branch, self.git_enabled) else {
            return Vec::new();
        };

        let root = self.repo_root.to_string_lossy().into_owned();
        let sandbox = self.sandbox_dir.to_string_lossy().into_owned();
        vec![
            vec![
                "git".to_string(),
                "clone".to_string(),
                root,
                sandbox.clone(),
            ],
            vec![
                "git".to_string(),
                "-C".to_string(),
                sandbox,
                "checkout".to_string(),
                "-b".to_string(),
                branch.clone(),
            ],
        ]
    }
}

pub(crate) struct Session {
    pub(crate) sandbox_dir: PathBuf,
    pub(crate) repo_root: PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) main_branch: Option<String>,
    pub(crate) container_name: String,
    pub(crate) runtime: ContainerRuntime,
    pub(crate) git_enabled: bool,
}

impl Session {
    /// Boots a session rooted at `cwd`: plans the sandbox, clones the repo (if
    /// any), starts the long-lived container, and returns a handle.
    pub(crate) fn start(cwd: &Path) -> Result<Self, SessionError> {
        let timestamp = unix_timestamp();
        let plan = plan(cwd, timestamp)?;

        let runtime = ContainerRuntime::from_env()?;
        runtime.ensure_available()?;

        // Clean up strays from previous `kill -9` runs. Never removes dirs.
        match runtime.cleanup_orphans(&plan.repo_root) {
            Ok(ids) if !ids.is_empty() => {
                println!(
                    "Removed {} stray container(s) for {}: {}",
                    ids.len(),
                    plan.repo_root.display(),
                    ids.join(", ")
                );
            }
            Ok(_) => {}
            Err(err) => eprintln!("Warning: orphan cleanup failed: {err}"),
        }

        let main_branch = plan
            .git_enabled
            .then(|| git_current_branch(&plan.repo_root))
            .flatten();

        if plan.git_enabled {
            prepare_clone(&plan)?;
        }

        let (uid, gid) = host_uid_gid(&plan.repo_root)?;
        let image = container::env_nonempty("ANATOLY_SANDBOX_IMAGE")
            .unwrap_or_else(|| DEFAULT_SANDBOX_IMAGE.to_string());
        if !runtime.image_exists(&image) {
            eprintln!(
                "Warning: sandbox image '{image}' not found locally. \
                 Build it with `make sandbox-image RUNTIME={}`.",
                runtime.binary()
            );
        }

        let spec = RunSpec {
            name: plan.container_name.clone(),
            repo_root: plan.repo_root.clone(),
            image: image.clone(),
            sandbox_dir: plan.sandbox_dir.clone(),
            memory: container::env_nonempty("ANATOLY_SANDBOX_MEMORY")
                .unwrap_or_else(|| "4g".to_string()),
            cpus: container::env_nonempty("ANATOLY_SANDBOX_CPUS")
                .unwrap_or_else(|| "4".to_string()),
            uid,
            gid,
            git: git_identity(&plan.repo_root),
        };
        runtime.run_detached(&spec)?;

        let session = Session {
            sandbox_dir: plan.sandbox_dir,
            repo_root: plan.repo_root,
            branch: plan.branch,
            main_branch,
            container_name: plan.container_name,
            runtime,
            git_enabled: plan.git_enabled,
        };
        session.print_start(&spec.image);
        Ok(session)
    }

    fn print_start(&self, image: &str) {
        println!("\nSandbox session ready.");
        println!("  container: {} (image {image})", self.container_name);
        println!("  repo:      {}", self.repo_root.display());
        println!("  sandbox:   {}", self.sandbox_dir.display());
        if self.git_enabled {
            println!(
                "  branch:    {} (base: {})",
                self.branch.as_deref().unwrap_or("<none>"),
                self.main_branch.as_deref().unwrap_or("HEAD")
            );
        } else {
            println!("  branch:    <none> (not a git repository)");
        }
        println!();
    }

    /// Removes the container and prints consolidation instructions. The sandbox
    /// directory and branch are intentionally preserved.
    pub(crate) fn shutdown(&self) {
        println!("\n\nRemoving container...");
        if let Err(err) = self.runtime.remove_force(&self.container_name) {
            eprintln!(
                "Warning: failed to remove container '{}': {err}",
                self.container_name
            );
        }
        self.print_consolidation();
    }

    fn print_consolidation(&self) {
        println!(
            "\nSession ended. Container '{}' removed.",
            self.container_name
        );
        if !self.git_enabled {
            println!(
                "Fallback mode (not a git repository): working directory was {}",
                self.sandbox_dir.display()
            );
            return;
        }

        let sandbox = self.sandbox_dir.display();
        let branch = self.branch.as_deref().unwrap_or("anatoly/<session>");
        let base = self.main_branch.as_deref().unwrap_or("main");
        println!("The sandbox clone and branch were preserved:");
        println!("  sandbox: {sandbox}");
        println!("  branch:  {branch}");
        println!("\nTo review and consolidate on the host:");
        println!("  git fetch {sandbox} 'refs/heads/*:refs/remotes/anatoly/*'");
        println!("  git log {base}..{branch}      # review; `git diff {base}..{branch}` likewise");
        println!("  git merge --no-ff {branch}    # or rebase / cherry-pick");
        println!("  rm -rf {sandbox}              # when done");
    }
}

/// Computes the session plan for `cwd` at a given unix timestamp.
pub(crate) fn plan(cwd: &Path, timestamp: u64) -> Result<SandboxPlan, SessionError> {
    let container_name = format!("anatoly-{timestamp}");

    match git_toplevel(cwd) {
        Some(repo_root) => {
            let sandbox_dir = sandbox_dir_for(&repo_root, timestamp);
            Ok(SandboxPlan {
                git_enabled: true,
                repo_root,
                sandbox_dir,
                branch: Some(format!("anatoly/{timestamp}")),
                container_name,
            })
        }
        None => Ok(SandboxPlan {
            git_enabled: false,
            repo_root: cwd.to_path_buf(),
            sandbox_dir: cwd.to_path_buf(),
            branch: None,
            container_name,
        }),
    }
}

fn prepare_clone(plan: &SandboxPlan) -> Result<(), SessionError> {
    if plan.sandbox_dir.exists() {
        let mut entries = plan.sandbox_dir.read_dir()?;
        if entries.next().is_some() {
            return Err(SessionError::SandboxDirNotEmpty(plan.sandbox_dir.clone()));
        }
    }

    for argv in plan.git_commands() {
        run_git(&argv)?;
    }
    Ok(())
}

fn sandbox_dir_for(repo_root: &Path, timestamp: u64) -> PathBuf {
    if let Some(dir) = container::env_nonempty("ANATOLY_SANDBOX_DIR") {
        return PathBuf::from(dir);
    }

    let parent = repo_root.parent().unwrap_or(repo_root);
    let name = repo_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    parent.join(format!("{name}-sandbox-{timestamp}"))
}

fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn git_current_branch(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

fn git_config(repo_root: &Path, key: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn git_identity(repo_root: &Path) -> GitIdentity {
    GitIdentity {
        name: container::env_nonempty("ANATOLY_GIT_AUTHOR_NAME")
            .or_else(|| git_config(repo_root, "user.name"))
            .unwrap_or_else(|| "anatoly".to_string()),
        email: container::env_nonempty("ANATOLY_GIT_AUTHOR_EMAIL")
            .or_else(|| git_config(repo_root, "user.email"))
            .unwrap_or_else(|| "anatoly@localhost".to_string()),
    }
}

fn run_git(argv: &[String]) -> Result<(), SessionError> {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    let output = command.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(SessionError::GitFailed(format!(
            "{}: {}",
            argv.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(unix)]
fn host_uid_gid(path: &Path) -> Result<(u32, u32), SessionError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.uid(), metadata.gid()))
}

#[cfg(not(unix))]
fn host_uid_gid(_path: &Path) -> Result<(u32, u32), SessionError> {
    Ok((1000, 1000))
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    #[test]
    fn plan_falls_back_outside_a_git_repo() {
        let temp = temp_dir::TempDir::new().expect("temp dir");
        let plan = plan(temp.path(), 1_700_000_000).expect("plan");

        assert!(!plan.git_enabled);
        assert_eq!(plan.sandbox_dir, temp.path());
        assert_eq!(plan.branch, None);
        assert_eq!(plan.container_name, "anatoly-1700000000");
        assert!(plan.git_commands().is_empty());
    }

    #[test]
    fn plan_git_mode_command_sequence() {
        let temp = temp_dir::TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).expect("mkdir repo");
        let status = Command::new("git")
            .arg("init")
            .arg(&repo)
            .output()
            .expect("git init");
        assert!(status.status.success());

        let mut settings = insta::Settings::clone_current();
        settings.add_filter(
            &temp.path().canonicalize().unwrap().to_string_lossy(),
            "[TMP]",
        );
        let _guard = settings.bind_to_scope();

        let plan = plan(&repo, 1_700_000_000).expect("plan");
        assert!(plan.git_enabled);
        assert_eq!(plan.branch.as_deref(), Some("anatoly/1700000000"));
        assert_eq!(plan.container_name, "anatoly-1700000000");
        assert_eq!(
            plan.sandbox_dir.file_name().unwrap().to_string_lossy(),
            "repo-sandbox-1700000000"
        );
        assert_eq!(
            plan.sandbox_dir.parent(),
            repo.canonicalize().unwrap().parent()
        );
        assert_snapshot!(serde_json::to_string_pretty(&plan.git_commands()).unwrap(), @r#"
        [
          [
            "git",
            "clone",
            "[TMP]/repo",
            "[TMP]/repo-sandbox-1700000000"
          ],
          [
            "git",
            "-C",
            "[TMP]/repo-sandbox-1700000000",
            "checkout",
            "-b",
            "anatoly/1700000000"
          ]
        ]
        "#);
    }

    /// Returns the runtime + image when both are usable, otherwise `None` so
    /// the suite still passes on machines without a running podman machine.
    fn gated() -> Option<(ContainerRuntime, String)> {
        let runtime = ContainerRuntime::from_env().ok()?;
        let image = container::env_nonempty("ANATOLY_SANDBOX_IMAGE")
            .unwrap_or_else(|| DEFAULT_SANDBOX_IMAGE.to_string());
        runtime.image_exists(&image).then_some((runtime, image))
    }

    #[test]
    fn cleanup_orphans_removes_labeled_stray() {
        let Some((runtime, image)) = gated() else {
            eprintln!("skipping: no container runtime or sandbox image");
            return;
        };

        let temp = temp_dir::TempDir::new().expect("temp dir");
        let (uid, gid) = host_uid_gid(temp.path()).expect("uid/gid");
        let spec = RunSpec {
            name: format!("anatoly-orphan-test-{}", std::process::id()),
            repo_root: temp.path().to_path_buf(),
            image,
            sandbox_dir: temp.path().to_path_buf(),
            memory: "256m".to_string(),
            cpus: "1".to_string(),
            uid,
            gid,
            git: GitIdentity {
                name: "test".to_string(),
                email: "test@example.com".to_string(),
            },
        };

        runtime.run_detached(&spec).expect("start stray");
        let removed = runtime
            .cleanup_orphans(temp.path())
            .expect("cleanup orphans");
        assert!(
            !removed.is_empty(),
            "expected cleanup to remove the stray container, got {removed:?}"
        );
        let again = runtime
            .cleanup_orphans(temp.path())
            .expect("cleanup orphans");
        assert!(again.is_empty(), "expected no orphans, got {again:?}");
    }

    #[tokio::test]
    async fn session_keeps_host_repo_untouched() {
        if gated().is_none() {
            eprintln!("skipping: no container runtime or sandbox image");
            return;
        }

        let temp = temp_dir::TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).expect("mkdir repo");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("run git");
            assert!(output.status.success(), "git {args:?} failed");
        };
        git(&["init"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.com"]);
        std::fs::write(repo.join("tracked.txt"), "original\n").expect("write tracked");
        git(&["add", "tracked.txt"]);
        git(&["commit", "-m", "initial"]);

        let session = Session::start(&repo).expect("start session");

        let result = session
            .runtime
            .exec(
                &session.container_name,
                "echo changed >> tracked.txt && echo ok",
                std::time::Duration::from_secs(30),
            )
            .await
            .expect("exec");
        assert!(result.success, "exec failed: {:?}", result.stderr);

        // Host repo untouched: the tracked file still has its committed content.
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).expect("read tracked"),
            "original\n"
        );

        session.shutdown();
        let _ = std::fs::remove_dir_all(&session.sandbox_dir);
    }
}
