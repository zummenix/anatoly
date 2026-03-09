use rig::{
    agent::{AgentBuilder, HookAction, PromptHook, ToolCallHookAction},
    client::{CompletionClient, ProviderClient},
    completion::{CompletionModel, CompletionResponse, Prompt, ToolDefinition},
    message::Message,
    providers::openrouter,
    tool::Tool,
    tools::ThinkTool,
};
use serde_json::json;
use std::{
    borrow::Cow,
    collections::HashSet,
    io::{self, Write},
    process::Command,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod utils;

use crate::utils::{SnipTextFmtCtx, snip_long_text};

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

    let code_assistant = AgentBuilder::new(llm)
        .name("Code Assistant")
        .max_tokens(1024)
        .default_max_turns(100)
        .preamble("You are code assistant helping user to devise the best solutions. Use the tools provided to answer user's question.")
        .hook(ToolHook {
            agent_name: "Code Assistant",
        })
        .tool(ThinkTool)
        .tool(SafeShellTool { allowed_cmds })
        .build();

    println!("Good day, sir! What can I help you with?\nCtrl-C to exit\n");

    let mut history: Vec<Message> = vec![];
    loop {
        print!("> ");
        io::stdout().flush()?;
        let mut prompt = String::new();
        io::stdin().read_line(&mut prompt)?;

        let mut retrying_interval = 1;
        loop {
            match code_assistant
                .prompt(prompt.trim())
                .with_history(&mut history)
                .await
            {
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

#[derive(Clone)]
struct ToolHook<'a> {
    agent_name: &'a str,
}

impl<'a, M: CompletionModel> PromptHook<M> for ToolHook<'a> {
    async fn on_tool_call(
        &self,
        tool_name: &str,
        _tool_call_id: Option<String>,
        _internal_call_id: &str,
        args: &str,
    ) -> ToolCallHookAction {
        println!(
            "\n[{}] => CALLING TOOL: {}\n{}\n",
            self.agent_name, tool_name, args
        );
        ToolCallHookAction::Continue
    }

    async fn on_tool_result(
        &self,
        tool_name: &str,
        _tool_call_id: Option<String>,
        _internal_call_id: &str,
        args: &str,
        result: &str,
    ) -> HookAction {
        println!(
            "\n[{}] <= TOOL RESULT {}\n{}\n{}",
            self.agent_name,
            tool_name,
            args,
            snip_long_text(
                Cow::from(result),
                300,
                |SnipTextFmtCtx {
                     bytes,
                     max_bytes: _,
                 }| { format!("... (total {bytes}b)") }
            )
        );

        HookAction::cont()
    }

    async fn on_completion_call(&self, _prompt: &Message, _history: &[Message]) -> HookAction {
        HookAction::cont()
    }

    async fn on_completion_response(
        &self,
        _prompt: &Message,
        _response: &CompletionResponse<M::Response>,
    ) -> HookAction {
        HookAction::cont()
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

    let snip_message_fmt =
        |SnipTextFmtCtx {
             bytes: _,
             max_bytes,
         }| { format!("\n\n[... Output truncated. First {max_bytes} bytes shown ...]",) };
    if output.status.success() {
        let raw_output = String::from_utf8_lossy(&output.stdout);
        Ok(utils::snip_long_text(raw_output, 10_000, snip_message_fmt).into())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(SafeShellToolError::Failure(
            snip_long_text(stderr, 5000, snip_message_fmt).into(),
        ))
    }
}

fn allowed_cmds() -> HashSet<String> {
    [
        "ls", "grep", "cat", "head", "tail", "find", "wc", "jq", "file",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}
