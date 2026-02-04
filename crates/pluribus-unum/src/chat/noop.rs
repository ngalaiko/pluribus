use std::future::Future;
use std::pin::Pin;

use futures_lite::Stream;
use pluribus_frequency::protocol::{ContentPart, NodeId};
use pluribus_frequency::state::Entry;
use pluribus_llm::LlmEventStream;

/// No-op chat interface. Produces no input and discards all output.
pub struct Chat;

impl crate::chat::Chat for Chat {
    fn messages(&self) -> Pin<Box<dyn Stream<Item = Vec<ContentPart>> + Send + '_>> {
        Box::pin(futures_lite::stream::empty())
    }

    fn display<'a>(
        &'a self,
        _self_id: &'a NodeId,
        _entry: &'a Entry,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn display_stream<'a>(&'a self, stream: LlmEventStream<'a>) -> LlmEventStream<'a> {
        stream
    }
}
