mod chat;
mod geocoding;
mod llm;
mod telegram;
mod tools;

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use futures_lite::{Stream, StreamExt};
use is_terminal::IsTerminal;
use macro_rules_attribute::apply;
use serde_json::Map;
use smol_macros::main;

use pluribus_frequency::network;
use pluribus_frequency::protocol::{Message, NodeId, ToolCall, ToolDef, ToolName};
use pluribus_frequency::state::{Entry, State};

use pluribus_frequency::Handle;
use pluribus_llm::{collect_response, GenOptions, Provider};

/// Maximum number of user messages to keep in the context window.
const CONTEXT_USER_MESSAGES: usize = 5;
/// Maximum number of iterations to allow in a single LLM turn before giving up
const MAX_ITERATIONS: usize = 50;
/// Port nodes listen on for communication.
const PORT: u16 = 8613;

#[apply(main!)]
async fn main(executor: Arc<async_executor::Executor<'static>>) {
    let network = network::tailscale(PORT).await.expect("tailscale identity");
    let data_dir = dirs::data_dir()
        .expect("could not determine data directory")
        .join("pluribus");

    let state = State::open(&data_dir).expect("failed to open log");

    let initial_defs = tools::resolve(&state).defs();

    let frequency = pluribus_frequency::join(executor.clone(), network, &state, initial_defs)
        .await
        .expect("start networking");

    tracing::info!(name = %frequency.self_manifest().name, "started");

    let llm = llm::resolve(&state).await;

    let options = GenOptions {
        thinking: true,
        ..GenOptions::default()
    };
    let config = state.configuration();

    if std::io::stdout().is_terminal() {
        let chat = chat::terminal();
        run_interactive(&executor, llm, state, &frequency, &options, &chat).await;
    } else if let (Some(token), Some(chat_id)) = (
        tools::telegram::bot_token(&config),
        tools::telegram::chat_id(&config),
    ) {
        tracing::info!(chat_id, "using Telegram chat");
        let chat = chat::telegram(&token, chat_id);
        run_interactive(&executor, llm, state, &frequency, &options, &chat).await;
    } else {
        let chat = chat::noop();
        run_interactive(&executor, llm, state, &frequency, &options, &chat).await;
    }
}

async fn run_interactive<P: Provider, C: chat::Chat>(
    executor: &Arc<async_executor::Executor<'static>>,
    llm: P,
    state: State,
    frequency: &Handle,
    options: &GenOptions,
    chat: &C,
) {
    let node_id = frequency.self_manifest().id.clone();

    futures_lite::future::zip(
        run(executor, &llm, &state, frequency, options, chat),
        futures_lite::future::zip(
            async {
                let mut stream = if chat.show_full_history() {
                    state.history().listen().boxed()
                } else {
                    state.history().listen_live().boxed()
                };
                while let Some(entry) = stream.next().await {
                    chat.display(&node_id, &entry).await;
                }
            },
            futures_lite::future::zip(
                async {
                    let mut stream = chat.messages();
                    while let Some(content) = stream.next().await {
                        if chat.requires_leader() && !is_lowest_node(&node_id, frequency) {
                            continue;
                        }
                        let message = Message::User { content };
                        let entry = Entry::new(message, node_id.clone());
                        if state.history().push(&entry).is_err() {
                            break;
                        }
                    }
                    state.leave();
                },
                run_scheduler(&state, frequency),
            ),
        ),
    )
    .await;
}

/// Check if this node has the lowest `NodeId` among all connected peers.
///
/// Returns `true` when alone or when every peer has a higher ID.
fn is_lowest_node(my_id: &NodeId, net: &Handle) -> bool {
    net.others().iter().all(|m| *my_id < m.id)
}

fn is_lowest_for_tool(my_id: &NodeId, tool_name: &ToolName, net: &Handle) -> bool {
    net.others()
        .iter()
        .filter(|m| m.tools.iter().any(|t| t.name() == tool_name))
        .all(|m| *my_id < m.id)
}

