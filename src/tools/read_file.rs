use rig::{completion::ToolDefinition, tool::Tool};

pub(crate) struct ReadFileTool;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadFileToolError {
    #[error("Failed to read file '{0}', IO Error: '{1}'")]
    FailedToReadFile(String, std::io::Error),
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadFileToolArgs {
    /// A path for a file
    file_path: String,
}

impl Tool for ReadFileTool {
    const NAME: &'static str = "read-file";

    type Error = ReadFileToolError;

    type Args = ReadFileToolArgs;

    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        let parameters = schemars::schema_for!(ReadFileToolArgs);
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Reads a file into memory".to_string(),
            parameters: serde_json::to_value(parameters).unwrap(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        run_read_file(&args.file_path)
    }
}

fn run_read_file(file_path: &str) -> Result<String, ReadFileToolError> {
    std::fs::read_to_string(file_path)
        .map_err(|err| ReadFileToolError::FailedToReadFile(String::from(file_path), err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::FileEnv;
    use insta::assert_snapshot;

    #[tokio::test]
    async fn tool_definition() {
        let def = ReadFileTool.definition(String::from("prompt")).await;
        assert_snapshot!(serde_json::to_string_pretty(&def).unwrap(), @r#"
        {
          "name": "read-file",
          "description": "Reads a file into memory",
          "parameters": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "ReadFileToolArgs",
            "type": "object",
            "properties": {
              "file_path": {
                "description": "A path for a file",
                "type": "string"
              }
            },
            "required": [
              "file_path"
            ]
          }
        }
        "#);
    }

    #[tokio::test]
    async fn read_file_success() {
        let file_env = FileEnv::new();
        let path = file_env.write_file("hello.txt", "hi".as_bytes());
        let tool = ReadFileTool;
        let result = tool
            .call(ReadFileToolArgs {
                file_path: path.to_string_lossy().into(),
            })
            .await
            .expect("tool success");
        assert_snapshot!(result, @"hi");
    }

    #[tokio::test]
    async fn read_file_failure() {
        let mut file_env = FileEnv::new();
        file_env.setup_insta_filter();
        let mut path = file_env.write_file("hello.txt", "hi".as_bytes());
        path.set_extension("md"); // changing extension, so the tool tries to read non existent file
        let tool = ReadFileTool;
        let result = tool
            .call(ReadFileToolArgs {
                file_path: path.to_string_lossy().into(),
            })
            .await
            .expect_err("tool failure");
        assert_snapshot!(result, @"Failed to read file '[TEMP_DIR]/hello.md', IO Error: 'No such file or directory (os error 2)'");
    }
}
