use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::container::{
    self, ContainerError, ContainerRuntime, DEFAULT_SANDBOX_IMAGE, GitIdentity, RunSpec,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionError {
    #[error("git command failed: {0}")]
    GitFailed(String),
    #[error("Session {0} '{1}' already exists")]
    NameCollision(&'static str, String),
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
    pub(crate) root_dir: PathBuf,
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

        let root = self.root_dir.to_string_lossy().into_owned();
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
    pub(crate) root_dir: PathBuf,
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
        let suffix = random_suffix();
        let plan = plan(cwd, &suffix)?;

        let runtime = ContainerRuntime::from_env()?;
        runtime.ensure_available()?;
        check_name_collisions(&plan, &runtime)?;

        // Clean up strays from previous `kill -9` runs. Never removes dirs.
        match runtime.cleanup_orphans(&plan.root_dir) {
            Ok(ids) if !ids.is_empty() => {
                println!(
                    "Removed {} stray container(s) for {}: {}",
                    ids.len(),
                    plan.root_dir.display(),
                    ids.join(", ")
                );
            }
            Ok(_) => {}
            Err(err) => eprintln!("Warning: orphan cleanup failed: {err}"),
        }

        let main_branch = plan
            .git_enabled
            .then(|| git_current_branch(&plan.root_dir))
            .flatten();

        if plan.git_enabled {
            prepare_clone(&plan)?;
        } else {
            prepare_copy(&plan)?;
        }

        let session = (|| {
            let (uid, gid) = host_uid_gid(&plan.sandbox_dir)?;
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
                repo_root: plan.root_dir.clone(),
                image: image.clone(),
                sandbox_dir: plan.sandbox_dir.clone(),
                memory: container::env_nonempty("ANATOLY_SANDBOX_MEMORY")
                    .unwrap_or_else(|| "4g".to_string()),
                cpus: container::env_nonempty("ANATOLY_SANDBOX_CPUS")
                    .unwrap_or_else(|| "4".to_string()),
                uid,
                gid,
                git: git_identity(&plan.root_dir),
            };
            runtime.run_detached(&spec)?;

            Ok::<_, SessionError>((
                Session {
                    sandbox_dir: plan.sandbox_dir.clone(),
                    root_dir: plan.root_dir.clone(),
                    branch: plan.branch.clone(),
                    main_branch,
                    container_name: plan.container_name.clone(),
                    runtime,
                    git_enabled: plan.git_enabled,
                },
                image,
            ))
        })();
        let (session, image) = match session {
            Ok(session) => session,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&plan.sandbox_dir);
                return Err(err);
            }
        };
        session.print_start(&image);
        Ok(session)
    }

    fn print_start(&self, image: &str) {
        println!("\nSandbox session ready.");
        println!("  container: {} (image {image})", self.container_name);
        println!("  root:      {}", self.root_dir.display());
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
                "Fallback mode (not a git repository): a copy of {} is preserved at {}",
                self.root_dir.display(),
                self.sandbox_dir.display()
            );
            return;
        }

        let sandbox = self.sandbox_dir.display();
        let branch = self.branch.as_deref().unwrap_or("<session>");
        let remote_branch = format!("refs/remotes/{branch}");
        let base = self.main_branch.as_deref().unwrap_or("main");
        println!("The sandbox clone and branch were preserved:");
        println!("  sandbox: {sandbox}");
        println!("  branch:  {branch}");
        println!("\nTo review and consolidate on the host:");
        println!("  git fetch {sandbox} '{branch}:{remote_branch}'");
        println!(
            "  git log {base}..{remote_branch}      # review; `git diff {base}..{remote_branch}` likewise"
        );
        println!("  git merge --no-ff {remote_branch}    # or rebase / cherry-pick");
        println!("  rm -rf {sandbox}              # when done");
    }
}

