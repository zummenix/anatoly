use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};
use std::time::Duration;

use tokio::process::Command as TokioCommand;

/// Default sandbox image tag. Pinned on purpose (no `:latest`).
pub(crate) const DEFAULT_SANDBOX_IMAGE: &str = "anatoly-sandbox:0.1";

/// Label used to associate a container with the host repository it serves.
const REPO_LABEL: &str = "anatoly.repo";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeKind {
    Podman,
    Docker,
}

impl RuntimeKind {
    pub(crate) fn binary(self) -> &'static str {
        match self {
            RuntimeKind::Podman => "podman",
            RuntimeKind::Docker => "docker",
        }
    }

    /// Runtimes the `auto` mode probes, in priority order.
    const AUTO_ORDER: [RuntimeKind; 2] = [RuntimeKind::Podman, RuntimeKind::Docker];
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ContainerError {
    #[error("Container runtime '{0}' is not available: {1}")]
    RuntimeUnavailable(String, String),
    #[error(
        "No supported container runtime found (tried podman, docker). \
         Install one, or set ANATOLY_RUNTIME to 'podman' or 'docker'."
    )]
    NoRuntimeFound,
    #[error("Invalid ANATOLY_RUNTIME value '{0}': expected 'auto', 'podman' or 'docker'")]
    InvalidRuntime(String),
    #[error("Failed to run container runtime '{rt}': {source}")]
    Io {
        rt: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("Container command timed out after {0}s")]
    TimedOut(u64),
    #[error("Container command failed: {0}")]
    CommandFailed(String),
}

/// Host git identity forwarded to the container so agent commits are
/// attributable. Never derived from, and never carrying, host secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitIdentity {
    pub(crate) name: String,
    pub(crate) email: String,
}

/// Declarative description of the long-lived sandbox container.
#[derive(Debug, Clone)]
pub(crate) struct RunSpec {
    pub(crate) name: String,
    /// Host repository root this container belongs to (used for the label).
    pub(crate) repo_root: PathBuf,
    pub(crate) image: String,
    /// The one and only read-write bind mount, mounted at its own absolute path.
    pub(crate) sandbox_dir: PathBuf,
    pub(crate) memory: String,
    pub(crate) cpus: String,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) git: GitIdentity,
}

impl RunSpec {
    pub(crate) fn argv(&self, rt: RuntimeKind) -> Vec<String> {
        run_argv(rt.binary(), self)
    }
}

/// Builds the `run` argv. This is the security-critical vector: exactly one
/// read-write bind mount (the sandbox at its own path), no network, and only
/// explicit `-e KEY=VALUE` entries (never a bare `-e` that would forward host
/// environment, and never any host secret).
pub(crate) fn run_argv(rt: &str, spec: &RunSpec) -> Vec<String> {
    let sandbox = spec.sandbox_dir.to_string_lossy().into_owned();
    vec![
        rt.to_string(),
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        spec.name.clone(),
        "--label".to_string(),
        format!("{REPO_LABEL}={}", spec.repo_root.display()),
        "--read-only".to_string(),
        "--tmpfs".to_string(),
        "/tmp:size=4g".to_string(),
        "--memory".to_string(),
        spec.memory.clone(),
        "--cpus".to_string(),
        spec.cpus.clone(),
        "--security-opt".to_string(),
        "no-new-privileges:true".to_string(),
        "--user".to_string(),
        format!("{}:{}", spec.uid, spec.gid),
        "--network".to_string(),
        "none".to_string(),
        "-e".to_string(),
        "HOME=/tmp".to_string(),
        "-e".to_string(),
        "DEBIAN_FRONTEND=noninteractive".to_string(),
        "-e".to_string(),
        "CARGO_TARGET_DIR=/tmp/target".to_string(),
        "-e".to_string(),
        format!("GIT_AUTHOR_NAME={}", spec.git.name),
        "-e".to_string(),
        format!("GIT_AUTHOR_EMAIL={}", spec.git.email),
        "-e".to_string(),
        format!("GIT_COMMITTER_NAME={}", spec.git.name),
        "-e".to_string(),
        format!("GIT_COMMITTER_EMAIL={}", spec.git.email),
        "-v".to_string(),
        format!("{sandbox}:{sandbox}"),
        "-w".to_string(),
        sandbox,
        spec.image.clone(),
        "sleep".to_string(),
        "infinity".to_string(),
    ]
}

pub(crate) fn exec_argv(rt: &str, name: &str, cmd: &str) -> Vec<String> {
    vec![
        rt.to_string(),
        "exec".to_string(),
        "-i".to_string(),
        name.to_string(),
        "bash".to_string(),
        "-c".to_string(),
        cmd.to_string(),
    ]
}