/// Collect all tool definitions (local + others).
fn all_tool_defs(local_defs: &[ToolDef], net: &Handle) -> Vec<ToolDef> {
    let mut defs = local_defs.to_vec();
    for m in net.others() {
        defs.extend(m.tools);
    }
    defs
}

/// Build a context window from the conversation history.
///
/// Keeps at most the last [`CONTEXT_USER_MESSAGES`] user messages plus
/// surrounding context. Messages in the "current turn" (from the last `User`
/// message onward) are kept verbatim so the LLM can see pending tool calls
/// and results. Older messages have tool calls stripped from `Assistant`
/// messages and `Tool` result messages are dropped entirely.
fn build_context(entries: &[Entry]) -> Vec<Message> {
    // Find the window start: the Nth user message from the end.
    let window_start = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.message, Message::User { .. }))
        .rev()
        .nth(CONTEXT_USER_MESSAGES - 1)
        .map_or(0, |(i, _)| i);

    // Find the last user message index — everything from here onward is the
    // "current turn" and must be kept intact (tool calls + results).
    let last_user = entries
        .iter()
        .rposition(|e| matches!(e.message, Message::User { .. }));

    let windowed = &entries[window_start..];

    windowed
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let absolute = window_start + i;
            let is_current_turn = last_user.is_some_and(|lu| absolute >= lu);

            if is_current_turn {
                return Some(e.message.clone());
            }

            // Historical: strip tool overhead and reasoning.
            match &e.message {
                Message::System { .. } | Message::User { .. } => Some(e.message.clone()),
                Message::Assistant { content, .. } => {
                    if content.is_empty() {
                        // Tool-only assistant message — drop it.
                        None
                    } else {
                        // Keep text, strip tool calls and reasoning.
                        Some(Message::Assistant {
                            content: content.clone(),
                            tool_calls: Vec::new(),
                            reasoning_content: None,
                        })
                    }
                }
                Message::Tool { .. } => None,
            }
        })
        .collect()
}

/// Build the system message for LLM evaluation.
///
/// Contains: identity, network state, memory, schedule instructions,
/// and an optional chat-specific hint.
fn build_system_message(net: &Handle, state: &State, chat: &dyn chat::Chat) -> Message {
    let manifest = net.self_manifest();
    let self_name = &manifest.name;

    let peers_section = {
        let others = net.others();
        if others.is_empty() {
            String::from("No other nodes online.")
        } else {
            let names: Vec<&str> = others.iter().map(|m| m.name.as_str()).collect();
            format!("Connected nodes: {}.", names.join(", "))
        }
    };

    let memory_section = {
        let config = state.configuration();
        let memory = tools::memory::entries(&config);
        if memory.is_empty() {
            String::from("(empty)")
        } else {
            let mut lines: Vec<String> =
                memory.iter().map(|(k, v)| format!("- {k}: {v}")).collect();
            lines.sort();
            lines.join("\n")
        }
    };

    let chat_hint = chat.context_hint();
    let chat_hint_section = if chat_hint.is_empty() {
        String::new()
    } else {
        format!("\n\n{chat_hint}")
    };

    Message::system(format!(
        "\
You are Pluribus, a personal AI assistant running across a network of devices.
You are on node \"{self_name}\". {peers_section}

Act through tools. Prefer action over advice.
Call multiple tools at once when they are independent.
If a tool fails, say what happened and offer alternatives.

## Memory

Durable facts about the user injected into every conversation.
Use remember(key, value) when you learn something lasting — names, preferences, projects.
Use forget(key) when something is no longer true.
Do not store transient information.

{memory_section}

## Schedules

Schedules execute a prompt as a new top-level request at a specified time.
The prompt runs without conversation history — write it as a self-contained instruction.

- One-shot: provide `at` (RFC 3339 datetime). Auto-deleted after firing.
- Recurring: provide `cron` (5-field expression). Runs until cancelled.

Use getCurrentTime before computing schedule times.{chat_hint_section}"
    ))
}

