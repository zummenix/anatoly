use crate::container::{ContainerError, ContainerRuntime};
use crate::utils::{SnipTextFmtCtx, snip_long_text};
use rig::tool::{Tool, ToolContext, ToolExecutionError};
use std::time::Duration;

/// Runs commands inside the session's long-lived sandbox container.
pub(crate) struct ShellTool {
    runtime: ContainerRuntime,
    container: String,
    timeout: Duration,
}

impl ShellTool {
    pub(crate) fn new(runtime: ContainerRuntime, container: String, timeout: Duration) -> Self {
        Self {
            runtime,
            container,
            timeout,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ShellToolError {
    #[error("Command exited with error: '{0}'")]
    Failure(String),
    #[error(transparent)]
    Runtime(#[from] ContainerError),
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ShellToolArgs {
    cmd: String,
}

impl Tool for ShellTool {
    const NAME: &'static str = "shell";

    type Error = ShellToolError;

    type Args = ShellToolArgs;

    type Output = String;

    fn description(&self) -> String {
        "Runs shell commands (cat, grep, find, git, ...) inside an isolated sandbox container."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ShellToolArgs)).unwrap()
    }

    /// Without this override Rig redacts any error that is not already a
    /// [`ToolExecutionError`] to the generic string "the tool failed", so the
    /// command's stdout/stderr and exit status would never reach the model.
    /// The transcript is exactly what the model needs, so surface it verbatim.
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            ShellToolError::Failure(message) => ToolExecutionError::other(message),
            ShellToolError::Runtime(error) => ToolExecutionError::other(error.to_string()),
        }
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let output = self
            .runtime
            .exec(&self.container, &args.cmd, self.timeout)
            .await?;

        let snip_message_fmt = |SnipTextFmtCtx {
                                    bytes: _,
                                    max_bytes,
                                }| {
            format!("\n\n[... Output truncated. First {max_bytes} bytes shown ...]")
        };
        if output.success {
            let stdout = String::from_utf8_lossy(&output.stdout);
            Ok(snip_long_text(stdout, 10_000, snip_message_fmt).into())
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stdout = snip_long_text(stdout, 5000, snip_message_fmt);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = snip_long_text(stderr, 5000, snip_message_fmt);
            Err(ShellToolError::Failure(format!(
                "Exit status: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                output
                    .status
                    .map_or_else(|| "unknown".to_string(), |status| status.to_string())
            )))
        }
    }
}

/// Per-exec wall-clock timeout, overridable via `ANATOLY_SHELL_TIMEOUT`.
pub(crate) fn shell_timeout() -> Duration {
    let seconds = std::env::var("ANATOLY_SHELL_TIMEOUT")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(300);
    Duration::from_secs(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    #[test]
    fn tool_definition() {
        let runtime = ContainerRuntime::for_kind(crate::container::RuntimeKind::Podman);
        let tool = ShellTool::new(runtime, "anatoly-1".to_string(), Duration::from_secs(300));
        let def = rig::tool::tool_definition(&tool);
        assert_snapshot!(serde_json::to_string_pretty(&def).unwrap(), @r#"
        {
          "name": "shell",
          "description": "Runs shell commands (cat, grep, find, git, ...) inside an isolated sandbox container.",
          "parameters": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "ShellToolArgs",
            "type": "object",
            "properties": {
              "cmd": {
                "type": "string"
              }
            },
            "required": [
              "cmd"
            ]
          }
        }
        "#);
    }

    /// The command transcript must reach the model; Rig's default `map_error`
    /// would replace it with the generic string "the tool failed".
    #[test]
    fn failures_surface_command_output_to_the_model() {
        let runtime = ContainerRuntime::for_kind(crate::container::RuntimeKind::Podman);
        let tool = ShellTool::new(runtime, "anatoly-1".to_string(), Duration::from_secs(300));

        let transcript = "Exit status: 128\nstdout:\n\nstderr:\nfatal: not a git repository";
        let mapped = tool.map_error(ShellToolError::Failure(transcript.to_string()));
        assert_eq!(mapped.model_feedback(), Some(transcript));

        let mapped = tool.map_error(ShellToolError::Runtime(ContainerError::TimedOut(300)));
        assert!(
            mapped
                .model_feedback()
                .is_some_and(|feedback| feedback.contains("timed out")),
            "runtime errors must keep their detail, got {:?}",
            mapped.model_feedback()
        );
    }
}
