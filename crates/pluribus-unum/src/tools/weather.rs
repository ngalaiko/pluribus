mod met;

use std::future::Future;
use std::pin::Pin;

use pluribus_frequency::protocol::{ToolDef, ToolName};
use pluribus_frequency::state::Configuration;

use super::geocode;
use crate::geocoding::Geocoder;
use crate::tools::{Provider, Tools};

trait Backend: Send + Sync + 'static {
    fn forecast(
        &self,
        lat: f64,
        lon: f64,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>;
}

pub struct Weather;

impl Provider for Weather {
    fn resolve(&self, config: &Configuration) -> Tools {
        let mut tools = geocode::Geocoding.resolve(config);
        tools.register(ForecastTool {
            backend: Box::new(met::Backend),
        });
        match geocode::state(config) {
            geocode::State::Connected(key) => {
                let geocoder = crate::geocoding::mapsco::Backend::new(key);
                tools.register(ForecastByNameTool {
                    backend: Box::new(met::Backend),
                    geocoder: Box::new(geocoder),
                });
            }
            geocode::State::Disconnected => {
                tools.register(crate::tools::disabled_tool(
                    "getWeatherByLocation",
                    "Get weather by location name. Example: {\"location\": \"Oslo, Norway\"}",
                    "Requires geocoding backend.",
                ));
            }
        }
        tools
    }
}

/// Get weather forecast by latitude and longitude. Always available.
struct ForecastTool {
    backend: Box<dyn Backend>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ForecastInput {
    /// Latitude in decimal degrees.
    lat: f64,
    /// Longitude in decimal degrees.
    lon: f64,
}

impl crate::tools::Tool for ForecastTool {
    type Input = ForecastInput;
    type Output = String;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("getWeather"),
            "Get current weather and short-term forecast for coordinates (lat, lon). Data from MET Norway (yr.no). Example: {\"lat\": 59.91, \"lon\": 10.75}",
        )
    }

    fn execute(
        &self,
        input: ForecastInput,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        self.backend.forecast(input.lat, input.lon)
    }
}

/// Get weather forecast by location name. Requires geocoding to be connected.
struct ForecastByNameTool {
    backend: Box<dyn Backend>,
    geocoder: Box<dyn Geocoder>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ForecastByNameInput {
    /// Location name or address to get weather for.
    location: String,
}

impl crate::tools::Tool for ForecastByNameTool {
    type Input = ForecastByNameInput;
    type Output = String;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("getWeatherByLocation"),
            "Get current weather and short-term forecast for a location name. Geocodes the name then fetches weather. Data from MET Norway (yr.no). Example: {\"location\": \"Oslo, Norway\"}",
        )
    }

    fn execute(
        &self,
        input: ForecastByNameInput,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let results = self.geocoder.geocode(input.location).await?;
            let coords = results
                .into_iter()
                .next()
                .ok_or("No geocoding results found for the given location.")?;
            self.backend.forecast(coords.lat, coords.lon).await
        })
    }
}
