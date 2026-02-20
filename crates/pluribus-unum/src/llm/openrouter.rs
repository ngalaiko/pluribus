use exn::ResultExt;
use futures_lite::io::BufReader;
use futures_lite::stream;
use isahc::{HttpClient, Request};
use pluribus_frequency::protocol::{Message, ToolDef};
use pluribus_frequency::state::Configuration;

use crate::llm::{openai_compat, GenOptions, LlmEventStream, Provider};

const BASE_URL: &str = "https://openrouter.ai/api/v1";
const KEY_API_KEY: &str = "openrouter.api_key";
const MODEL: &str = "openrouter/auto";

/// `OpenRouter` LLM backend using the OpenAI-compatible chat completions API.
pub struct OpenRouter {
    client: HttpClient,
    api_key: String,
}

#[derive(Debug)]
pub enum Error {
    Network(String),
    InvalidAPIKey,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(msg) => write!(f, "Network error: {msg}"),
            Self::InvalidAPIKey => write!(f, "Invalid API key"),
        }
    }
}

impl std::error::Error for Error {}

/// Store an `OpenRouter` API key in configuration.
pub fn set_api_key(
    config: &Configuration,
    key: &str,
) -> exn::Result<(), pluribus_frequency::state::Error> {
    config.set(KEY_API_KEY, &key)
}

/// Build an `OpenRouter` backend from stored configuration.
///
/// Returns `None` if no API key is stored or the stored key is invalid.
pub async fn from_config(config: &Configuration) -> Option<OpenRouter> {
    let key: String = config.get(KEY_API_KEY)?;
    OpenRouter::new(&key).await.ok()
}

impl OpenRouter {
    /// Create a new `OpenRouter` LLM backend, validating the API key.
    pub async fn new(api_key: &str) -> exn::Result<Self, Error> {
        let client = HttpClient::new().or_raise(|| Error::Network("create HTTP client".into()))?;
        let request = Request::get(format!("{BASE_URL}/auth/key"))
            .header("Authorization", format!("Bearer {api_key}"))
            .body(())
            .or_raise(|| Error::Network("build HTTP request".into()))?;
        let response = client
            .send_async(request)
            .await
            .or_raise(|| Error::Network("send HTTP request".into()))?;
        if response.status().is_success() {
            Ok(Self {
                client,
                api_key: api_key.to_owned(),
            })
        } else {
            Err(Error::InvalidAPIKey.into())
        }
    }
}

/// Internal state for the streaming unfold.
enum State {
    New,
    Streaming(BufReader<isahc::AsyncBody>),
    Finished,
}

impl Provider for OpenRouter {
    fn complete_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDef],
        options: &'a GenOptions,
    ) -> LlmEventStream<'a> {
        let url = format!("{BASE_URL}/chat/completions");
        Box::pin(stream::unfold(State::New, move |state| {
            let url = url.clone();
            async move {
                match state {
                    State::New => {
                        tracing::debug!(model = MODEL, "starting OpenRouter SSE stream");
                        match openai_compat::start_sse_stream(
                            &self.client,
                            &url,
                            &self.api_key,
                            MODEL,
                            messages,
                            tools,
                            options,
                        )
                        .await
                        {
                            Ok(mut reader) => {
                                tracing::debug!("OpenRouter SSE stream connected");
                                match openai_compat::read_next_sse_event(&mut reader).await {
                                    Ok(Some(event)) => {
                                        Some((exn::Ok(event), State::Streaming(reader)))
                                    }
                                    Ok(None) => None,
                                    Err(e) => {
                                        tracing::warn!(%e, "OpenRouter SSE stream error on first event");
                                        Some((Err(e), State::Finished))
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(%e, "failed to start OpenRouter SSE stream");
                                Some((Err(e), State::Finished))
                            }
                        }
                    }
                    State::Streaming(mut reader) => {
                        match openai_compat::read_next_sse_event(&mut reader).await {
                            Ok(Some(event)) => Some((exn::Ok(event), State::Streaming(reader))),
                            Ok(None) => {
                                tracing::debug!("OpenRouter SSE stream finished");
                                None
                            }
                            Err(e) => {
                                tracing::warn!(%e, "OpenRouter SSE stream error");
                                Some((Err(e), State::Finished))
                            }
                        }
                    }
                    State::Finished => None,
                }
            }
        }))
    }
}
