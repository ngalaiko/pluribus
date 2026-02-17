use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_lite::Stream;

use pluribus_frequency::protocol::{ContentPart, Message, NodeId};
use pluribus_frequency::state::Entry;

use crate::llm::{LlmEvent, LlmEventStream};
use crate::telegram::{ApiClient, ERROR_BACKOFF, POLL_TIMEOUT};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Escape `&`, `<`, and `>` for safe inclusion in `Telegram` HTML.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Combine optional reasoning and HTML text into a single `Telegram` message.
///
/// When reasoning is present it is wrapped in an expandable blockquote so the
/// user can collapse it.
fn combine_reasoning_html(reasoning: &str, html_text: &str) -> String {
    if reasoning.is_empty() {
        html_text.to_owned()
    } else {
        format!(
            "<blockquote expandable>{}</blockquote>\n{}",
            escape_html(reasoning),
            html_text,
        )
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Minimum interval between `editMessageText` calls during streaming.
const EDIT_THROTTLE: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

/// State saved by `display_stream` for `display` to finalize.
struct StreamingState {
    message_id: i64,
}

/// `Telegram` bot chat interface.
///
/// Receives user messages via long polling and sends assistant responses
/// back to a single `Telegram` chat. Streaming LLM output is shown by
/// editing a message in place.
#[allow(clippy::struct_field_names)]
pub struct Chat {
    api: ApiClient,
    chat_id: i64,
    /// Set by `display_stream`, consumed by `display` for the final edit.
    streaming: Mutex<Option<StreamingState>>,
}

impl Chat {
    #[must_use]
    pub fn new(bot_token: &str, chat_id: i64) -> Self {
        Self {
            api: ApiClient::new(bot_token),
            chat_id,
            streaming: Mutex::new(None),
        }
    }
}

impl Chat {
    /// Send or edit a streaming snapshot, returning the message-id on first send.
    async fn send_or_edit_snapshot(
        &self,
        action: &Action,
        reasoning: &str,
        text: &str,
        msg_id: Option<i64>,
    ) -> Option<i64> {
        let combined = {
            let html_text = crate::telegram::markdown::to_telegram_html(text);
            combine_reasoning_html(reasoning, &html_text)
        };
        match action {
            Action::Send => {
                let result = if let Ok(sent) = self
                    .api
                    .send_message(self.chat_id, &combined, Some("HTML"))
                    .await
                {
                    Ok(sent)
                } else {
                    let plain = format_plain_fallback(reasoning, text);
                    self.api.send_message(self.chat_id, &plain, None).await
                };
                match result {
                    Ok(sent) => Some(sent.message_id),
                    Err(e) => {
                        tracing::warn!("Telegram sendMessage error: {e}");
                        None
                    }
                }
            }
            Action::Edit => {
                if let Some(mid) = msg_id {
                    if self
                        .api
                        .edit_message_text(self.chat_id, mid, &combined, Some("HTML"))
                        .await
                        .is_err()
                    {
                        let plain = format_plain_fallback(reasoning, text);
                        let _ = self
                            .api
                            .edit_message_text(self.chat_id, mid, &plain, None)
                            .await;
                    }
                }
                None
            }
            Action::Skip => None,
        }
    }
}

impl crate::chat::Chat for Chat {
    fn messages(&self) -> Pin<Box<dyn Stream<Item = Vec<ContentPart>> + Send + '_>> {
        struct PollState {
            offset: i64,
            buffer: VecDeque<Vec<ContentPart>>,
        }

        let chat_id = self.chat_id;
        let initial = PollState {
            offset: 0,
            buffer: VecDeque::new(),
        };

        Box::pin(futures_lite::stream::unfold(initial, move |mut state| {
            async move {
                // Drain buffer first.
                if let Some(item) = state.buffer.pop_front() {
                    return Some((item, state));
                }

                // Poll for new updates.
                loop {
                    match self.api.get_updates(state.offset, POLL_TIMEOUT).await {
                        Ok(updates) => {
                            for update in updates {
                                if update.update_id >= state.offset {
                                    state.offset = update.update_id + 1;
                                }
                                if let Some(msg) = update.message {
                                    if msg.chat.id == chat_id {
                                        if let Some(text) = msg.text {
                                            if !text.is_empty() && !text.starts_with('/') {
                                                state
                                                    .buffer
                                                    .push_back(vec![ContentPart::text(text)]);
                                            }
                                        }
                                    }
                                }
                            }
                            if let Some(item) = state.buffer.pop_front() {
                                return Some((item, state));
                            }
                            // No relevant messages — poll again.
                        }
                        Err(e) => {
                            tracing::warn!("Telegram getUpdates error: {e}");
                            async_io::Timer::after(ERROR_BACKOFF).await;
                        }
                    }
                }
            }
        }))
    }

    fn display<'a>(
        &'a self,
        self_id: &'a NodeId,
        entry: &'a Entry,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if !entry.origin.eq(self_id) {
                // Only display entries originating from this chat.
                return;
            }
            match &entry.message {
                Message::User { .. }
                | Message::System { .. }
                | Message::Scheduled { .. }
                | Message::Tool { .. } => {}

                Message::Assistant { tool_calls, .. } => {
                    let streaming = {
                        let mut guard = self.streaming.lock().expect("poisoned");
                        guard.take()
                    };

                    let text = entry.message.text();

                    // Display text (with reasoning) — edit streaming message or send new.
                    if !text.is_empty() {
                        let html_text = crate::telegram::markdown::to_telegram_html(&text);
                        let reasoning = entry.message.reasoning_content().unwrap_or_default();
                        let html = combine_reasoning_html(reasoning, &html_text);
                        let plain = format_plain_fallback(reasoning, &text);

                        if let Some(state) = streaming {
                            if let Err(e) = self
                                .api
                                .edit_message_text(
                                    self.chat_id,
                                    state.message_id,
                                    &html,
                                    Some("HTML"),
                                )
                                .await
                            {
                                tracing::warn!(
                                    "Telegram editMessageText HTML error, retrying plain: {e}"
                                );
                                let _ = self
                                    .api
                                    .edit_message_text(self.chat_id, state.message_id, &plain, None)
                                    .await;
                            }
                        } else if let Err(e) = self
                            .api
                            .send_message(self.chat_id, &html, Some("HTML"))
                            .await
                        {
                            tracing::warn!("Telegram sendMessage HTML error, retrying plain: {e}");
                            let _ = self.api.send_message(self.chat_id, &plain, None).await;
                        }
                    }

                    // Display tool calls as a separate message (no reasoning).
                    if !tool_calls.is_empty() {
                        let tool_html: String = tool_calls
                            .iter()
                            .map(|c| {
                                format!(
                                    "<code>{}({})</code>",
                                    escape_html(&c.name),
                                    escape_html(&c.arguments),
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");

                        let tool_plain: String = tool_calls
                            .iter()
                            .map(|c| format!("{}({})", c.name, c.arguments))
                            .collect::<Vec<_>>()
                            .join("\n");

                        if let Err(e) = self
                            .api
                            .send_message(self.chat_id, &tool_html, Some("HTML"))
                            .await
                        {
                            tracing::warn!(
                                "Telegram tool call sendMessage HTML error, retrying plain: {e}"
                            );
                            let _ = self.api.send_message(self.chat_id, &tool_plain, None).await;
                        }
                    }
                }
            }
        })
    }

    fn display_stream<'a>(&'a self, stream: LlmEventStream<'a>) -> LlmEventStream<'a> {
        use futures_lite::StreamExt;

        // Clear any leftover streaming state from a previous turn.
        {
            let mut guard = self.streaming.lock().expect("poisoned");
            *guard = None;
        }

        // Shared accumulator for reasoning + text content.
        let acc: Arc<Mutex<StreamAcc>> = Arc::new(Mutex::new(StreamAcc {
            reasoning: String::new(),
            text: String::new(),
            message_id: None,
            last_edit: Instant::now(),
        }));

        Box::pin(stream.then(move |event| {
            let acc = Arc::clone(&acc);
            async move {
                let is_content_delta = matches!(
                    event,
                    Ok(LlmEvent::Text(_) | LlmEvent::Reasoning(_))
                );

                if is_content_delta {
                    let (action, reasoning_snap, text_snap, msg_id) = {
                        let mut guard = acc.lock().expect("poisoned");
                        match &event {
                            Ok(LlmEvent::Text(delta)) => guard.text.push_str(delta),
                            Ok(LlmEvent::Reasoning(delta)) => {
                                guard.reasoning.push_str(delta);
                            }
                            _ => {}
                        }

                        if guard.message_id.is_none() {
                            (
                                Action::Send,
                                guard.reasoning.clone(),
                                guard.text.clone(),
                                None,
                            )
                        } else if guard.last_edit.elapsed() >= EDIT_THROTTLE {
                            (
                                Action::Edit,
                                guard.reasoning.clone(),
                                guard.text.clone(),
                                guard.message_id,
                            )
                        } else {
                            (Action::Skip, String::new(), String::new(), None)
                        }
                    };

                    if let Some(new_id) = self
                        .send_or_edit_snapshot(&action, &reasoning_snap, &text_snap, msg_id)
                        .await
                    {
                        let mut guard = acc.lock().expect("poisoned");
                        guard.message_id = Some(new_id);
                        guard.last_edit = Instant::now();
                        drop(guard);
                        let mut s = self.streaming.lock().expect("poisoned");
                        *s = Some(StreamingState { message_id: new_id });
                    } else if matches!(action, Action::Edit) {
                        let mut guard = acc.lock().expect("poisoned");
                        guard.last_edit = Instant::now();
                    }
                }
                event
            }
        }))
    }

    fn requires_leader(&self) -> bool {
        true
    }

    fn context_hint(&self) -> &'static str {
        "You are chatting via Telegram. Use markdown formatting (bold, italic, code blocks, lists, etc.)."
    }
}

enum Action {
    Send,
    Edit,
    Skip,
}

/// Shared accumulator for streaming reasoning + text content.
struct StreamAcc {
    reasoning: String,
    text: String,
    message_id: Option<i64>,
    last_edit: Instant,
}

/// Plain-text fallback when HTML send/edit fails.
fn format_plain_fallback(reasoning: &str, text: &str) -> String {
    if reasoning.is_empty() {
        text.to_owned()
    } else {
        format!("[thinking]\n{reasoning}\n[/thinking]\n\n{text}")
    }
}
