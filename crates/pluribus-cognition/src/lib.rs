//! Event routing for one agent.
//!
//! RLM strategy lives in the cognition plugin; this crate routes and gates events.

pub mod agent;
pub mod dispatch;

#[cfg(test)]
mod tests;

pub use agent::{Agent, AgentError, AuthorityResolver, ComponentInstall, Progress};
pub use dispatch::{Registration, Routed, Router, RouterError, Subscriptions, payload_field};

mod authority;
pub use authority::{Connector, OriginAuthority, OriginConstraints};
