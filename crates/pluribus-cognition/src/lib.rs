//! Event routing for one agent.
//!
//! RLM strategy lives in the cognition plugin; this crate routes and gates events.

pub mod agent;
pub mod dispatch;

#[cfg(test)]
mod tests;

pub use agent::{Agent, AgentError, AuthorityResolver, ComponentInstall, Progress};
pub use dispatch::{
    PendingTimer, Registration, Routed, Router, RouterError, Subscriptions, fire_timer,
    payload_field, pending_timers,
};

mod authority;
pub use authority::{Connector, OriginAuthority, OriginConstraints};
