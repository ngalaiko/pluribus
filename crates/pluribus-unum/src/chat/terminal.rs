use std::future::Future;
use std::io::Write;
use std::pin::Pin;

use futures_lite::Stream;
use pluribus_frequency::protocol::{ContentPart, Message, NodeId};
use pluribus_frequency::state::Entry;
use pluribus_llm::{LlmEvent, LlmEventStream};

/// Console-based chat interface using stdin/stdout.
pub struct Chat;

impl crate::chat::Chat for Chat {
    fn messages(&self) -> Pin<Box<dyn Stream<Item = Vec<ContentPart>> + Send + '_>> {
        use futures_lite::io::AsyncBufReadExt;
        use futures_lite::StreamExt;

        let stdin = blocking::Unblock::new(std::io::stdin());
        let reader = futures_lite::io::BufReader::new(stdin);
        let lines = reader.lines();
        Box::pin(lines.filter_map(|line| {
            let Ok(line) = line else { return None };
            if line.is_empty() {
                return None;
            }
            Some(vec![ContentPart::text(line)])
        }))
    }

    fn display<'a>(
        &'a self,
        self_id: &'a NodeId,
        entry: &'a Entry,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            display_entry(self_id, entry);
        })
    }

    fn display_stream<'a>(&'a self, stream: LlmEventStream<'a>) -> LlmEventStream<'a> {
        use futures_lite::StreamExt;

        Box::pin(stream.inspect(|event| match event {
            Ok(LlmEvent::TextDelta(text)) => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            Ok(LlmEvent::ReasoningDelta(text)) => {
                // Dim ANSI styling for reasoning/thinking output.
                print!("\x1b[2m{text}\x1b[0m");
                let _ = std::io::stdout().flush();
            }
            _ => {}
        }))
    }

    fn context_hint(&self) -> &'static str {
        "You are chatting via a terminal. Use plain text."
    }
}

fn display_entry(this_node_id: &NodeId, entry: &Entry) {
    match &entry.message {
        Message::User { .. } => {
            println!("> {}", entry.message.text());
        }
        Message::Assistant { tool_calls, .. } => {
            let origin = format_origin(this_node_id, &entry.origin);
            let text = entry.message.text();
            if !text.is_empty() {
                println!("[{origin}] {text}");
            }
            for call in tool_calls {
                println!("[{origin}] [tool call] {}({})", call.name, call.arguments);
            }
        }
        Message::Tool { is_error, .. } => {
            let origin = format_origin(this_node_id, &entry.origin);
            let text = entry.message.text();
            if *is_error {
                println!("[{origin}] [tool result] ERROR: {text}");
            } else {
                println!("[{origin}] [tool result] {text}");
            }
        }
        Message::System { .. } => {}
    }
}

fn format_origin(this_node_id: &NodeId, origin: &NodeId) -> String {
    if origin == this_node_id {
        format!("{origin} (this node)")
    } else {
        format!("{origin}")
    }
}