/// Computes the session plan for `cwd` using the provided unique suffix.
pub(crate) fn plan(cwd: &Path, suffix: &str) -> Result<SandboxPlan, SessionError> {
    match git_toplevel(cwd) {
        Some(repo_root) => {
            let name = session_name(&repo_root, suffix);
            let sandbox_dir = sandbox_dir_for(&repo_root, &name);
            Ok(SandboxPlan {
                git_enabled: true,
                root_dir: repo_root,
                sandbox_dir,
                branch: Some(name.clone()),
                container_name: name,
            })
        }
        None => {
            let name = session_name(cwd, suffix);
            Ok(SandboxPlan {
                git_enabled: false,
                root_dir: cwd.to_path_buf(),
                sandbox_dir: std::env::temp_dir().canonicalize()?.join(&name),
                branch: None,
                container_name: name,
            })
        }
    }
}

fn check_name_collisions(
    plan: &SandboxPlan,
    runtime: &ContainerRuntime,
) -> Result<(), SessionError> {
    if plan.sandbox_dir.try_exists()? {
        return Err(SessionError::NameCollision(
            "sandbox directory",
            plan.sandbox_dir.display().to_string(),
        ));
    }
    if let Some(branch) = &plan.branch
        && branch_exists(&plan.root_dir, branch)?
    {
        return Err(SessionError::NameCollision("branch", branch.clone()));
    }
    if runtime.container_exists(&plan.container_name)? {
        return Err(SessionError::NameCollision(
            "container",
            plan.container_name.clone(),
        ));
    }
    Ok(())
}

fn prepare_clone(plan: &SandboxPlan) -> Result<(), SessionError> {
    std::fs::create_dir(&plan.sandbox_dir)?;
    let result = (|| {
        for argv in plan.git_commands() {
            run_git(&argv)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&plan.sandbox_dir);
    }
    result
}

fn prepare_copy(plan: &SandboxPlan) -> Result<(), SessionError> {
    let root_dir = plan.root_dir.canonicalize()?;
    if plan.sandbox_dir.starts_with(&root_dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "temporary sandbox directory is inside the source directory",
        )
        .into());
    }

    std::fs::create_dir(&plan.sandbox_dir)?;
    let result = copy_directory_contents(&plan.root_dir, &plan.sandbox_dir);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&plan.sandbox_dir);
    }
    result
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<(), SessionError> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source_path)?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            copy_symlink(&source_path, &destination_path)?;
        } else if file_type.is_dir() {
            std::fs::create_dir(&destination_path)?;
            copy_directory_contents(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&source_path, &destination_path)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("unsupported file type: {}", source_path.display()),
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<(), SessionError> {
    std::os::unix::fs::symlink(std::fs::read_link(source)?, destination)?;
    Ok(())
}

#[cfg(windows)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<(), SessionError> {
    use std::os::windows::fs::{symlink_dir, symlink_file};

    let target = std::fs::read_link(source)?;
    if std::fs::metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
        symlink_dir(target, destination)?;
    } else {
        symlink_file(target, destination)?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn copy_symlink(_source: &Path, _destination: &Path) -> Result<(), SessionError> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "copying symbolic links is not supported on this platform",
    )
    .into())
}

fn session_name(root_dir: &Path, suffix: &str) -> String {
    let project = root_dir
        .file_name()
        .map(|name| name.to_string_lossy())
        .map(|name| project_slug(&name))
        .unwrap_or_else(|| "project".to_string());
    format!("anatoly-{project}-{suffix}")
}

fn project_slug(name: &str) -> String {
    let mut slug = String::new();
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() == 48 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "project".to_string()
    } else {
        slug
    }
}