pub(crate) fn remove_argv(rt: &str, name: &str) -> Vec<String> {
    vec![
        rt.to_string(),
        "rm".to_string(),
        "-f".to_string(),
        name.to_string(),
    ]
}

/// Lists container ids carrying the `anatoly.repo` label. Also used by tests to
/// build the docker-compatible argv contract.
pub(crate) fn list_argv(rt: &str, repo_root: &Path) -> Vec<String> {
    vec![
        rt.to_string(),
        "ps".to_string(),
        "-aq".to_string(),
        "--filter".to_string(),
        format!("label={REPO_LABEL}={}", repo_root.display()),
    ]
}

#[derive(Debug)]
pub(crate) struct ExecOutput {
    /// Exit code, when the process exited normally. Read by tests and available
    /// to callers that want to distinguish exit codes.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) status: Option<i32>,
    pub(crate) success: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Thin wrapper over a container runtime CLI. Podman and docker share the flags
/// used here, so choosing a runtime is a configuration value, not a code fork.
#[derive(Debug, Clone)]
pub(crate) struct ContainerRuntime {
    kind: RuntimeKind,
}

impl ContainerRuntime {
    pub(crate) fn binary(&self) -> &'static str {
        self.kind.binary()
    }

    /// Constructs a handle for a specific runtime without probing availability.
    /// Used by unit tests that only inspect argv.
    #[cfg(test)]
    pub(crate) fn for_kind(kind: RuntimeKind) -> Self {
        Self { kind }
    }

    /// Selects the runtime from `ANATOLY_RUNTIME` (default `auto`).
    pub(crate) fn from_env() -> Result<Self, ContainerError> {
        let choice = env_nonempty("ANATOLY_RUNTIME").unwrap_or_else(|| "auto".to_string());
        match choice.as_str() {
            "podman" => {
                ensure_available(RuntimeKind::Podman)?;
                Ok(Self {
                    kind: RuntimeKind::Podman,
                })
            }
            "docker" => {
                ensure_available(RuntimeKind::Docker)?;
                Ok(Self {
                    kind: RuntimeKind::Docker,
                })
            }
            "auto" => {
                for kind in RuntimeKind::AUTO_ORDER {
                    if probe(kind).is_ok() {
                        return Ok(Self { kind });
                    }
                }
                Err(ContainerError::NoRuntimeFound)
            }
            other => Err(ContainerError::InvalidRuntime(other.to_string())),
        }
    }

    pub(crate) fn ensure_available(&self) -> Result<(), ContainerError> {
        ensure_available(self.kind)
    }

    pub(crate) fn run_detached(&self, spec: &RunSpec) -> Result<(), ContainerError> {
        let argv = spec.argv(self.kind);
        let output = run_sync(&argv, self.kind)?;
        if output.status.success() {
            Ok(())
        } else {
            let mut stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            stderr.push_str(&format!(
                "\nHint: is the image built? Try `make sandbox-image RUNTIME={}`.",
                self.kind.binary()
            ));
            if self.kind == RuntimeKind::Podman && cfg!(target_os = "macos") {
                stderr.push_str(
                    "\nOn macOS, make sure the podman machine is running: `podman machine start`.",
                );
            }
            Err(ContainerError::CommandFailed(stderr))
        }
    }

    pub(crate) async fn exec(
        &self,
        name: &str,
        cmd: &str,
        timeout: Duration,
    ) -> Result<ExecOutput, ContainerError> {
        let argv = exec_argv(self.kind.binary(), name, cmd);
        let mut command = TokioCommand::new(&argv[0]);
        command.args(&argv[1..]);
        // Cancellation (timeout) must not leave the exec running.
        command.kill_on_drop(true);

        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| ContainerError::TimedOut(timeout.as_secs()))?
            .map_err(|source| ContainerError::Io {
                rt: self.kind.binary(),
                source,
            })?;