/// Background scheduler: wakes every 30 seconds and fires due schedules.
///
/// Only the leader (lowest `NodeId`) fires schedules to avoid duplicates
/// in multi-node deployments.
///
/// When a schedule fires, a synthetic `User` message is pushed to history,
/// which triggers the main agent loop to run an LLM turn.
async fn run_scheduler(state: &State, net: &Handle) {
    let node_id = net.self_manifest().id.clone();

    loop {
        async_io::Timer::after(Duration::from_secs(30)).await;

        if !is_lowest_node(&node_id, net) {
            continue;
        }

        let config = state.configuration();
        let schedules = tools::schedule::entries(&config);
        let now = chrono::Utc::now();

        for (id, schedule) in &schedules {
            let should_fire = match &schedule.trigger {
                tools::schedule::Trigger::Once { at } => *at <= now,
                tools::schedule::Trigger::Cron { cron } => {
                    let Ok(parsed) = croner::Cron::from_str(cron) else {
                        tracing::warn!(id, cron, "invalid cron expression in schedule");
                        continue;
                    };
                    let check_from = now - chrono::Duration::seconds(30);
                    parsed
                        .find_next_occurrence(&check_from, true)
                        .ok()
                        .is_some_and(|next| next <= now)
                }
            };

            if should_fire {
                tracing::info!(id, prompt = %schedule.prompt, "schedule fired");
                let entry = Entry::new(Message::user(&schedule.prompt), node_id.clone());
                if state.history().push(&entry).is_err() {
                    return;
                }

                // Auto-delete one-shot schedules.
                if matches!(schedule.trigger, tools::schedule::Trigger::Once { .. }) {
                    let key = format!("schedule.{id}");
                    let _ = config.delete(&key);
                }
            }
        }
    }
}

/// Run the agent as an event-driven loop over a [`State`].
///
/// Reacts to entries in the conversation log:
/// - `User` from this node — runs LLM, posts `Assistant`
/// - `Assistant` with tool calls — executes tools we own if we're the lowest ID
/// - `Tool` result — if all results are in and we're the driver, continues LLM
///
/// Tools are rebuilt from current state before each LLM turn and before each
/// tool execution, so the available tools adapt to configuration changes.
///
/// Returns when the log stream ends (i.e. [`State::leave`] is called).
async fn run<P: Provider, C: chat::Chat>(
    executor: &Arc<async_executor::Executor<'static>>,
    llm: &P,
    state: &State,
    net: &Handle,
    options: &GenOptions,
    chat: &C,
) {
    let node_id = net.self_manifest().id.clone();
    let mut stream = state.history().listen_live();

    while let Some(entry) = stream.next().await {
        match &entry.message {
            // User message we originated → run tool loop
            Message::User { .. } if entry.origin == node_id => {
                tool_loop(llm, &node_id, state, net, options, chat, executor).await;
            }

            // Remote tool call we can handle
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                if entry.origin == node_id {
                    continue; // We're the LLM node, tool_loop handles this
                }
                execute_remote_calls(tool_calls, &node_id, state, net).await;
            }

            _ => {}
        }
    }
}

/// Non-LLM nodes: execute tool calls they own.
async fn execute_remote_calls(
    tool_calls: &[ToolCall],
    node_id: &NodeId,
    state: &State,
    net: &Handle,
) {
    let tools = tools::resolve(state);
    let local_defs = tools.defs();
    net.update_tools(&local_defs);

    for call in tool_calls {
        let name = ToolName::new(&call.name);
        if !local_defs.iter().any(|d| d.name() == &name) {
            continue;
        }
        if !is_lowest_for_tool(node_id, &name, net) {
            continue;
        }

        let input: serde_json::Value = serde_json::from_str(&call.arguments)
            .unwrap_or_else(|_| serde_json::Value::Object(Map::default()));

        let result = tools.execute(&name, input).await;
        let (content, is_error) = match &result {
            Ok(v) => (serde_json::to_string(v).unwrap_or_default(), false),
            Err(e) => (e.clone(), true),
        };

        let entry = Entry::new(
            Message::tool_result(&call.id, content, is_error),
            node_id.clone(),
        );
        let _ = state.history().push(&entry);
    }
}