fn sandbox_dir_for(root_dir: &Path, name: &str) -> PathBuf {
    if let Some(dir) = container::env_nonempty("ANATOLY_SANDBOX_DIR") {
        return PathBuf::from(dir);
    }

    let parent = root_dir.parent().unwrap_or(root_dir);
    parent.join(name)
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

fn branch_exists(repo_root: &Path, branch: &str) -> Result<bool, SessionError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show-ref", "--quiet", "--verify"])
        .arg(format!("refs/heads/{branch}"))
        .output()?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(SessionError::GitFailed(format!(
            "git show-ref failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
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

fn random_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);

    let mut value = hasher.finish();
    let mut suffix = String::with_capacity(8);
    for _ in 0..8 {
        suffix.push(char::from(b'a' + (value % 26) as u8));
        value /= 26;
    }
    suffix
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    #[test]
    fn plan_falls_back_outside_a_git_repo() {
        let temp = temp_dir::TempDir::new().expect("temp dir");
        let plan = plan(temp.path(), "abcdefgh").expect("plan");

        assert!(!plan.git_enabled);
        assert_eq!(plan.root_dir, temp.path());
        assert_ne!(plan.sandbox_dir, temp.path());
        assert!(plan.sandbox_dir.is_absolute());
        assert_eq!(
            plan.sandbox_dir.parent(),
            Some(std::env::temp_dir().canonicalize().unwrap().as_path())
        );
        assert_eq!(plan.branch, None);
        assert!(plan.container_name.starts_with("anatoly-"));
        assert!(plan.container_name.ends_with("-abcdefgh"));
        assert_eq!(
            plan.sandbox_dir.file_name().unwrap().to_string_lossy(),
            plan.container_name
        );
        assert!(plan.git_commands().is_empty());
    }

    #[test]
    fn session_name_uses_a_normalized_project_and_lowercase_suffix() {
        let root = Path::new("/tmp/My Wallet");
        let name = session_name(root, "abcdefgh");
        let suffix = random_suffix();

        assert_eq!(name, "anatoly-my-wallet-abcdefgh");
        assert_eq!(suffix.len(), 8);
        assert!(suffix.bytes().all(|byte| byte.is_ascii_lowercase()));
    }

    #[test]
    fn fallback_copy_is_independent_and_preserves_symlinks() {
        let temp = temp_dir::TempDir::new().expect("temp dir");
        let source = temp.path().join("source");
        let destination = temp.path().join("copy");
        std::fs::create_dir(&source).expect("mkdir source");
        std::fs::create_dir(source.join("nested")).expect("mkdir nested");
        std::fs::write(source.join("nested/file.txt"), "original").expect("write source");
        let outside = temp.path().join("outside.txt");
        std::fs::write(&outside, "outside").expect("write outside");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, source.join("outside-link")).expect("create symlink");

        let plan = SandboxPlan {
            git_enabled: false,
            root_dir: source.clone(),
            sandbox_dir: destination.clone(),
            branch: None,
            container_name: "anatoly-test".to_string(),
        };
        prepare_copy(&plan).expect("prepare copy");

        assert_eq!(
            std::fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "original"
        );
        std::fs::write(destination.join("nested/file.txt"), "changed").expect("edit copy");
        assert_eq!(
            std::fs::read_to_string(source.join("nested/file.txt")).unwrap(),
            "original"
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(destination.join("outside-link")).unwrap(),
            outside
        );
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

        let plan = plan(&repo, "abcdefgh").expect("plan");
        assert!(plan.git_enabled);
        assert_eq!(plan.branch.as_deref(), Some("anatoly-repo-abcdefgh"));
        assert_eq!(plan.container_name, "anatoly-repo-abcdefgh");
        assert_eq!(
            plan.sandbox_dir.file_name().unwrap().to_string_lossy(),
            "anatoly-repo-abcdefgh"
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
            "[TMP]/anatoly-repo-abcdefgh"
          ],
          [
            "git",
            "-C",
            "[TMP]/anatoly-repo-abcdefgh",
            "checkout",
            "-b",
            "anatoly-repo-abcdefgh"
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

        // Regression guard: git must work inside the container. On podman
        // machine the mount root can appear root-owned, which without an
        // injected `safe.directory` fails every git command with exit 128.
        let git = session
            .runtime
            .exec(
                &session.container_name,
                "echo x > note.txt && git add note.txt && git commit -q -m note \
                 && git log -1 --format='%cn <%ce>'",
                std::time::Duration::from_secs(30),
            )
            .await
            .expect("exec git");
        assert!(
            git.success,
            "git must work inside the sandbox: {}",
            String::from_utf8_lossy(&git.stderr)
        );
        assert!(
            String::from_utf8_lossy(&git.stdout).contains("test <test@example.com>"),
            "commit identity must come from the repo config: {}",
            String::from_utf8_lossy(&git.stdout)
        );

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
