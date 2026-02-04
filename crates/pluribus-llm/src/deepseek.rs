use exn::ResultExt;
use futures_lite::io::BufReader;
use futures_lite::stream;
use isahc::{HttpClient, Request};
use pluribus_frequency::protocol::{Message, ToolDef};
use pluribus_frequency::state::Configuration;

use crate::openai_compat;
use crate::{GenOptions, LlmEventStream, Provider};

const BASE_URL: &str = "https://api.deepseek.com";
const KEY_API_KEY: &str = "deepseek.api_key";

#[derive(Clone, Copy)]
pub enum DeepSeekModel {
    Chat,
    Reasoner,
}

/// `DeepSeek` LLM backend using the OpenAI-compatible chat completions API.
pub struct DeepSeek {
    client: HttpClient,
    api_key: String,
    model: String,
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

/// Store a `DeepSeek` API key in configuration.
pub fn set_api_key(
    config: &Configuration,
    key: &str,
) -> exn::Result<(), pluribus_frequency::state::Error> {
    config.set(KEY_API_KEY, &key)
}

/// Build a `DeepSeek` chat backend from stored configuration.
///
/// Returns `None` if no API key is stored or the stored key is invalid.
pub async fn from_config(config: &Configuration) -> Option<DeepSeek> {
    let key: String = config.get(KEY_API_KEY)?;
    DeepSeek::new(&key, DeepSeekModel::Chat).await.ok()
}

impl DeepSeek {
    /// Create a new `DeepSeek` LLM backend, validating the API key.
    pub async fn new(api_key: &str, model: DeepSeekModel) -> exn::Result<Self, Error> {
        let client = HttpClient::new().or_raise(|| Error::Network("create HTTP client".into()))?;
        let model_name = match model {
            DeepSeekModel::Chat => "deepseek-chat",
            DeepSeekModel::Reasoner => "deepseek-reasoner",
        };
        let request = Request::get(format!("{BASE_URL}/user/balance"))
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
                model: model_name.to_owned(),
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

impl Provider for DeepSeek {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "deepseek"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn complete_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDef],
        options: &'a GenOptions,
    ) -> LlmEventStream<'a> {
        let url = format!("{BASE_URL}/v1/chat/completions");
        Box::pin(stream::unfold(State::New, move |state| {
            let url = url.clone();
            async move {
                match state {
                    State::New => {
                        match openai_compat::start_sse_stream(
                            &self.client,
                            &url,
                            &self.api_key,
                            &self.model,
                            messages,
                            tools,
                            options,
                        )
                        .await
                        {
                            Ok(mut reader) => {
                                match openai_compat::read_next_sse_event(&mut reader).await {
                                    Ok(Some(event)) => {
                                        Some((exn::Ok(event), State::Streaming(reader)))
                                    }
                                    Ok(None) => None,
                                    Err(e) => Some((Err(e), State::Finished)),
                                }
                            }
                            Err(e) => Some((Err(e), State::Finished)),
                        }
                    }
                    State::Streaming(mut reader) => {
                        match openai_compat::read_next_sse_event(&mut reader).await {
                            Ok(Some(event)) => Some((exn::Ok(event), State::Streaming(reader))),
                            Ok(None) => None,
                            Err(e) => Some((Err(e), State::Finished)),
                        }
                    }
                    State::Finished => None,
                }
            }
        }))
    }
}
