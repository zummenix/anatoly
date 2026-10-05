use crate::container::{ContainerError, ContainerRuntime};
use crate::utils::{SnipTextFmtCtx, snip_long_text};
use rig::{completion::ToolDefinition, tool::Tool};
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

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        let parameters = schemars::schema_for!(ShellToolArgs);
        ToolDefinition {
            name: Self::NAME.to_string(),
            description:
                "Runs shell commands (cat, grep, find, git, ...) inside an isolated sandbox container."
                    .to_string(),
            parameters: serde_json::to_value(parameters).unwrap(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
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
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(ShellToolError::Failure(
                snip_long_text(stderr, 5000, snip_message_fmt).into(),
            ))
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

    #[tokio::test]
    async fn tool_definition() {
        let runtime = ContainerRuntime::for_kind(crate::container::RuntimeKind::Podman);
        let tool = ShellTool::new(runtime, "anatoly-1".to_string(), Duration::from_secs(300));
        let def = tool.definition(String::from("prompt")).await;
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
}
