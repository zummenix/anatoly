use rig::{
    agent::{AgentBuilder, HookAction, PromptHook, PromptResponse, ToolCallHookAction},
    client::{CompletionClient, ProviderClient},
    completion::{CompletionModel, CompletionResponse, Message, Prompt, PromptError},
    providers::openrouter,
    tool::Tool,
    tools::ThinkTool,
};
use std::{
    borrow::Cow,
    io::{self, Write},
    sync::Arc,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod container;
mod session;
#[cfg(test)]
mod test_utils;
mod tools;
mod utils;

use crate::{
    tools::read_file::{ReadFileTool, ReadFileToolArgs, ReadFileToolOutput},
    tools::shell::{ShellTool, shell_timeout},
    utils::{FilePermissions, SnipTextFmtCtx, snip_long_text},
};
use session::Session;

const CODE_ASSISTANT_PREAMBLE: &str = include_str!("prompts/code_assistant_preamble.md");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cwd = std::env::current_dir()?;
    let session = Arc::new(Session::start(&cwd)?);
    let file_permissions = FilePermissions::with_root(session.sandbox_dir.clone())?;

    // SIGINT does not run destructors, so shutdown must be explicit.
    {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    session.shutdown();
                    std::process::exit(130);
                }
                Err(err) => eprintln!("Failed to listen for Ctrl-C: {err}"),
            }
        });
    }

    let client = openrouter::Client::from_env()
        .unwrap_or_else(|e| panic!("Failed to create OpenRouter client: {e}"));
    let model_name = std::env::var("OPENROUTER_MODEL_NAME").expect("OPENROUTER_MODEL_NAME not set");
    let llm = client.completion_model(model_name);

    let shell_tool = ShellTool::new(
        session.runtime.clone(),
        session.container_name.clone(),
        shell_timeout(),
    );

    let code_assistant = AgentBuilder::new(llm)
        .name("Code Assistant")
        .max_tokens(1024)
        .default_max_turns(100)
        .preamble(CODE_ASSISTANT_PREAMBLE)
        .hook(ToolHook {
            agent_name: "Code Assistant",
        })
        .tool(ThinkTool)
        .tool(ReadFileTool::new(file_permissions))
        .tool(shell_tool)
        .build();

    println!("Good day, sir! What can I help you with?\nCtrl-C to exit\n");

    let mut history: Vec<Message> = vec![];
    loop {
        print!("> ");
        io::stdout().flush()?;
        let mut prompt = String::new();
        let bytes_read = io::stdin().read_line(&mut prompt)?;
        if bytes_read == 0 {
            // EOF (Ctrl-D).
            break;
        }

        let mut retrying_interval = 1;
        loop {
            let prompt_response: Result<PromptResponse, PromptError> = code_assistant
                .prompt(prompt.trim())
                .with_history(history.clone())
                .extended_details()
                .await;
            match prompt_response {
                Ok(response) => {
                    let output = response.output;
                    let usage = response.usage;
                    println!("\n\n---\n{output}\n[{usage:?}]\n---\n\n");
                    if let Some(messages) = response.messages {
                        history.extend_from_slice(&messages);
                    }
                    break;
                }
                Err(err) => {
                    eprintln!("{err}\n\nRetrying after: {retrying_interval}s");
                    tokio::time::sleep(std::time::Duration::from_secs(retrying_interval)).await;
                    retrying_interval += 1;
                    continue;
                }
            }
        }
    }

    session.shutdown();
    Ok(())
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
        _args: &str,
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
                    "\n[{}] <= TOOL RESULT {}\n{}",
                    self.agent_name,
                    tool_name,
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
