use crate::utils::{SnipTextFmtCtx, snip_long_text};
use rig::{completion::ToolDefinition, tool::Tool};
use serde_json::json;
use std::process::Command;

pub(crate) struct SafeShellTool;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SafeShellToolError {
    #[error("Command '{0}' is not allowed or is restricted")]
    CommandIsNotAllowed(String),
    #[error("Failed to execute '{0}', IO Error: '{1}'")]
    FailedToExecute(String, std::io::Error),
    #[error("Command exited with error: '{0}'")]
    Failure(String),
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct SafeShellToolArgs {
    cmd: String,
}

impl Tool for SafeShellTool {
    const NAME: &'static str = "safe-shell";

    type Error = SafeShellToolError;

    type Args = SafeShellToolArgs;

    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Runs safe shell commands like cat, grep, find, etc.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "cmd": {
                        "type": "string",
                        "description": "Shell command to run"
                    },
                },
                "required": ["cmd"],
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        run_safe_shell_cmd(&args.cmd)
    }
}

fn run_safe_shell_cmd(command: &str) -> Result<String, SafeShellToolError> {
    if !safe_chains::is_safe_command(command) {
        return Err(SafeShellToolError::CommandIsNotAllowed(String::from(
            command,
        )));
    }

    let output = Command::new("bash")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(|e| SafeShellToolError::FailedToExecute(String::from(command), e))?;

    let snip_message_fmt =
        |SnipTextFmtCtx {
             bytes: _,
             max_bytes,
         }| { format!("\n\n[... Output truncated. First {max_bytes} bytes shown ...]",) };
    if output.status.success() {
        let raw_output = String::from_utf8_lossy(&output.stdout);
        Ok(snip_long_text(raw_output, 10_000, snip_message_fmt).into())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(SafeShellToolError::Failure(
            snip_long_text(stderr, 5000, snip_message_fmt).into(),
        ))
    }
}
