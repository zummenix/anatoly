use rig::{
    agent::{AgentBuilder, HookAction, PromptHook, ToolCallHookAction},
    client::{CompletionClient, ProviderClient},
    completion::{CompletionModel, CompletionResponse, Prompt},
    message::Message,
    providers::openrouter,
    tool::Tool,
    tools::ThinkTool,
};
use std::{
    borrow::Cow,
    io::{self, Write},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(test)]
mod test_utils;
mod tools;
mod utils;

use crate::{
    tools::read_file::{ReadFileToolArgs, ReadFileToolOutput},
    utils::{SnipTextFmtCtx, snip_long_text},
};
use crate::{
    tools::{read_file::ReadFileTool, safe_shell::SafeShellTool},
    utils::FilePermissions,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let file_permissions = FilePermissions::new()?;

    let client = openrouter::Client::from_env();
    let model_name = std::env::var("OPENROUTER_MODEL_NAME").expect("OPENROUTER_MODEL_NAME not set");
    let llm = client.completion_model(model_name);

    let code_assistant = AgentBuilder::new(llm)
        .name("Code Assistant")
        .max_tokens(1024)
        .default_max_turns(100)
        .preamble("You are code assistant helping user explore codebases and to devise the best solutions. Use the tools provided to answer user's question.")
        .hook(ToolHook {
            agent_name: "Code Assistant",
        })
        .tool(ThinkTool)
        .tool(ReadFileTool::new(file_permissions))
        .tool(SafeShellTool)
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
        match tool_name {
            ThinkTool::NAME => {
                println!("\n[{}] Thinking...", self.agent_name);
            }
            ReadFileTool::NAME => {
                if let Ok(args) = serde_json::from_str::<ReadFileToolArgs>(args) {
                    println!("\n[{}] Reading file: {args}", self.agent_name)
                }
            }
            _ => {
                println!(
                    "\n[{}] => CALLING TOOL: {}\n{}\n",
                    self.agent_name, tool_name, args
                );
            }
        }

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
        match tool_name {
            ThinkTool::NAME => {
                println!("{}\n", result);
            }
            ReadFileTool::NAME => {
                if let Ok(output) = serde_json::from_str::<ReadFileToolOutput>(result) {
                    println!("{} bytes", output.content.len());
                } else {
                    // Failed to deserialize, so this is an error, just print it.
                    println!("{}\n", result);
                }
            }
            _ => {
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
            }
        }

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
