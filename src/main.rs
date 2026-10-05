use rig::{
    agent::{
        AgentBuilder, AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction,
        DispatchEvent, HookContext, OutcomeAction, OutcomeEvent, PromptResponse,
    },
    completion::{Message, PromptError},
    core::{DynModel, operation::Completion},
    effect::EffectKind,
    providers::openrouter,
    tool::{Tool, builtin::ThinkTool},
};
use std::{
    borrow::Cow,
    future::Future,
    io::{self, Write},
    sync::{Arc, Mutex},
    time::Duration,
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

    let client = openrouter::from_env()
        .unwrap_or_else(|e| panic!("Failed to create OpenRouter client: {e}"));
    let model_name = std::env::var("OPENROUTER_MODEL_NAME").expect("OPENROUTER_MODEL_NAME not set");
    let llm = client.completion(model_name);

    let shell_tool = ShellTool::new(
        session.runtime.clone(),
        session.container_name.clone(),
        shell_timeout(),
    );

    // rig-core does not attach the in-flight messages to a completion error, so a
    // failed attempt loses every tool turn it made. The hook snapshots the full
    // request history before each completion call, letting us resume from there.
    let progress = Arc::new(Mutex::new(Vec::<Message>::new()));

    let code_assistant = agent_builder(llm, Arc::clone(&progress))
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

        let pending_prompt = Message::from(prompt.trim());
        let agent = &code_assistant;
        let (new_history, response) = run_with_retries(
            history,
            pending_prompt,
            |pending_prompt, history| async move {
                agent.prompt(pending_prompt).history(history).await
            },
            || progress.lock().unwrap().clone(),
        )
        .await;
        history = new_history;

        let output = response.output;
        let usage = response.usage;
        println!("\n\n---\n{output}\n[{usage:?}]\n---\n\n");
    }

    session.shutdown();
    Ok(())
}

/// Builds the configured agent without tools. The CLI and the end-to-end tests
/// share this so the tests drive the real rig loop under production wiring;
/// callers add their own tools (mock tools in tests) and call `.build()`.
fn agent_builder(
    model: impl Into<DynModel<Completion>>,
    progress: Arc<Mutex<Vec<Message>>>,
) -> AgentBuilder {
    AgentBuilder::new(model)
        .name("Code Assistant")
        .max_tokens(1024)
        .default_max_turns(100)
        .preamble(CODE_ASSISTANT_PREAMBLE)
        .add_hook(ToolHook {
            agent_name: "Code Assistant",
            progress,
        })
}

/// Runs `attempt` until it succeeds, resuming from the furthest progress the
/// agent reached after each failure.
///
/// `attempt` is called with the pending message and the committed history.
/// `snapshot` returns the hook's record of the last request's full history
/// (input history plus the pending message); it recovers the tool turns a failed
/// attempt produced but never returned to us.
async fn run_with_retries<A, AF, S>(
    mut history: Vec<Message>,
    mut pending_prompt: Message,
    mut attempt: A,
    snapshot: S,
) -> (Vec<Message>, PromptResponse)
where
    A: FnMut(Message, Vec<Message>) -> AF,
    AF: Future<Output = Result<PromptResponse, PromptError>>,
    S: Fn() -> Vec<Message>,
{
    let mut retrying_interval = 1;
    loop {
        match attempt(pending_prompt.clone(), history.clone()).await {
            Ok(response) => {
                if let Some(messages) = &response.messages {
                    history.extend_from_slice(messages);
                }
                return (history, response);
            }
            Err(err) => {
                eprintln!("{err}\n\nRetrying after: {retrying_interval}s");
                if resume_from_snapshot(&mut history, &mut pending_prompt, &snapshot()) {
                    eprintln!(
                        "Resuming from saved progress ({} messages in history).",
                        history.len()
                    );
                }
                tokio::time::sleep(Duration::from_secs(retrying_interval)).await;
                retrying_interval += 1;
            }
        }
    }
}

/// Adopts the furthest progress recorded in `snapshot` into `history` and
/// `pending_prompt`.
///
/// A snapshot is the committed history followed by the message being sent, so
/// splitting off that last message yields a valid history prefix plus the prompt
/// to resume with. Returns `true` when the snapshot extended the committed
/// history. Shorter, equal, or unrelated snapshots are ignored so a stale record
/// can never rewind or duplicate state.
fn resume_from_snapshot(
    history: &mut Vec<Message>,
    pending_prompt: &mut Message,
    snapshot: &[Message],
) -> bool {
    if snapshot.len() <= history.len() || !snapshot.starts_with(history) {
        return false;
    }
    let (last, rest) = snapshot
        .split_last()
        .expect("snapshot is longer than history, so it is non-empty");
    *history = rest.to_vec();
    *pending_prompt = last.clone();
    true
}