/// Run the tool loop for a single user message until the LLM is done.
async fn tool_loop<P: Provider, C: chat::Chat>(
    llm: &P,
    node_id: &NodeId,
    state: &State,
    net: &Handle,
    options: &GenOptions,
    chat: &C,
    executor: &Arc<async_executor::Executor<'static>>,
) {
    for _ in 0..MAX_ITERATIONS {
        let local_tools = Arc::new(tools::resolve(state));
        let local_defs = local_tools.defs();
        net.update_tools(&local_defs);
        let all_defs = all_tool_defs(&local_defs, net);
        let entries = state.history().messages();
        let cursor = entries.len();
        let mut messages = build_context(&entries);
        messages.insert(0, build_system_message(net, state, chat));

        let response = {
            let stream = llm.complete_stream(&messages, &all_defs, options);
            let stream = chat.display_stream(stream);
            match collect_response(stream).await {
                Ok(msg) => msg,
                Err(e) => {
                    tracing::warn!(%e, "llm error");
                    let entry = Entry::new(
                        Message::assistant(format!("LLM error: {e}")),
                        node_id.clone(),
                    );
                    let _ = state.history().push(&entry);
                    return;
                }
            }
        };

        let tool_calls = response.tool_calls().to_vec();
        let entry = Entry::new(response, node_id.clone());
        let _ = state.history().push(&entry);

        // No tool calls — LLM is done.
        if tool_calls.is_empty() {
            return;
        }

        // Partition: local vs remote
        let mut local = Vec::new();
        let mut remote = Vec::new();

        for call in &tool_calls {
            let name = ToolName::new(&call.name);
            if local_defs.iter().any(|d| d.name() == &name) {
                local.push(call);
            } else {
                remote.push(call);
            }
        }

        // Execute local tools in parallel
        let local_tasks: Vec<_> = local
            .into_iter()
            .map(|call| {
                let tools = Arc::clone(&local_tools);
                let name = ToolName::new(&call.name);
                let input: serde_json::Value = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::Object(Map::default()));
                let call_id = call.id.clone();
                executor.spawn(async move {
                    let result = tools.execute(&name, input).await;
                    let (content, is_error) = match &result {
                        Ok(v) => (serde_json::to_string(v).unwrap_or_default(), false),
                        Err(e) => (e.clone(), true),
                    };
                    (call_id, content, is_error)
                })
            })
            .collect();

        // Dispatch remote tools — push to history and wait for results
        let mut pending_remote: HashSet<String> = HashSet::new();
        for call in &remote {
            pending_remote.insert(call.id.clone());
            // The tool call is already in the history (in the Assistant message).
            // Remote nodes see it and execute it.
        }

        // Collect local results
        for task in local_tasks {
            let (call_id, content, is_error) = task.await;
            pending_remote.remove(&call_id); // shouldn't be there, but safe
            let entry = Entry::new(
                Message::tool_result(&call_id, content, is_error),
                node_id.clone(),
            );
            let _ = state.history().push(&entry);
        }

        // Wait for remote results
        if !pending_remote.is_empty() {
            let stream = state.history().listen_since(cursor);
            wait_for_results(stream, &pending_remote, Duration::from_secs(120)).await;
        }

        // All results in — loop back to LLM
    }

    // Hit max iterations
    let entry = Entry::new(
        Message::assistant("Stopped: too many tool iterations."),
        node_id.clone(),
    );
    let _ = state.history().push(&entry);
}

async fn wait_for_results(
    stream: impl Stream<Item = Entry> + Unpin,
    pending: &HashSet<String>,
    timeout: Duration,
) -> bool {
    let mut remaining: HashSet<&str> = pending.iter().map(String::as_str).collect();
    futures_lite::pin!(stream);

    let done = async {
        while let Some(entry) = stream.next().await {
            if let Message::Tool { tool_call_id, .. } = &entry.message {
                remaining.remove(tool_call_id.as_str());
                if remaining.is_empty() {
                    return true;
                }
            }
        }
        false
    };

    futures_lite::future::or(done, async {
        async_io::Timer::after(timeout).await;
        false
    })
    .await
}
