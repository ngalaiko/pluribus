mod current_time;
mod geocode;
pub mod memory;
mod message;
pub mod schedule;
pub mod telegram;
mod weather;
mod web;

use std::future::Future;
use std::pin::Pin;

use pluribus_frequency::protocol::{ToolDef, ToolName};
use pluribus_frequency::state::{Configuration, State};
use schemars::JsonSchema;
use serde::{de::DeserializeOwned, Serialize};

/// A provider resolves its current configuration into a set of tools.
pub trait Provider: Send + Sync {
    fn resolve(&self, config: &Configuration) -> Tools;
}

/// Build the tool set from current state.
#[must_use]
pub fn resolve(state: &State) -> Tools {
    let config = state.configuration();
    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(weather::Weather),
        Box::new(memory::Memory),
        Box::new(schedule::Scheduler),
        Box::new(telegram::Telegram),
        Box::new(message::Message),
        Box::new(web::Web),
    ];
    let mut tools = Tools::new();
    tools.register(current_time::new());
    for p in &providers {
        tools.merge(p.resolve(&config));
    }
    tools
}

/// A typed tool that can be executed.
///
/// Tool authors implement only `execute`. Name, description, and schema
/// live on [`ToolDef`], paired at registration via [`Tools`].
pub trait Tool: Send + Sync + 'static {
    /// The input type, deserialized from JSON.
    type Input: DeserializeOwned + JsonSchema + Send;
    /// The output type, serialized to JSON.
    type Output: Serialize + Send;

    fn def(&self) -> ToolDef;

    /// Execute the tool with the given typed input.
    fn execute(
        &self,
        input: Self::Input,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, String>> + Send + '_>>;
}

/// Type-erased version of [`Tool`].
trait DynTool: Send + Sync {
    fn def(&self) -> ToolDef;

    fn execute_dyn(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + '_>>;
}

impl<T: Tool> DynTool for T {
    fn def(&self) -> ToolDef {
        Tool::def(self)
    }

    fn execute_dyn(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + '_>> {
        let parsed: Result<T::Input, _> = serde_json::from_value(input);
        match parsed {
            Ok(typed_input) => Box::pin(async move {
                let output = self.execute(typed_input).await?;
                serde_json::to_value(output).map_err(|e| e.to_string())
            }),
            Err(e) => {
                let msg = format!("invalid input: {e}");
                Box::pin(async move { Err(msg) })
            }
        }
    }
}

/// A dynamic, type-erased collection of tools.
pub struct Tools {
    tools: Vec<Box<dyn DynTool>>,
}

impl Tools {
    /// Create an empty tool container.
    #[must_use]
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Register a tool.
    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.push(Box::new(tool));
    }

    /// Merge all tools from another collection into this one.
    pub fn merge(&mut self, other: Self) {
        self.tools.extend(other.tools);
    }

    /// Collect the definitions of all registered tools.
    #[must_use]
    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools.iter().map(|t| t.def()).collect()
    }

    /// Execute the tool identified by `name` with the given JSON input.
    pub async fn execute(
        &self,
        name: &ToolName,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        for tool in &self.tools {
            if tool.def().name() == name {
                return tool.execute_dyn(input).await;
            }
        }
        Err(format!("tool not found: {name}"))
    }
}

// ---------------------------------------------------------------------------
// Generic helper tools: ConnectTool, DisconnectTool, DisabledTool
// ---------------------------------------------------------------------------

/// A tool that stores an API key in configuration to enable a provider.
struct ConnectTool {
    name: ToolName,
    description: String,
    config_key: String,
    config: Configuration,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ConnectInput {
    /// The API key.
    api_key: String,
}

#[derive(serde::Serialize)]
struct ConnectOutput {
    connected: bool,
}

impl Tool for ConnectTool {
    type Input = ConnectInput;
    type Output = ConnectOutput;

    fn def(&self) -> ToolDef {
        ToolDef::new(self.name.clone(), &self.description)
    }

    fn execute(
        &self,
        input: ConnectInput,
    ) -> Pin<Box<dyn Future<Output = Result<ConnectOutput, String>> + Send + '_>> {
        Box::pin(async move {
            self.config
                .set(&self.config_key, &input.api_key)
                .map_err(|e| e.to_string())?;
            Ok(ConnectOutput { connected: true })
        })
    }
}

/// Build a connect tool that stores a single API key.
#[must_use]
pub fn connect_tool(
    config_key: &str,
    name: &str,
    description: &str,
    config: Configuration,
) -> impl Tool {
    ConnectTool {
        name: ToolName::new(name),
        description: description.to_owned(),
        config_key: config_key.to_owned(),
        config,
    }
}

/// A tool that removes configuration keys to disable a provider.
struct DisconnectTool {
    name: ToolName,
    description: String,
    config_keys: Vec<String>,
    config: Configuration,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct DisconnectInput {}

#[derive(serde::Serialize)]
struct DisconnectOutput {
    disconnected: bool,
}

impl Tool for DisconnectTool {
    type Input = DisconnectInput;
    type Output = DisconnectOutput;

    fn def(&self) -> ToolDef {
        ToolDef::new(self.name.clone(), &self.description)
    }

    fn execute(
        &self,
        _input: DisconnectInput,
    ) -> Pin<Box<dyn Future<Output = Result<DisconnectOutput, String>> + Send + '_>> {
        Box::pin(async {
            for key in &self.config_keys {
                let _ = self.config.delete(key);
            }
            Ok(DisconnectOutput { disconnected: true })
        })
    }
}

/// Build a disconnect tool that removes one or more configuration keys.
#[must_use]
pub fn disconnect_tool(
    config_keys: &[&str],
    name: &str,
    description: &str,
    config: Configuration,
) -> impl Tool {
    DisconnectTool {
        name: ToolName::new(name),
        description: description.to_owned(),
        config_keys: config_keys.iter().map(|s| (*s).to_owned()).collect(),
        config,
    }
}

/// A tool that appears in the tool list but always returns an error when called.
/// The description includes the reason it is disabled.
struct DisabledTool {
    name: ToolName,
    description: String,
    reason: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct DisabledInput {}

impl Tool for DisabledTool {
    type Input = DisabledInput;
    type Output = String;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            self.name.clone(),
            format!("{} [DISABLED: {}]", self.description, self.reason),
        )
    }

    fn execute(
        &self,
        _input: DisabledInput,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        let reason = self.reason.clone();
        Box::pin(async move { Err(reason) })
    }
}

/// Build a disabled tool that shows up in the tool list but returns an error.
#[must_use]
pub fn disabled_tool(name: &str, description: &str, reason: &str) -> impl Tool {
    DisabledTool {
        name: ToolName::new(name),
        description: description.to_owned(),
        reason: reason.to_owned(),
    }
}
