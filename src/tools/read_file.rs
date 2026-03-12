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
    use std::path::{Path, PathBuf};

    // Define the allowed root as the current working directory.
    let root = std::env::current_dir()
        .map_err(|err| ReadFileToolError::FailedToReadFile(String::from(file_path), err))?;

    // Build the requested path relative to the root if necessary.
    let requested = Path::new(file_path);
    let candidate: PathBuf = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    // Canonicalize both root and candidate to resolve symlinks and `..`.
    let canonical_root = root
        .canonicalize()
        .map_err(|err| ReadFileToolError::FailedToReadFile(String::from(file_path), err))?;
    let canonical_candidate = candidate
        .canonicalize()
        .map_err(|err| ReadFileToolError::FailedToReadFile(String::from(file_path), err))?;

    // Ensure the target path is inside the allowed root.
    if !canonical_candidate.starts_with(&canonical_root) {
        let io_err = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Access to paths outside the workspace is not allowed",
        );
        return Err(ReadFileToolError::FailedToReadFile(
            String::from(file_path),
            io_err,
        ));
    }

    // Optionally, reject symlinks to avoid symlink-based escapes.
    let metadata = std::fs::symlink_metadata(&canonical_candidate)
        .map_err(|err| ReadFileToolError::FailedToReadFile(String::from(file_path), err))?;
    if metadata.file_type().is_symlink() {
        let io_err = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Access to symbolic links is not allowed",
        );
        return Err(ReadFileToolError::FailedToReadFile(
            String::from(file_path),
            io_err,
        ));
    }

    std::fs::read_to_string(&canonical_candidate)
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
    async fn read_file_permission_error() {
        let mut file_env = FileEnv::new();
        file_env.setup_insta_filter();
        let path = file_env.write_file("hello.txt", "hi".as_bytes());
        let tool = ReadFileTool;
        let err = tool
            .call(ReadFileToolArgs {
                file_path: path.to_string_lossy().into(),
            })
            .await
            .expect_err("tool failure");
        assert_snapshot!(err, @"Failed to read file '[TEMP_DIR]/hello.txt', IO Error: 'Access to paths outside the workspace is not allowed'");
    }

    #[tokio::test]
    async fn read_file_does_not_exist() {
        let tool = ReadFileTool;
        let err = tool
            .call(ReadFileToolArgs {
                file_path: String::from("abba.txt"),
            })
            .await
            .expect_err("tool failure");
        let ReadFileToolError::FailedToReadFile(file_path, io_error) = err;
        assert_eq!(file_path, "abba.txt");
        assert_eq!(io_error.kind(), std::io::ErrorKind::NotFound);
    }
}