        Ok(ExecOutput {
            status: output.status.code(),
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    pub(crate) fn remove_force(&self, name: &str) -> Result<(), ContainerError> {
        let argv = remove_argv(self.kind.binary(), name);
        let output = run_sync(&argv, self.kind)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ContainerError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }

    /// Removes stray containers labelled with `repo_root` (e.g. left behind by
    /// `kill -9`). Returns the ids that were removed.
    pub(crate) fn cleanup_orphans(&self, repo_root: &Path) -> Result<Vec<String>, ContainerError> {
        let argv = list_argv(self.kind.binary(), repo_root);
        let output = run_sync(&argv, self.kind)?;
        if !output.status.success() {
            return Err(ContainerError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }

        let ids: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect();

        for id in &ids {
            self.remove_force(id)?;
        }
        Ok(ids)
    }

    /// Whether the sandbox image exists locally.
    pub(crate) fn image_exists(&self, image: &str) -> bool {
        let argv = vec![
            self.kind.binary().to_string(),
            "image".to_string(),
            "exists".to_string(),
            image.to_string(),
        ];
        run_sync(&argv, self.kind).is_ok_and(|o| o.status.success())
    }
}

/// Probes `ANATOLY_RUNTIME` handling; kept public-in-crate for tests.
pub(crate) fn probe(kind: RuntimeKind) -> Result<(), String> {
    let output = StdCommand::new(kind.binary()).arg("version").output();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if stderr.is_empty() {
                format!("`{} version` exited with {}", kind.binary(), output.status)
            } else {
                stderr
            })
        }
        Err(err) => Err(err.to_string()),
    }
}

pub(crate) fn ensure_available(kind: RuntimeKind) -> Result<(), ContainerError> {
    if probe(kind).is_ok() {
        return Ok(());
    }

    let mut reason = format!("`{} version` failed", kind.binary());
    if kind == RuntimeKind::Podman && cfg!(target_os = "macos") {
        reason.push_str(". On macOS, start the podman machine first: `podman machine start`");
    }
    Err(ContainerError::RuntimeUnavailable(
        kind.binary().to_string(),
        reason,
    ))
}

fn run_sync(argv: &[String], kind: RuntimeKind) -> Result<Output, ContainerError> {
    let mut command = StdCommand::new(&argv[0]);
    command.args(&argv[1..]);
    command.output().map_err(|source| ContainerError::Io {
        rt: kind.binary(),
        source,
    })
}