#[derive(Clone)]
struct ToolHook {
    agent_name: &'static str,
    progress: Arc<Mutex<Vec<Message>>>,
}

impl AgentHook for ToolHook {
    async fn on_dispatch(&self, _ctx: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if let EffectKind::ToolCall { name, args } = event.kind {
            match name.as_str() {
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
                        self.agent_name, name, args
                    );
                }
            }
        }

        DispatchAction::Proceed
    }

    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(tool_name) = event.tool_name() {
            let result = event
                .tool_result()
                .map(|result| result.output().render())
                .unwrap_or_default();
            match tool_name {
                ThinkTool::NAME => {
                    println!("{result}\n");
                }
                ReadFileTool::NAME => {
                    if let Ok(output) = serde_json::from_str::<ReadFileToolOutput>(&result) {
                        println!("{} bytes", output.content.len());
                    } else {
                        // Failed to deserialize, so this is an error, just print it.
                        println!("{result}\n");
                    }
                }
                _ => {
                    println!(
                        "\n[{}] <= TOOL RESULT {}\n{}",
                        self.agent_name,
                        tool_name,
                        snip_long_text(
                            Cow::from(result.as_str()),
                            300,
                            |SnipTextFmtCtx {
                                 bytes,
                                 max_bytes: _,
                             }| { format!("... (total {bytes}b)") }
                        )
                    );
                }
            }
        }

        OutcomeAction::Proceed
    }

    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        // Called before each completion request with the input history plus every
        // message accumulated so far except the pending one. Together they form
        // the exact request history, which is what we salvage on failure.
        {
            let mut progress = self.progress.lock().unwrap();
            progress.clear();
            progress.extend_from_slice(event.history);
            progress.push(event.prompt.clone());
        }
        CompletionCallAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::{
        completion::Usage,
        error::{ErrorKind, ProviderError},
        message::{
            AssistantContent, CallId, ToolCall, ToolFunction, ToolName, ToolResultContent,
            UserContent,
        },
        test_utils::{MockAddTool, MockCompletionModel, MockTurn},
    };
    use serde_json::json;

    fn user(text: &str) -> Message {
        Message::user(text)
    }

    fn assistant(text: &str) -> Message {
        Message::assistant(text)
    }

    fn tool_result(id: &str, text: &str) -> Message {
        Message::tool_result(
            CallId::from_wire(id),
            ToolName::new("shell").expect("non-empty tool name"),
            text,
        )
    }

    fn provider_error() -> PromptError {
        PromptError::CompletionError(ProviderError::Response("provider exploded".into()))
    }

    /// The assistant turn the rig `add` mock tool call deserializes to.
    fn assistant_tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Message {
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(ToolCall::from_wire(
                id,
                ToolFunction::new(ToolName::new(name).expect("non-empty tool name"), arguments),
            ))],
        }
    }

    /// The tool result the rig `add` mock tool commits. Its `i32` output is a
    /// serializable non-string, so it lands as structured JSON rather than text.
    fn add_tool_result(id: &str, value: i64) -> Message {
        Message::User {
            content: vec![UserContent::tool_result(
                CallId::from_wire(id),
                ToolName::new("add").expect("non-empty tool name"),
                vec![ToolResultContent::json(json!(value))],
            )],
        }
    }

    /// The history rig actually sends: the preamble as a leading system message,
    /// then the tool hook's snapshot. Source: `run::prepare::prepare_request`.
    fn with_preamble(messages: impl IntoIterator<Item = Message>) -> Vec<Message> {
        std::iter::once(Message::system(CODE_ASSISTANT_PREAMBLE))
            .chain(messages)
            .collect()
    }

    #[test]
    fn resume_ignores_snapshots_that_do_not_extend_history() {
        let mut history = vec![user("a"), assistant("b")];
        let mut pending = user("c");

        // Empty snapshot.
        assert!(!resume_from_snapshot(&mut history, &mut pending, &[]));
        // Snapshot equal to the committed history.
        let equal = history.clone();
        assert!(!resume_from_snapshot(&mut history, &mut pending, &equal));
        // Snapshot that is not a prefix of the committed history.
        let unrelated = vec![user("x"), assistant("y")];
        assert!(!resume_from_snapshot(&mut history, &mut pending, &unrelated));

        assert_eq!(history, vec![user("a"), assistant("b")]);
        assert_eq!(pending, user("c"));
    }

    #[test]
    fn resume_adopts_prefix_and_splits_off_pending_message() {
        let mut history = vec![user("task")];
        let mut pending = user("task");
        let snapshot = vec![
            user("task"),
            assistant("calling shell"),
            tool_result("call-1", "42"),
        ];

        assert!(resume_from_snapshot(&mut history, &mut pending, &snapshot));

        assert_eq!(history, vec![user("task"), assistant("calling shell")]);
        assert_eq!(pending, tool_result("call-1", "42"));
    }

    #[test]
    fn resume_before_any_progress_keeps_history_and_prompt() {
        let mut history = vec![user("earlier"), assistant("earlier answer")];
        let mut pending = user("task");
        let snapshot = vec![user("earlier"), assistant("earlier answer"), user("task")];

        assert!(resume_from_snapshot(&mut history, &mut pending, &snapshot));

        assert_eq!(history, vec![user("earlier"), assistant("earlier answer")]);
        assert_eq!(pending, user("task"));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_resumes_from_salvaged_progress_without_duplicating() {
        let snapshot = Arc::new(Mutex::new(Vec::<Message>::new()));
        let calls = Arc::new(Mutex::new(0usize));

        let attempt_snapshot = Arc::clone(&snapshot);
        let attempt_calls = Arc::clone(&calls);
        let attempt = move |_pending: Message, _history: Vec<Message>| {
            let attempt_snapshot = Arc::clone(&attempt_snapshot);
            let attempt_calls = Arc::clone(&attempt_calls);
            async move {
                let call = {
                    let mut calls = attempt_calls.lock().unwrap();
                    *calls += 1;
                    *calls
                };
                if call == 1 {
                    // The agent made a tool turn, then the provider failed.
                    *attempt_snapshot.lock().unwrap() = vec![
                        user("task"),
                        assistant("calling shell"),
                        tool_result("call-1", "42"),
                    ];
                    Err(provider_error())
                } else {
                    Ok(PromptResponse::new("done", Usage::default())
                        .with_messages(vec![tool_result("call-1", "42"), assistant("done")]))
                }
            }
        };
        let snapshot_reader = {
            let snapshot = Arc::clone(&snapshot);
            move || snapshot.lock().unwrap().clone()
        };

        let (history, response) =
            run_with_retries(vec![], user("task"), attempt, snapshot_reader).await;

        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(response.output, "done");
        assert_eq!(
            history,
            vec![
                user("task"),
                assistant("calling shell"),
                tool_result("call-1", "42"),
                assistant("done"),
            ],
            "salvaged tool turns must be kept exactly once"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_without_progress_reuses_original_prompt() {
        let snapshot = Arc::new(Mutex::new(Vec::<Message>::new()));
        let calls = Arc::new(Mutex::new(0usize));

        let attempt_snapshot = Arc::clone(&snapshot);
        let attempt_calls = Arc::clone(&calls);
        let attempt = move |_pending: Message, _history: Vec<Message>| {
            let attempt_snapshot = Arc::clone(&attempt_snapshot);
            let attempt_calls = Arc::clone(&attempt_calls);
            async move {
                let call = {
                    let mut calls = attempt_calls.lock().unwrap();
                    *calls += 1;
                    *calls
                };
                if call == 1 {
                    // Failure on the very first completion call: no tool progress.
                    *attempt_snapshot.lock().unwrap() = vec![user("task")];
                    Err(provider_error())
                } else {
                    Ok(PromptResponse::new("done", Usage::default())
                        .with_messages(vec![user("task"), assistant("done")]))
                }
            }
        };
        let snapshot_reader = {
            let snapshot = Arc::clone(&snapshot);
            move || snapshot.lock().unwrap().clone()
        };

        let (history, _response) =
            run_with_retries(vec![], user("task"), attempt, snapshot_reader).await;

        assert_eq!(
            history,
            vec![user("task"), assistant("done")],
            "a failure before any tool turn must not lose or duplicate the prompt"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn successful_attempt_commits_turn_messages_once() {
        let attempt = |_pending: Message, _history: Vec<Message>| async move {
            Ok::<_, PromptError>(
                PromptResponse::new("done", Usage::default())
                    .with_messages(vec![user("task"), assistant("done")]),
            )
        };
        let no_snapshot = || Vec::<Message>::new();

        let (history, _response) =
            run_with_retries(vec![], user("task"), attempt, no_snapshot).await;

        assert_eq!(history, vec![user("task"), assistant("done")]);
    }

    // -- End-to-end tests over rig's real agent loop -------------------------
    //
    // The tests above hand-build `PromptResponse`s and `attempt` closures, so
    // they exercise *our* code but not rig's agent loop. These drive that loop
    // through rig's scripted `MockCompletionModel` and assert the internals our
    // retry/resume logic relies on, so a future rig release that breaks them
    // fails loudly instead of silently degrading salvage.

    #[tokio::test(start_paused = true)]
    async fn e2e_happy_path_transcript_and_request_snapshot_match_the_model_call() {
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call_1", "add", json!({ "x": 20, "y": 22 })),
            MockTurn::text("done"),
        ]);
        let inspector = model.clone();
        let progress = Arc::new(Mutex::new(Vec::<Message>::new()));
        let agent = agent_builder(model, Arc::clone(&progress))
            .tool(MockAddTool)
            .build();

        let input = vec![user("earlier"), assistant("earlier answer")];
        let (history, response) = run_with_retries(
            input.clone(),
            user("add 20 and 22"),
            |pending, history| {
                let agent = &agent;
                async move { agent.prompt(pending).history(history).await }
            },
            || progress.lock().unwrap().clone(),
        )
        .await;

        let transcript = vec![
            user("add 20 and 22"),
            assistant_tool_call("call_1", "add", json!({ "x": 20, "y": 22 })),
            add_tool_result("call_1", 42),
            assistant("done"),
        ];

        // Invariant #2: `response.messages` is the run transcript — the prompt,
        // accepted assistant turns, and committed tool results — excluding the
        // input history. Source: rig-agent/src/run/response.rs,
        // `PromptResponse::messages`.
        assert_eq!(response.messages.as_deref(), Some(transcript.as_slice()));
        assert_eq!(
            history,
            input
                .iter()
                .cloned()
                .chain(transcript.iter().cloned())
                .collect::<Vec<_>>(),
            "input history plus the run transcript, committed exactly once"
        );

        let requests = inspector.requests();
        assert_eq!(requests.len(), 2, "one model call per run turn");

        let mut after_prompt = input.clone();
        after_prompt.push(user("add 20 and 22"));

        let mut after_tool = after_prompt.clone();
        after_tool.push(assistant_tool_call(
            "call_1",
            "add",
            json!({ "x": 20, "y": 22 }),
        ));
        after_tool.push(add_tool_result("call_1", 42));

        // Invariant #1: each request's `chat_history` is the hook snapshot
        // (`event.history + [event.prompt]`) with the preamble system message in
        // front, so the hook sees the complete request minus the preamble.
        // Sources: rig-agent/src/run/mod.rs (the `CallModel` arm's
        // `split_last` + `build_history_for_request`) and
        // rig-agent/src/run/prepare.rs (the preamble becomes the leading
        // `Message::system`).
        assert_eq!(with_preamble(after_prompt), requests[0].chat_history);
        assert_eq!(with_preamble(after_tool.clone()), requests[1].chat_history);

        // The hook's `progress` holds the last request snapshot it took.
        assert_eq!(progress.lock().unwrap().clone(), after_tool);
    }

    #[tokio::test(start_paused = true)]
    async fn e2e_fail_then_resume_replays_the_tool_turn_once_from_the_snapshot() {
        // Turn 1 answers with a tool call; the run's second completion call
        // hits the scripted provider error; the third call answers the resumed
        // run. `run_with_retries` must salvage turn 1 from the hook snapshot and
        // replay it exactly once.
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call_1", "add", json!({ "x": 20, "y": 22 })),
            MockTurn::error("provider exploded"),
            MockTurn::text("42"),
        ]);
        let inspector = model.clone();
        let progress = Arc::new(Mutex::new(Vec::<Message>::new()));
        let agent = agent_builder(model, Arc::clone(&progress))
            .tool(MockAddTool)
            .build();

        let input = vec![user("earlier"), assistant("earlier answer")];
        let attempts = Arc::new(Mutex::new(0usize));
        let attempt_calls = Arc::clone(&attempts);
        let agent_ref = &agent;
        let attempt = move |pending: Message, history: Vec<Message>| {
            let attempt_calls = Arc::clone(&attempt_calls);
            async move {
                *attempt_calls.lock().unwrap() += 1;
                agent_ref.prompt(pending).history(history).await
            }
        };

        let (history, response) =
            run_with_retries(input.clone(), user("add 20 and 22"), attempt, || {
                progress.lock().unwrap().clone()
            })
            .await;

        assert_eq!(
            *attempts.lock().unwrap(),
            2,
            "the provider failure must be retried once"
        );
        assert_eq!(response.output, "42");

        let transcript = [
            user("add 20 and 22"),
            assistant_tool_call("call_1", "add", json!({ "x": 20, "y": 22 })),
            add_tool_result("call_1", 42),
            assistant("42"),
        ];
        // Invariant #4: the completed tool turn survives the failure exactly
        // once — no duplication, no loss.
        assert_eq!(
            history,
            input
                .iter()
                .cloned()
                .chain(transcript.iter().cloned())
                .collect::<Vec<_>>()
        );

        // Invariant #3/#4: a provider failure carries no history, so recovery
        // comes solely from the hook. The resumed request re-sends the salvaged
        // prefix — the committed tool result as the pending prompt — instead of
        // restarting from the bare prompt.
        let requests = inspector.requests();
        assert_eq!(requests.len(), 3);

        let mut salvaged = input.clone();
        salvaged.push(user("add 20 and 22"));
        salvaged.push(assistant_tool_call(
            "call_1",
            "add",
            json!({ "x": 20, "y": 22 }),
        ));
        salvaged.push(add_tool_result("call_1", 42));

        assert_eq!(with_preamble(salvaged.clone()), requests[1].chat_history);
        assert_eq!(
            with_preamble(salvaged),
            requests[2].chat_history,
            "resume must replay the salvaged prefix (tool result included), not restart"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn e2e_provider_failure_carries_no_history_and_the_hook_is_the_only_record() {
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call_1", "add", json!({ "x": 1, "y": 2 })),
            MockTurn::error("provider exploded"),
        ]);
        let progress = Arc::new(Mutex::new(Vec::<Message>::new()));
        let agent = agent_builder(model, Arc::clone(&progress))
            .tool(MockAddTool)
            .build();

        let err = agent
            .prompt(user("add 1 and 2"))
            .history(Vec::<Message>::new())
            .await
            .expect_err("the second completion call fails");

        // Invariant #3: a provider failure carries no run history, so it cannot
        // be salvaged and only the hook snapshot can recover the tool turn.
        // Under the runtime's effect bus the failure surfaces as
        // `PromptError::Report` with `ErrorKind::Provider` (the `ProviderError`
        // wrapped in an `ErrorReport`), rather than the bare
        // `PromptError::CompletionError` the enum alone suggests. Sources:
        // rig-agent/src/agent/engine.rs (`streaming_error_into_prompt` and the
        // unary `run_model_turn`'s `CompletionDispatchError::Failed`) and
        // rig-agent/src/run/response.rs (`enum PromptError`).
        match &err {
            PromptError::Report(report) => assert_eq!(report.kind, ErrorKind::Provider),
            other => panic!("expected a bus-wrapped provider failure, got {other:?}"),
        }
        assert!(
            !matches!(
                err,
                PromptError::MaxTurnsError { .. }
                    | PromptError::PromptCancelled { .. }
                    | PromptError::UnknownToolCall { .. }
            ),
            "the failure must be one of the variants that carries no history"
        );

        // The hook still recorded the completed tool turn: the only salvage
        // source available after the failure.
        assert_eq!(
            progress.lock().unwrap().clone(),
            vec![
                user("add 1 and 2"),
                assistant_tool_call("call_1", "add", json!({ "x": 1, "y": 2 })),
                add_tool_result("call_1", 3),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn e2e_max_turns_error_carries_the_canonical_history() {
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call_1", "add", json!({ "x": 1, "y": 2 })),
            MockTurn::text("42"),
        ]);
        let progress = Arc::new(Mutex::new(Vec::<Message>::new()));
        let agent = agent_builder(model, Arc::clone(&progress))
            .default_max_turns(1)
            .tool(MockAddTool)
            .build();

        let err = agent
            .prompt(user("add 1 and 2"))
            .history(Vec::<Message>::new())
            .await
            .expect_err("the run exceeds its single-turn budget");

        // Invariant #5: unlike a provider failure, `MaxTurnsError` carries
        // `chat_history`, a secondary salvage source. Source:
        // rig-agent/src/run/response.rs, `enum PromptError`.
        match err {
            PromptError::MaxTurnsError {
                max_turns,
                chat_history,
                prompt,
            } => {
                assert_eq!(max_turns, 1);
                assert_eq!(
                    chat_history,
                    vec![
                        user("add 1 and 2"),
                        assistant_tool_call("call_1", "add", json!({ "x": 1, "y": 2 })),
                        add_tool_result("call_1", 3),
                    ]
                );
                assert_eq!(prompt, add_tool_result("call_1", 3));
            }
            other => panic!("expected MaxTurnsError, got {other:?}"),
        }
    }
}
