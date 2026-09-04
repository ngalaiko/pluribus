//! Where an agent keeps each kind of thing.
//!
//! The platform decides: XDG base directories on Linux, Apple's standard
//! directories on macOS, known folders on Windows. Every binary resolves the
//! same paths, so an endpoint one of them creates is the one another expects.

use directories::ProjectDirs;
use std::path::PathBuf;

/// The name every directory is called, on every platform.
const APPLICATION: &str = "pluribus";

fn project() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", APPLICATION)
}

/// Configuration: what an operator writes and reads back.
#[must_use]
pub fn config() -> PathBuf {
    project().map_or_else(
        || PathBuf::from(".pluribus"),
        |dirs| dirs.config_dir().to_owned(),
    )
}

/// State: the event store, blobs, and the emergency stop. Back this up.
#[must_use]
pub fn state() -> PathBuf {
    project().map_or_else(
        || PathBuf::from(".pluribus"),
        |dirs| dirs.data_dir().to_owned(),
    )
}

/// Cache: packages fetched by digest, reconstructible from their references.
#[must_use]
pub fn cache() -> PathBuf {
    project().map_or_else(
        || PathBuf::from(".pluribus"),
        |dirs| dirs.cache_dir().to_owned(),
    )
}

/// Runtime: endpoints a person's daemons listen on, gone after a reboot.
///
/// Platforms without one, macOS among them, keep endpoints with the state,
/// which is private to the account and stable across restarts.
#[must_use]
pub fn runtime_dir() -> Option<PathBuf> {
    project().and_then(|dirs| dirs.runtime_dir().map(ToOwned::to_owned))
}

/// Where endpoints belong when nothing names a state directory.
#[must_use]
pub fn runtime() -> PathBuf {
    runtime_dir().unwrap_or_else(state)
}
