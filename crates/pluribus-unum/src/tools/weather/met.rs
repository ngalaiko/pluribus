use std::fmt::Write;
use std::time::Duration;

use isahc::prelude::*;
use isahc::Request;

pub struct Backend;

impl super::Backend for Backend {
    fn forecast(
        &self,
        lat: f64,
        lon: f64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        Box::pin(fetch_weather(lat, lon))
    }
}

/// A single forecast timeseries entry from the MET API.
#[derive(serde::Deserialize)]
struct Timeseries {
    time: String,
    data: TimeseriesData,
}

#[derive(serde::Deserialize)]
struct TimeseriesData {
    instant: Instant,
    next_1_hours: Option<Next1Hours>,
}

#[derive(serde::Deserialize)]
struct Instant {
    details: InstantDetails,
}

#[derive(serde::Deserialize)]
struct InstantDetails {
    air_temperature: Option<f64>,
    wind_speed: Option<f64>,
    wind_from_direction: Option<f64>,
    relative_humidity: Option<f64>,
    air_pressure_at_sea_level: Option<f64>,
}

#[derive(serde::Deserialize)]
struct Next1Hours {
    summary: Summary,
    details: Option<PeriodDetails>,
}

#[derive(serde::Deserialize)]
struct Summary {
    symbol_code: String,
}

#[derive(serde::Deserialize)]
struct PeriodDetails {
    precipitation_amount: Option<f64>,
}

#[derive(serde::Deserialize)]
struct ForecastResponse {
    properties: Properties,
}

#[derive(serde::Deserialize)]
struct Properties {
    timeseries: Vec<Timeseries>,
}

const WIND_DIRS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];

fn wind_direction_name(degrees: f64) -> &'static str {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let index = (((degrees + 22.5) % 360.0) / 45.0) as usize;
    WIND_DIRS.get(index).copied().unwrap_or("N")
}

fn format_entry(entry: &Timeseries) -> String {
    let mut out = String::new();
    let _ = write!(out, "Time: {}", entry.time);

    let d = &entry.data.instant.details;
    if let Some(t) = d.air_temperature {
        let _ = write!(out, " | Temp: {t:.1}\u{00b0}C");
    }
    if let Some(ws) = d.wind_speed {
        let _ = write!(out, " | Wind: {ws:.1} m/s");
        if let Some(wd) = d.wind_from_direction {
            let _ = write!(out, " from {}", wind_direction_name(wd));
        }
    }
    if let Some(rh) = d.relative_humidity {
        let _ = write!(out, " | Humidity: {rh:.0}%");
    }
    if let Some(p) = d.air_pressure_at_sea_level {
        let _ = write!(out, " | Pressure: {p:.0} hPa");
    }
    if let Some(next) = &entry.data.next_1_hours {
        let _ = write!(out, " | Condition: {}", next.summary.symbol_code);
        if let Some(pd) = &next.details {
            if let Some(precip) = pd.precipitation_amount {
                if precip > 0.0 {
                    let _ = write!(out, " | Precip: {precip:.1} mm");
                }
            }
        }
    }
    out
}

const MAX_ENTRIES: usize = 7;

async fn fetch_weather(lat: f64, lon: f64) -> Result<String, String> {
    let url = format!(
        "https://api.met.no/weatherapi/locationforecast/2.0/compact?lat={lat:.4}&lon={lon:.4}",
    );

    let request = Request::get(&url)
        .timeout(Duration::from_secs(15))
        .header("User-Agent", "Pluribus/1.0")
        .header("Accept", "application/json")
        .body(())
        .map_err(|e| e.to_string())?;

    let client = isahc::HttpClient::new().map_err(|e| e.to_string())?;
    let mut response = client
        .send_async(request)
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("MET API returned status {}", response.status()));
    }

    let body = response.text().await.map_err(|e| e.to_string())?;
    let forecast: ForecastResponse =
        serde_json::from_str(&body).map_err(|e| format!("failed to parse MET response: {e}"))?;

    let entries = forecast
        .properties
        .timeseries
        .iter()
        .take(MAX_ENTRIES)
        .map(format_entry)
        .collect::<Vec<_>>()
        .join("\n");

    Ok(format!(
        "Weather forecast for ({lat:.4}, {lon:.4}):\n{entries}",
    ))
}
