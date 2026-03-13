use rig::{completion::ToolDefinition, tool::Tool};

pub(crate) struct ReadFileTool {
    /// Maximum number of lines to return in a single call.
    pub(crate) max_lines: usize,
    /// Maximum number of bytes to return in a single call.
    pub(crate) max_bytes: usize,
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self {
            max_lines: 1_000,
            max_bytes: 100_000,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadFileToolError {
    #[error("Failed to read file '{0}', IO Error: '{1}'")]
    FailedToReadFile(String, std::io::Error),
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadFileToolArgs {
    /// A path for a file
    file_path: String,
    /// First line to read (1-indexed, inclusive). Defaults to 1.
    start_line: Option<usize>,
    /// Last line to read (1-indexed, inclusive). Defaults to end of file.
    end_line: Option<usize>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct ReadFileToolOutput {
    /// The file content for the requested range.
    pub(crate) content: String,
    /// Whether the output was truncated due to size limits.
    pub(crate) truncated: Option<bool>,
    /// The actual line range returned, formatted as "start,end" (e.g. "1,100").
    pub(crate) range_lines: String,
    /// A human-readable message describing truncation, if any.
    pub(crate) message: String,
}

impl Tool for ReadFileTool {
    const NAME: &'static str = "read-file";

    type Error = ReadFileToolError;

    type Args = ReadFileToolArgs;

    type Output = ReadFileToolOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        let parameters = schemars::schema_for!(ReadFileToolArgs);
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Reads a file and returns its content with optional line range selection"
                .to_string(),
            parameters: serde_json::to_value(parameters).unwrap(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        run_read_file(
            &args.file_path,
            args.start_line,
            args.end_line,
            self.max_lines,
            self.max_bytes,
        )
    }
}

fn run_read_file(
    file_path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_lines: usize,
    max_bytes: usize,
) -> Result<ReadFileToolOutput, ReadFileToolError> {
    use std::io::{BufRead, BufReader};
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

    // Reject symlinks to avoid symlink-based escapes.
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

    let file = std::fs::File::open(&canonical_candidate)
        .map_err(|err| ReadFileToolError::FailedToReadFile(String::from(file_path), err))?;
    let reader = BufReader::new(file);

    let range_start = start_line.unwrap_or(1);

    let mut content = String::new();
    let mut truncated = false;
    let mut truncation_message = String::new();
    let mut first_line: Option<usize> = None;
    let mut last_line: usize = range_start.saturating_sub(1);
    let mut lines_collected: usize = 0;

    for (idx, line_result) in reader.lines().enumerate() {
        let line_num = idx + 1; // 1-indexed

        // Skip lines before the requested start.
        if line_num < range_start {
            continue;
        }

        // Stop after the requested end line.
        if let Some(end) = end_line {
            if line_num > end {
                break;
            }
        }

        let line = line_result
            .map_err(|err| ReadFileToolError::FailedToReadFile(String::from(file_path), err))?;

        // Check line limit before appending.
        if lines_collected >= max_lines {
            truncated = true;
            truncation_message = format!(
                "Output truncated at line {line_num}: exceeded maximum line limit of {max_lines}."
            );
            break;
        }

        // Check byte limit before appending (account for the newline character).
        let line_bytes = line.len() + 1;
        if content.len() + line_bytes > max_bytes {
            truncated = true;
            truncation_message = format!(
                "Output truncated at line {line_num}: exceeded maximum byte limit of {max_bytes}."
            );
            break;
        }

        if first_line.is_none() {
            first_line = Some(line_num);
        }
        last_line = line_num;
        lines_collected += 1;

        content.push_str(&line);
        content.push('\n');
    }

    let actual_first = first_line.unwrap_or(range_start);
    let range_lines = format!("{},{}", actual_first, last_line);

    Ok(ReadFileToolOutput {
        content,
        truncated: truncated.then_some(true),
        range_lines,
        message: truncation_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::FileEnv;
    use insta::assert_snapshot;

    #[tokio::test]
    async fn tool_definition() {
        let def = ReadFileTool::default()
            .definition(String::from("prompt"))
            .await;
        assert_snapshot!(serde_json::to_string_pretty(&def).unwrap(), @r#"
        {
          "name": "read-file",
          "description": "Reads a file and returns its content with optional line range selection",
          "parameters": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "ReadFileToolArgs",
            "type": "object",
            "properties": {
              "file_path": {
                "description": "A path for a file",
                "type": "string"
              },
              "start_line": {
                "description": "First line to read (1-indexed, inclusive). Defaults to 1.",
                "type": [
                  "integer",
                  "null"
                ],
                "format": "uint",
                "minimum": 0
              },
              "end_line": {
                "description": "Last line to read (1-indexed, inclusive). Defaults to end of file.",
                "type": [
                  "integer",
                  "null"
                ],
                "format": "uint",
                "minimum": 0
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
        let tool = ReadFileTool::default();
        let err = tool
            .call(ReadFileToolArgs {
                file_path: path.to_string_lossy().into(),
                start_line: None,
                end_line: None,
            })
            .await
            .expect_err("tool failure");
        assert_snapshot!(err, @"Failed to read file '[TEMP_DIR]/hello.txt', IO Error: 'Access to paths outside the workspace is not allowed'");
    }

    #[tokio::test]
    async fn read_file_does_not_exist() {
        let tool = ReadFileTool::default();
        let err = tool
            .call(ReadFileToolArgs {
                file_path: String::from("abba.txt"),
                start_line: None,
                end_line: None,
            })
            .await
            .expect_err("tool failure");
        let ReadFileToolError::FailedToReadFile(file_path, io_error) = err;
        assert_eq!(file_path, "abba.txt");
        assert_eq!(io_error.kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn read_file_full() {
        let tool = ReadFileTool::default();
        let result = tool
            .call(ReadFileToolArgs {
                file_path: String::from("tests/fixtures/lorem_ipsum.txt"),
                start_line: None,
                end_line: None,
            })
            .await
            .expect("tool success");
        assert_eq!(result.truncated, None);
        assert!(result.message.is_empty());
        // The fixture has 10 lines.
        assert_eq!(result.range_lines, "1,10");
        assert!(result.content.contains("Lorem ipsum"));
    }

    #[tokio::test]
    async fn read_file_line_range() {
        let tool = ReadFileTool::default();
        let result = tool
            .call(ReadFileToolArgs {
                file_path: String::from("tests/fixtures/lorem_ipsum.txt"),
                start_line: Some(2),
                end_line: Some(4),
            })
            .await
            .expect("tool success");
        assert_eq!(result.truncated, None);
        assert!(result.message.is_empty());
        assert_eq!(result.range_lines, "2,4");
    }

    #[tokio::test]
    async fn read_file_truncated_by_line_limit() {
        let tool = ReadFileTool {
            max_lines: 3,
            max_bytes: 100_000,
        };
        let result = tool
            .call(ReadFileToolArgs {
                file_path: String::from("tests/fixtures/lorem_ipsum.txt"),
                start_line: None,
                end_line: None,
            })
            .await
            .expect("tool success");
        assert_eq!(result.truncated, Some(true));
        assert!(!result.message.is_empty());
        assert_eq!(result.range_lines, "1,3");
        assert_eq!(result.content.lines().count(), 3);
    }

    #[tokio::test]
    async fn read_file_truncated_by_byte_limit() {
        let tool = ReadFileTool {
            max_lines: 1_000,
            max_bytes: 50,
        };
        let result = tool
            .call(ReadFileToolArgs {
                file_path: String::from("tests/fixtures/lorem_ipsum.txt"),
                start_line: None,
                end_line: None,
            })
            .await
            .expect("tool success");
        assert_eq!(result.truncated, Some(true));
        assert!(!result.message.is_empty());
        assert!(result.content.len() <= 50);
    }
}
