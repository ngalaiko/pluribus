//! How a binary says what it is doing.
//!
//! One subscriber per process, writing one line per event to stderr. The event
//! store records what the agent did; these lines describe the process that
//! carried it out. `docs/logging.md` says what belongs in them.

use std::io::{self, IsTerminal};
use std::sync::Once;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

/// The filter every binary reads. `RUST_LOG` is not consulted.
const FILTER: &str = "PLURIBUS_LOG";

static INSTALLED: Once = Once::new();

/// Installs the stderr subscriber, once. Later calls do nothing.
///
/// The filter comes from `PLURIBUS_LOG`, defaulting to `info`. Levels do not
/// depend on the terminal; only colour does.
pub fn init() {
    INSTALLED.call_once(|| {
        let filter = EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .with_env_var(FILTER)
            .from_env_lossy();
        // A subscriber another crate installed first stays; this is not an error.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .with_ansi(io::stderr().is_terminal())
            .compact()
            .try_init();
    });
}

#[cfg(test)]
mod tests {
    use super::init;

    #[test]
    fn init_is_idempotent() {
        init();
        init();
    }
}
