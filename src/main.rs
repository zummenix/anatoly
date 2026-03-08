use rig::{
    agent::AgentBuilder,
    client::{CompletionClient, ProviderClient},
    completion::{Prompt, ToolDefinition},
    providers::openrouter,
    tool::Tool,
};
use serde_json::json;
use std::{
    collections::HashSet,
    io::{self, Write},
    process::Command,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let client = openrouter::Client::from_env();
    let model_name = std::env::var("OPENROUTER_MODEL_NAME").expect("OPENROUTER_MODEL_NAME not set");
    let llm = client.completion_model(model_name);

    let allowed_cmds = allowed_cmds();

    let sentinel_agent = AgentBuilder::new(llm.clone())
        .name("Sentinel")
        .description("An AI agent specialized in exploring and understanding complex codebases")
        .max_tokens(512)
        .default_max_turns(100)
        .preamble(
            r#"
You are Sentinel, an AI agent specialized in exploring and understanding
complex codebases. Your goal is to provide accurate architectural insights while
maintaining a lean and high-signal context window.

Use the tools provided to progress forward answering the user's question.

Tool Guidance:
- Before running a command, identify exactly what information you need.
- Start with `ls` to understand the directory structure before diving into file
  contents.
- Your context window is your "scarce RAM." Every line of command output
  consumes this budget. If an output is truncated, reduce scope search.
            "#,
        )
        .tool(SafeShellTool { allowed_cmds })
        .build();

    let code_assistant = AgentBuilder::new(llm)
        .name("Code Assistant")
        .max_tokens(1024)
        .default_max_turns(10)
        .preamble("You are code assistant. Use the tools provided to answer user's question.")
        .tool(sentinel_agent)
        .build();

    println!("Good day, sir! What can I help you with?\nCtrl-C to exit\n");

    loop {
        print!("> ");
        io::stdout().flush()?;
        let mut prompt = String::new();
        io::stdin().read_line(&mut prompt)?;

        let mut retrying_interval = 1;
        loop {
            match code_assistant.prompt(prompt.trim()).await {
                Ok(response) => {
                    println!("\n\n---\n{response}\n---\n\n");
                    break;
                }
                Err(err) => {
                    eprintln!("{err}\n\nRetrying after: {retrying_interval}s");
                    let mut interval =
                        tokio::time::interval(std::time::Duration::from_secs(retrying_interval));
                    interval.tick().await;
                    retrying_interval += 1;
                    continue;
                }
            }
        }
    }
}

struct SafeShellTool {
    allowed_cmds: HashSet<String>,
}

impl SafeShellTool {
    fn sorted_cmds(&self) -> Vec<String> {
        let mut list: Vec<_> = self.allowed_cmds.iter().cloned().collect();
        list.sort();
        list
    }
}

#[derive(Debug, thiserror::Error)]
enum SafeShellToolError {
    #[error("Command '{0}' is not allowed or is restricted")]
    CommandIsNotAllowed(String),
    #[error("Failed to execute '{0}', IO Error: '{1}'")]
    FailedToExecute(String, std::io::Error),
    #[error("Command exited with error: '{0}'")]
    Failure(String),
}

#[derive(Debug, serde::Deserialize)]
struct SafeShellToolArgs {
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
            description: format!(
                "Runs safe shell commands: {}",
                self.sorted_cmds().join(", ")
            ),
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
        println!("\n\nCalling {} with cmd: {}\n\n", Self::NAME, args.cmd);
        run_safe_shell_cmd(&args.cmd, &self.allowed_cmds)
    }
}

fn run_safe_shell_cmd(
    command: &str,
    allowed_cmds: &HashSet<String>,
) -> Result<String, SafeShellToolError> {
    if !allowed_cmds.iter().any(|cmd| command.starts_with(cmd)) {
        return Err(SafeShellToolError::CommandIsNotAllowed(String::from(
            command,
        )));
    }

    let output = Command::new("bash")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(|e| SafeShellToolError::FailedToExecute(String::from(command), e))?;

    if output.status.success() {
        let raw_output = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(truncate_output(raw_output, 10_000))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(SafeShellToolError::Failure(truncate_output(
            String::from(stderr),
            5000,
        )))
    }
}

fn truncate_output(output: String, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output;
    }

    let mut truncated = output;
    let snip_msg = format!("\n\n[... Output truncated. First {max_bytes} bytes shown ...]",);

    let mut byte_limit = max_bytes.saturating_sub(snip_msg.len());

    while !truncated.is_char_boundary(byte_limit) && byte_limit > 0 {
        byte_limit -= 1;
    }

    truncated.truncate(byte_limit);
    truncated.push_str(&snip_msg);
    truncated
}

fn allowed_cmds() -> HashSet<String> {
    [
        "ls", "grep", "cat", "head", "tail", "find", "wc", "jq", "file",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}
