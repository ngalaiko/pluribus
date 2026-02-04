pub mod noop;
pub mod telegram;
pub mod terminal;

use std::future::Future;
use std::pin::Pin;

use futures_lite::Stream;
use pluribus_frequency::protocol::{ContentPart, NodeId};
use pluribus_frequency::state::Entry;
use pluribus_llm::LlmEventStream;

/// A chat interface for human interaction.
///
/// Implementations handle both user input (via a stream of content parts)
/// and output (displaying log entries and streaming LLM responses).
pub trait Chat: Send + Sync {
    /// Stream of user input. Each item is a list of content parts that maps
    /// directly to `Message::User { content }`. Ends when the user disconnects.
    fn messages(&self) -> Pin<Box<dyn Stream<Item = Vec<ContentPart>> + Send + '_>>;

    /// Display a completed log entry (from peers, tool results, etc.).
    /// `self_id` is this node's ID, for display context.
    fn display<'a>(
        &'a self,
        self_id: &'a NodeId,
        entry: &'a Entry,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Wrap an LLM event stream with display side-effects.
    ///
    /// Returns a stream yielding the same events. The caller passes
    /// the returned stream to `collect_response` for message collection.
    fn display_stream<'a>(&'a self, stream: LlmEventStream<'a>) -> LlmEventStream<'a>;

    /// Whether this chat source requires the node to be the leader (lowest
    /// `NodeId`) before accepting messages.
    ///
    /// Defaults to `false`. Override to `true` for shared sources like
    /// `Telegram` where only one node should consume messages.
    fn requires_leader(&self) -> bool {
        false
    }

    /// A hint to include in the system prompt describing this chat interface.
    ///
    /// Lets the LLM tailor its formatting to the output medium.
    fn context_hint(&self) -> &'static str {
        ""
    }
}

#[must_use]
pub const fn terminal() -> terminal::Chat {
    terminal::Chat
}

#[must_use]
pub const fn noop() -> noop::Chat {
    noop::Chat
}

#[must_use]
pub fn telegram(bot_token: &str, chat_id: i64) -> telegram::Chat {
    telegram::Chat::new(bot_token, chat_id)
}
