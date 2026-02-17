pub mod markdown;

use std::time::Duration;

use isahc::config::Configurable;
use isahc::AsyncReadResponseExt;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// `Telegram` Bot API limits messages to 4096 characters.
const MAX_MESSAGE_LEN: usize = 4096;

/// Long-poll timeout sent to `Telegram` (seconds, server-side).
pub const POLL_TIMEOUT: u64 = 30;

/// HTTP request timeout for `getUpdates` (must exceed [`POLL_TIMEOUT`]).
const HTTP_TIMEOUT: Duration = Duration::from_secs(45);

/// Backoff duration after a `getUpdates` error.
pub const ERROR_BACKOFF: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Telegram Bot API wire types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ApiResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TgMessage>,
}

#[derive(Deserialize)]
pub struct TgMessage {
    #[allow(dead_code)]
    pub message_id: i64,
    pub chat: TgChat,
    pub text: Option<String>,
    pub from: Option<TgUser>,
    pub photo: Option<Vec<PhotoSize>>,
    pub caption: Option<String>,
}

#[derive(Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
}

#[derive(Deserialize)]
pub struct TgFile {
    pub file_path: Option<String>,
}

#[derive(Deserialize)]
pub struct TgChat {
    pub id: i64,
}

#[derive(Deserialize)]
pub struct TgUser {
    pub first_name: String,
}

#[derive(Deserialize)]
pub struct BotUser {
    pub username: Option<String>,
    pub first_name: String,
}

#[derive(Deserialize)]
pub struct SentMessage {
    pub message_id: i64,
}

#[derive(Serialize)]
struct GetUpdatesBody {
    offset: i64,
    timeout: u64,
    allowed_updates: Vec<String>,
}

#[derive(Serialize)]
struct SendMessageBody<'a> {
    chat_id: i64,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<&'a str>,
}

#[derive(Serialize)]
struct EditMessageBody<'a> {
    chat_id: i64,
    message_id: i64,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// API client
// ---------------------------------------------------------------------------

pub struct ApiClient {
    http: isahc::HttpClient,
    base_url: String,
    bot_token: String,
}

impl ApiClient {
    pub fn new(bot_token: &str) -> Self {
        Self {
            http: isahc::HttpClient::new().expect("create HTTP client"),
            base_url: format!("https://api.telegram.org/bot{bot_token}"),
            bot_token: bot_token.to_owned(),
        }
    }

    pub async fn get_me(&self) -> Result<BotUser, String> {
        let url = format!("{}/getMe", self.base_url);

        let request = isahc::Request::get(&url)
            .body(())
            .map_err(|e: isahc::http::Error| e.to_string())?;

        let mut response = self
            .http
            .send_async(request)
            .await
            .map_err(|e| e.to_string())?;
        let text = response.text().await.map_err(|e| e.to_string())?;
        let parsed: ApiResponse<BotUser> =
            serde_json::from_str(&text).map_err(|e| e.to_string())?;

        if parsed.ok {
            parsed.result.ok_or_else(|| "no result".into())
        } else {
            Err(parsed.description.unwrap_or_else(|| "unknown error".into()))
        }
    }

    pub async fn get_updates(&self, offset: i64, timeout: u64) -> Result<Vec<Update>, String> {
        let url = format!("{}/getUpdates", self.base_url);
        let body = GetUpdatesBody {
            offset,
            timeout,
            allowed_updates: vec!["message".into()],
        };
        let json = serde_json::to_vec(&body).map_err(|e| e.to_string())?;

        let request = isahc::Request::post(&url)
            .timeout(HTTP_TIMEOUT)
            .header("Content-Type", "application/json")
            .body(json)
            .map_err(|e: isahc::http::Error| e.to_string())?;

        let mut response = self
            .http
            .send_async(request)
            .await
            .map_err(|e| e.to_string())?;
        let text = response.text().await.map_err(|e| e.to_string())?;
        let parsed: ApiResponse<Vec<Update>> =
            serde_json::from_str(&text).map_err(|e| e.to_string())?;

        if parsed.ok {
            Ok(parsed.result.unwrap_or_default())
        } else {
            Err(parsed.description.unwrap_or_else(|| "unknown error".into()))
        }
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<SentMessage, String> {
        let url = format!("{}/sendMessage", self.base_url);
        let truncated = truncate(text);
        let body = SendMessageBody {
            chat_id,
            text: &truncated,
            parse_mode,
        };
        let json = serde_json::to_vec(&body).map_err(|e| e.to_string())?;

        let request = isahc::Request::post(&url)
            .header("Content-Type", "application/json")
            .body(json)
            .map_err(|e: isahc::http::Error| e.to_string())?;

        let mut response = self
            .http
            .send_async(request)
            .await
            .map_err(|e| e.to_string())?;
        let resp_text = response.text().await.map_err(|e| e.to_string())?;
        let parsed: ApiResponse<SentMessage> =
            serde_json::from_str(&resp_text).map_err(|e| e.to_string())?;

        if parsed.ok {
            parsed.result.ok_or_else(|| "no result".into())
        } else {
            Err(parsed.description.unwrap_or_else(|| "unknown error".into()))
        }
    }

    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<(), String> {
        let url = format!("{}/editMessageText", self.base_url);
        let truncated = truncate(text);
        let body = EditMessageBody {
            chat_id,
            message_id,
            text: &truncated,
            parse_mode,
        };
        let json = serde_json::to_vec(&body).map_err(|e| e.to_string())?;

        let request = isahc::Request::post(&url)
            .header("Content-Type", "application/json")
            .body(json)
            .map_err(|e: isahc::http::Error| e.to_string())?;

        let mut response = self
            .http
            .send_async(request)
            .await
            .map_err(|e| e.to_string())?;
        // Consume body to release the connection.
        let _ = response.text().await;
        Ok(())
    }

    pub async fn get_file(&self, file_id: &str) -> Result<TgFile, String> {
        let url = format!("{}/getFile", self.base_url);
        let body = serde_json::json!({ "file_id": file_id });
        let json = serde_json::to_vec(&body).map_err(|e| e.to_string())?;

        let request = isahc::Request::post(&url)
            .header("Content-Type", "application/json")
            .body(json)
            .map_err(|e: isahc::http::Error| e.to_string())?;

        let mut response = self
            .http
            .send_async(request)
            .await
            .map_err(|e| e.to_string())?;
        let text = response.text().await.map_err(|e| e.to_string())?;
        let parsed: ApiResponse<TgFile> =
            serde_json::from_str(&text).map_err(|e| e.to_string())?;

        if parsed.ok {
            parsed.result.ok_or_else(|| "no result".into())
        } else {
            Err(parsed.description.unwrap_or_else(|| "unknown error".into()))
        }
    }

    #[must_use]
    pub fn file_url(&self, file_path: &str) -> String {
        format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.bot_token, file_path
        )
    }
}

pub fn truncate(text: &str) -> String {
    if text.len() <= MAX_MESSAGE_LEN {
        text.to_owned()
    } else {
        let mut s = text[..MAX_MESSAGE_LEN - 3].to_owned();
        s.push_str("...");
        s
    }
}