pub(crate) fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    fn sample_spec() -> RunSpec {
        RunSpec {
            name: "anatoly-1700000000".to_string(),
            repo_root: PathBuf::from("/home/user/project"),
            image: DEFAULT_SANDBOX_IMAGE.to_string(),
            sandbox_dir: PathBuf::from("/home/user/project-sandbox-1700000000"),
            memory: "4g".to_string(),
            cpus: "4".to_string(),
            uid: 1000,
            gid: 1000,
            git: GitIdentity {
                name: "Code Assistant".to_string(),
                email: "agent@example.com".to_string(),
            },
        }
    }

    #[test]
    fn run_argv_is_stable() {
        let argv = sample_spec().argv(RuntimeKind::Podman);
        assert_snapshot!(serde_json::to_string_pretty(&argv).unwrap(), @r#"
        [
          "podman",
          "run",
          "-d",
          "--name",
          "anatoly-1700000000",
          "--label",
          "anatoly.repo=/home/user/project",
          "--read-only",
          "--tmpfs",
          "/tmp:size=4g",
          "--memory",
          "4g",
          "--cpus",
          "4",
          "--security-opt",
          "no-new-privileges:true",
          "--user",
          "1000:1000",
          "--network",
          "none",
          "-e",
          "HOME=/tmp",
          "-e",
          "DEBIAN_FRONTEND=noninteractive",
          "-e",
          "CARGO_TARGET_DIR=/tmp/target",
          "-e",
          "GIT_AUTHOR_NAME=Code Assistant",
          "-e",
          "GIT_AUTHOR_EMAIL=agent@example.com",
          "-e",
          "GIT_COMMITTER_NAME=Code Assistant",
          "-e",
          "GIT_COMMITTER_EMAIL=agent@example.com",
          "-v",
          "/home/user/project-sandbox-1700000000:/home/user/project-sandbox-1700000000",
          "-w",
          "/home/user/project-sandbox-1700000000",
          "anatoly-sandbox:0.1",
          "sleep",
          "infinity"
        ]
        "#);
    }

    #[test]
    fn run_argv_differs_only_by_binary() {
        let podman = sample_spec().argv(RuntimeKind::Podman);
        let docker = sample_spec().argv(RuntimeKind::Docker);
        assert_eq!(podman[0], "podman");
        assert_eq!(docker[0], "docker");
        assert_eq!(podman[1..], docker[1..]);
    }

    #[test]
    fn exec_argv_is_stable() {
        let argv = exec_argv("podman", "anatoly-1700000000", "ls -la && git status");
        assert_snapshot!(serde_json::to_string_pretty(&argv).unwrap(), @r#"
        [
          "podman",
          "exec",
          "-i",
          "anatoly-1700000000",
          "bash",
          "-c",
          "ls -la && git status"
        ]
        "#);
    }

    #[test]
    fn remove_and_list_argv_are_stable() {
        let remove = remove_argv("podman", "anatoly-1700000000");
        assert_snapshot!(serde_json::to_string_pretty(&remove).unwrap(), @r#"
        [
          "podman",
          "rm",
          "-f",
          "anatoly-1700000000"
        ]
        "#);

        let list = list_argv("podman", Path::new("/home/user/project"));
        assert_snapshot!(serde_json::to_string_pretty(&list).unwrap(), @r#"
        [
          "podman",
          "ps",
          "-aq",
          "--filter",
          "label=anatoly.repo=/home/user/project"
        ]
        "#);
    }

    /// The runtime argv must never forward host environment: every `-e` has an
    /// explicit `KEY=VALUE`, and nothing may mention host secrets.
    #[test]
    fn run_argv_never_forwards_host_env() {
        let argv = sample_spec().argv(RuntimeKind::Podman);

        for (idx, arg) in argv.iter().enumerate() {
            assert!(
                !arg.contains("OPENROUTER"),
                "argv leaks host secret-looking value: {arg}"
            );
            if arg == "-e" {
                let value = argv.get(idx + 1).expect("-e must be followed by KEY=VALUE");
                assert!(
                    value.contains('='),
                    "bare `-e` would forward host environment: {value}"
                );
            }
        }
    }

    /// Exactly one read-write bind mount: the sandbox, at its own absolute path.
    #[test]
    fn run_argv_has_single_rw_mount_and_no_network() {
        let argv = sample_spec().argv(RuntimeKind::Podman);

        let mounts: Vec<&String> = argv
            .iter()
            .enumerate()
            .filter(|(_, arg)| *arg == "-v")
            .map(|(idx, _)| &argv[idx + 1])
            .collect();
        assert_eq!(mounts.len(), 1, "expected exactly one bind mount");
        assert_eq!(
            mounts[0],
            "/home/user/project-sandbox-1700000000:/home/user/project-sandbox-1700000000"
        );

        let network_idx = argv
            .iter()
            .position(|arg| arg == "--network")
            .expect("--network present");
        assert_eq!(argv[network_idx + 1], "none");
    }

    /// Gated integration test: exec round-trip, exit-code propagation, and files
    /// written in the sandbox being visible on the host.
    #[tokio::test]
    async fn gated_exec_round_trip() {
        use std::os::unix::fs::MetadataExt;

        let Some(runtime) = ContainerRuntime::from_env().ok() else {
            eprintln!("skipping: no container runtime");
            return;
        };
        let image = env_nonempty("ANATOLY_SANDBOX_IMAGE")
            .unwrap_or_else(|| DEFAULT_SANDBOX_IMAGE.to_string());
        if !runtime.image_exists(&image) {
            eprintln!("skipping: sandbox image '{image}' not built");
            return;
        }

        let temp = temp_dir::TempDir::new().expect("temp dir");
        let metadata = std::fs::metadata(temp.path()).expect("metadata");
        let spec = RunSpec {
            name: format!("anatoly-exec-test-{}", std::process::id()),
            repo_root: temp.path().to_path_buf(),
            image,
            sandbox_dir: temp.path().to_path_buf(),
            memory: "256m".to_string(),
            cpus: "1".to_string(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            git: GitIdentity {
                name: "test".to_string(),
                email: "test@example.com".to_string(),
            },
        };
        runtime.run_detached(&spec).expect("start container");

        let ok = runtime
            .exec(&spec.name, "echo hello", Duration::from_secs(30))
            .await
            .expect("exec");
        assert!(ok.success);
        assert_eq!(ok.status, Some(0));
        assert_eq!(ok.stdout, b"hello\n");

        // Invariant: no host secret (e.g. OPENROUTER_API_KEY) is visible inside
        // the container. Only the explicitly listed `-e` entries exist.
        let env = runtime
            .exec(&spec.name, "env", Duration::from_secs(30))
            .await
            .expect("exec");
        let env = String::from_utf8_lossy(&env.stdout);
        assert!(!env.contains("OPENROUTER"), "container env leaked: {env}");
        assert!(env.contains("HOME=/tmp"));

        let failed = runtime
            .exec(&spec.name, "exit 3", Duration::from_secs(30))
            .await
            .expect("exec");
        assert!(!failed.success);
        assert_eq!(failed.status, Some(3));

        runtime
            .exec(
                &spec.name,
                "echo from-container > sandbox_file.txt",
                Duration::from_secs(30),
            )
            .await
            .expect("exec");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("sandbox_file.txt")).expect("read file"),
            "from-container\n"
        );

        runtime.remove_force(&spec.name).expect("remove container");
    }
}
