use std::fmt::Write as _;
use std::time::Duration;

use isahc::prelude::*;
use isahc::Request;

#[derive(serde::Serialize)]
struct RequestBody {
    query: String,
    #[serde(rename = "numResults")]
    num_results: u32,
    #[serde(rename = "type")]
    search_type: String,
    contents: RequestContents,
}

#[derive(serde::Serialize)]
struct RequestContents {
    text: RequestText,
}

#[derive(serde::Serialize)]
struct RequestText {
    #[serde(rename = "maxCharacters")]
    max_characters: u32,
}

#[derive(serde::Deserialize)]
struct Response {
    results: Vec<SearchResult>,
}

#[derive(serde::Deserialize)]
struct SearchResult {
    title: Option<String>,
    url: Option<String>,
    text: Option<String>,
}

pub async fn search(api_key: &str, query: String) -> Result<String, String> {
    let body = RequestBody {
        query,
        num_results: 5,
        search_type: "auto".to_string(),
        contents: RequestContents {
            text: RequestText {
                max_characters: 3000,
            },
        },
    };

    let json = serde_json::to_vec(&body).map_err(|e| e.to_string())?;

    let request = Request::post("https://api.exa.ai/search")
        .timeout(Duration::from_secs(30))
        .header("Content-Type", "application/json")
        .header("x-api-key", api_key)
        .body(json)
        .map_err(|e| e.to_string())?;

    let client = isahc::HttpClient::new().map_err(|e| e.to_string())?;
    let mut response = client
        .send_async(request)
        .await
        .map_err(|e| e.to_string())?;

    let text = response.text().await.map_err(|e| e.to_string())?;
    let parsed: Response = serde_json::from_str(&text).map_err(|e| e.to_string())?;

    Ok(format_results(&parsed.results))
}

fn format_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results found.".to_string();
    }

    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        let title = r.title.as_deref().unwrap_or("(no title)");
        let url = r.url.as_deref().unwrap_or("");
        let snippet = r.text.as_deref().unwrap_or("");

        let _ = writeln!(out, "{}. {}", i + 1, title);
        if !url.is_empty() {
            let _ = writeln!(out, "   {url}");
        }
        if !snippet.is_empty() {
            let _ = writeln!(out, "   {snippet}");
        }
        out.push('\n');
    }
    out
}
