use std::fmt::Write as _;
use std::time::Duration;

use isahc::prelude::*;
use isahc::Request;

use super::{Coordinates, Geocoder};

#[derive(Clone)]
pub struct Backend {
    api_key: String,
}

impl Backend {
    pub const fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

impl Geocoder for Backend {
    fn geocode(
        &self,
        query: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<Coordinates>, String>> + Send + '_>,
    > {
        Box::pin(forward_geocode(&self.api_key, query))
    }

    fn reverse_geocode(
        &self,
        lat: f64,
        lon: f64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        Box::pin(reverse_geocode(&self.api_key, lat, lon))
    }
}

const MAX_RESULTS: usize = 5;

#[derive(serde::Deserialize)]
struct ForwardResult {
    lat: String,
    lon: String,
    display_name: String,
}

#[derive(serde::Deserialize)]
struct ReverseResult {
    display_name: String,
    address: Option<Address>,
}

#[derive(serde::Deserialize)]
struct Address {
    house_number: Option<String>,
    road: Option<String>,
    city: Option<String>,
    town: Option<String>,
    village: Option<String>,
    state: Option<String>,
    postcode: Option<String>,
    country: Option<String>,
}

async fn forward_geocode(api_key: &str, query: String) -> Result<Vec<Coordinates>, String> {
    let encoded = percent_encode(&query);
    let url = format!("https://geocode.maps.co/search?q={encoded}&api_key={api_key}",);

    let request = Request::get(&url)
        .timeout(Duration::from_secs(15))
        .header("User-Agent", "Pluribus/1.0")
        .body(())
        .map_err(|e| e.to_string())?;

    let client = isahc::HttpClient::new().map_err(|e| e.to_string())?;
    let mut response = client
        .send_async(request)
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!(
            "geocode.maps.co returned status {}",
            response.status()
        ));
    }

    let body = response.text().await.map_err(|e| e.to_string())?;
    let results: Vec<ForwardResult> = serde_json::from_str(&body)
        .map_err(|e| format!("failed to parse geocoding response: {e}"))?;

    results
        .into_iter()
        .take(MAX_RESULTS)
        .map(|r| {
            let lat = r
                .lat
                .parse::<f64>()
                .map_err(|e| format!("invalid lat: {e}"))?;
            let lon = r
                .lon
                .parse::<f64>()
                .map_err(|e| format!("invalid lon: {e}"))?;
            Ok(Coordinates {
                lat,
                lon,
                display_name: r.display_name,
            })
        })
        .collect()
}

async fn reverse_geocode(api_key: &str, lat: f64, lon: f64) -> Result<String, String> {
    let url = format!("https://geocode.maps.co/reverse?lat={lat}&lon={lon}&api_key={api_key}",);

    let request = Request::get(&url)
        .timeout(Duration::from_secs(15))
        .header("User-Agent", "Pluribus/1.0")
        .body(())
        .map_err(|e| e.to_string())?;

    let client = isahc::HttpClient::new().map_err(|e| e.to_string())?;
    let mut response = client
        .send_async(request)
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!(
            "geocode.maps.co returned status {}",
            response.status()
        ));
    }

    let body = response.text().await.map_err(|e| e.to_string())?;
    let result: ReverseResult = serde_json::from_str(&body)
        .map_err(|e| format!("failed to parse reverse geocoding response: {e}"))?;

    let mut out = String::new();
    let _ = writeln!(out, "Reverse geocoding for ({lat}, {lon}):");
    let _ = writeln!(out, "  {}", result.display_name);
    if let Some(addr) = &result.address {
        let _ = write!(out, "  Address:");
        if let Some(v) = &addr.house_number {
            let _ = write!(out, " {v}");
        }
        if let Some(v) = &addr.road {
            let _ = write!(out, " {v},");
        }
        let city = addr
            .city
            .as_deref()
            .or(addr.town.as_deref())
            .or(addr.village.as_deref());
        if let Some(v) = city {
            let _ = write!(out, " {v},");
        }
        if let Some(v) = &addr.state {
            let _ = write!(out, " {v}");
        }
        if let Some(v) = &addr.postcode {
            let _ = write!(out, " {v},");
        }
        if let Some(v) = &addr.country {
            let _ = write!(out, " {v}");
        }
        out.push('\n');
    }
    Ok(out)
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
