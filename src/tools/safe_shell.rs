use crate::utils::{SnipTextFmtCtx, snip_long_text};
use rig::{completion::ToolDefinition, tool::Tool};
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

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct SafeShellToolArgs {
    cmd: String,
}

impl Tool for SafeShellTool {
    const NAME: &'static str = "safe-shell";

    type Error = SafeShellToolError;

    type Args = SafeShellToolArgs;

    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        let parameters = schemars::schema_for!(SafeShellToolArgs);
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Runs safe shell commands like cat, grep, find, etc.".to_string(),
            parameters: serde_json::to_value(parameters).unwrap(),
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

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;
    use insta::assert_snapshot;
    use temp_dir::TempDir;

    #[tokio::test]
    async fn tool_definition() {
        let def = SafeShellTool.definition(String::from("prompt")).await;
        assert_snapshot!(serde_json::to_string_pretty(&def).unwrap(), @r#"
        {
          "name": "safe-shell",
          "description": "Runs safe shell commands like cat, grep, find, etc.",
          "parameters": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "SafeShellToolArgs",
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

    struct FileEnv {
        temp_dir: TempDir,
        #[allow(dyn_drop)]
        insta_settings_bind_drop_guard: Option<Box<dyn Drop>>,
    }

    impl FileEnv {
        fn new() -> Self {
            Self {
                temp_dir: TempDir::new().expect("create TempDir"),
                insta_settings_bind_drop_guard: None,
            }
        }

        fn setup_insta_filter(&mut self) {
            let mut settings = insta::Settings::clone_current();
            settings.add_filter(&self.temp_dir.path().to_string_lossy(), "[TEMP_DIR]");
            self.insta_settings_bind_drop_guard = Some(Box::new(settings.bind_to_scope()));
        }

        fn write_file(&self, path: &str, contents: &[u8]) -> PathBuf {
            let full_path = self.temp_dir.child(path);
            std::fs::write(&full_path, contents).expect("write file");
            full_path
        }
    }

    #[tokio::test]
    async fn cat_command_is_safe() {
        let file_env = FileEnv::new();
        let path = file_env.write_file("text.txt", "hello\n  world".as_bytes());

        let tool = SafeShellTool;
        let result = tool
            .call(SafeShellToolArgs {
                cmd: format!("cat {}", path.to_string_lossy()),
            })
            .await
            .expect("call success");
        assert_snapshot!(result, @r"
        hello
          world
        ");
    }

    #[tokio::test]
    async fn cat_command_is_denied() {
        let mut file_env = FileEnv::new();
        file_env.setup_insta_filter();
        let path = file_env.write_file("text.txt", "hello\n  world".as_bytes());
        let output_path = file_env.write_file("output.txt", "something".as_bytes());

        let tool = SafeShellTool;
        let result = tool
            .call(SafeShellToolArgs {
                cmd: format!(
                    "cat {} >> {}",
                    path.to_string_lossy(),
                    output_path.to_string_lossy()
                ),
            })
            .await
            .expect_err("call failure");
        assert_snapshot!(result.to_string(), @r"Command 'cat [TEMP_DIR]/text.txt >> [TEMP_DIR]/output.txt' is not allowed or is restricted");
        assert_eq!(
            fs::read_to_string(output_path).expect("read output file"),
            "something"
        );
    }
}
