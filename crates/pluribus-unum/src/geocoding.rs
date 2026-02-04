pub mod mapsco;

use std::future::Future;
use std::pin::Pin;

/// A geocoded location with coordinates and display name.
pub struct Coordinates {
    pub lat: f64,
    pub lon: f64,
    pub display_name: String,
}

/// A geocoding provider that can resolve addresses to coordinates and vice versa.
pub trait Geocoder: Send + Sync + 'static {
    /// Forward geocode: resolve a query string to a list of coordinates.
    fn geocode(
        &self,
        query: String,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Coordinates>, String>> + Send + '_>>;

    /// Reverse geocode: resolve coordinates to a display name.
    fn reverse_geocode(
        &self,
        lat: f64,
        lon: f64,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>;
}
