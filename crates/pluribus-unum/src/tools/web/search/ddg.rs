use std::fmt::Write as _;
use std::time::Duration;

use isahc::config::RedirectPolicy;
use isahc::prelude::*;
use isahc::Request;

const MAX_RESULTS: usize = 8;

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

pub async fn search(query: String) -> Result<String, String> {
    let encoded = percent_encode(&query);
    let url = format!("https://html.duckduckgo.com/html/?q={encoded}");

    let request = Request::get(&url)
        .timeout(Duration::from_secs(15))
        .redirect_policy(RedirectPolicy::Limit(5))
        .header("User-Agent", "Pluribus/1.0")
        .body(())
        .map_err(|e| e.to_string())?;

    let client = isahc::HttpClient::new().map_err(|e| e.to_string())?;
    let mut response = client
        .send_async(request)
        .await
        .map_err(|e| e.to_string())?;

    let html = response.text().await.map_err(|e| e.to_string())?;
    let results = parse_results(&html);

    if results.is_empty() {
        return Ok("No results found.".to_string());
    }

    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        let _ = writeln!(out, "{}. {}", i + 1, r.title);
        if !r.url.is_empty() {
            let _ = writeln!(out, "   {}", r.url);
        }
        if !r.snippet.is_empty() {
            let _ = writeln!(out, "   {}", r.snippet);
        }
        out.push('\n');
    }
    Ok(out)
}

fn parse_results(html: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();

    // DuckDuckGo HTML search results have class="result__a" for links
    // and class="result__snippet" for snippets.
    // We parse them with simple string scanning rather than pulling in an HTML parser.

    let mut pos = 0;
    while results.len() < MAX_RESULTS {
        let Some(link_start) = html[pos..].find("class=\"result__a\"") else {
            break;
        };
        let link_start = pos + link_start;
        pos = link_start + 1;

        let title = extract_tag_text(html, link_start);
        let url = extract_href(html, link_start);

        let snippet_search_end = (link_start + 2000).min(html.len());
        let snippet = html[link_start..snippet_search_end]
            .find("class=\"result__snippet\"")
            .map(|offset| extract_tag_text(html, link_start + offset))
            .unwrap_or_default();

        if !title.is_empty() || !url.is_empty() {
            results.push(SearchResult {
                title,
                url: decode_ddg_url(&url),
                snippet,
            });
        }
    }

    results
}

/// Extract text content between `>` and `</` after the given position.
fn extract_tag_text(html: &str, from: usize) -> String {
    let Some(gt) = html[from..].find('>') else {
        return String::new();
    };
    let text_start = from + gt + 1;

    let Some(lt) = html[text_start..].find("</") else {
        return String::new();
    };

    let raw = &html[text_start..text_start + lt];
    strip_tags_and_decode(raw)
}

/// Extract href="..." value from a tag starting near the given position.
fn extract_href(html: &str, from: usize) -> String {
    let search_start = from.saturating_sub(200);
    let chunk = &html[search_start..html.len().min(from + 500)];

    let Some(href_pos) = chunk.find("href=\"") else {
        return String::new();
    };
    let value_start = href_pos + 6;

    let Some(quote_end) = chunk[value_start..].find('"') else {
        return String::new();
    };

    chunk[value_start..value_start + quote_end].to_string()
}

/// `DuckDuckGo` wraps URLs in a redirect: `//duckduckgo.com/l/?uddg=<encoded>&...`
fn decode_ddg_url(url: &str) -> String {
    url.find("uddg=").map_or_else(
        || url.to_string(),
        |uddg_start| {
            let value_start = uddg_start + 5;
            let value_end = url[value_start..]
                .find('&')
                .map_or(url.len(), |i| value_start + i);
            let encoded = &url[value_start..value_end];
            percent_decode(encoded)
        },
    )
}

/// Percent-encode a string for use in a URL query parameter.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Decode a percent-encoded string.
fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(decoded) = decode_hex_pair(bytes[i + 1], bytes[i + 2]) {
                out.push(decoded);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let h = hex_val(hi)?;
    let l = hex_val(lo)?;
    Some(h << 4 | l)
}

const fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Remove HTML tags and decode common entities.
fn strip_tags_and_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}
