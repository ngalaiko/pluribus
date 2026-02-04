use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use pluribus_frequency::protocol::{ToolDef, ToolName};
use pluribus_frequency::state::Configuration;

use crate::geocoding::{Coordinates, Geocoder};
use crate::tools::{Provider, Tools};

const KEY_MAPSCO_API_KEY: &str = "geocoding.mapsco_api_key";

pub enum State {
    Disconnected,
    Connected(String),
}

#[must_use]
pub fn state(config: &Configuration) -> State {
    config
        .get(KEY_MAPSCO_API_KEY)
        .map_or(State::Disconnected, State::Connected)
}

pub struct Geocoding;

impl Provider for Geocoding {
    fn resolve(&self, config: &Configuration) -> Tools {
        let mut tools = Tools::new();
        match state(config) {
            State::Disconnected => {
                tools.register(crate::tools::connect_tool(
                    KEY_MAPSCO_API_KEY,
                    "connect_mapsco",
                    "Connect maps.co geocoding service. Get a free API key at geocode.maps.co.",
                    config.clone(),
                ));
            }
            State::Connected(key) => {
                let backend = crate::geocoding::mapsco::Backend::new(key);
                tools.register(ForwardTool {
                    geocoder: Box::new(backend.clone()),
                });
                tools.register(ReverseTool {
                    geocoder: Box::new(backend),
                });
                tools.register(crate::tools::disconnect_tool(
                    &[KEY_MAPSCO_API_KEY],
                    "disconnect_mapsco",
                    "Disconnect maps.co geocoding",
                    config.clone(),
                ));
            }
        }
        tools
    }
}

struct ForwardTool {
    geocoder: Box<dyn Geocoder>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ForwardInput {
    /// Address, place name, or location query to geocode.
    query: String,
}

impl crate::tools::Tool for ForwardTool {
    type Input = ForwardInput;
    type Output = String;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("geocode"),
            "Forward geocode an address or place name to coordinates (latitude/longitude). Powered by geocode.maps.co.",
        )
    }

    fn execute(
        &self,
        input: ForwardInput,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let query = input.query;
            let results = self.geocoder.geocode(query.clone()).await?;
            Ok(format_coordinates(&query, &results))
        })
    }
}

fn format_coordinates(query: &str, results: &[Coordinates]) -> String {
    if results.is_empty() {
        return format!("No geocoding results found for \"{query}\".");
    }

    let mut out = String::new();
    let _ = writeln!(out, "Geocoding results for \"{query}\":");
    for (i, r) in results.iter().enumerate() {
        let _ = writeln!(
            out,
            "{}. {} (lat: {}, lon: {})",
            i + 1,
            r.display_name,
            r.lat,
            r.lon
        );
    }
    out
}

struct ReverseTool {
    geocoder: Box<dyn Geocoder>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ReverseInput {
    /// Latitude in decimal degrees.
    lat: f64,
    /// Longitude in decimal degrees.
    lon: f64,
}

impl crate::tools::Tool for ReverseTool {
    type Input = ReverseInput;
    type Output = String;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("reverseGeocode"),
            "Reverse geocode coordinates (latitude/longitude) to an address. Powered by geocode.maps.co.",
        )
    }

    fn execute(
        &self,
        input: ReverseInput,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        self.geocoder.reverse_geocode(input.lat, input.lon)
    }
}
