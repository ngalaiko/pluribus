use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub manifest_version: u32,
    pub abi: String,
    pub id: String,
    pub name: String,
    pub config_schema: String,
    pub license: Option<String>,
    #[serde(default)]
    pub credentials: Vec<CredentialDeclaration>,
    pub components: std::collections::BTreeMap<String, ComponentManifest>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ComponentManifest {
    pub world: String,
    pub component: String,
    pub digest: String,
    pub imports: Vec<String>,
    pub config_schema: String,
    /// Capabilities this plugin answers. The host routes
    /// `capability.requested` by the payload's capability name.
    #[serde(default)]
    pub provides: Vec<ProvidedCapability>,
    /// Present when the plugin answers `model.requested`.
    pub model_provider: Option<ModelProvider>,
    /// Event types delivered beyond those implied by `provides` and
    /// `model_provider`. `*` subscribes to the whole stream.
    #[serde(default)]
    pub subscribes: Vec<String>,
    /// Event types this plugin may propose.
    #[serde(default)]
    pub emits: Vec<String>,
    /// Own mutation types replayed before capability dispatch.
    #[serde(default)]
    pub rebuilds: Vec<String>,
    /// The instance keeps linear memory for one activity because its
    /// continuation is a suspended call stack.
    #[serde(default)]
    pub pinned_session: bool,
    pub config_pointer: String,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub requested_capabilities: Vec<RequestedCapability>,
}

impl ComponentManifest {
    /// Reports whether the plugin receives every event on the stream.
    #[must_use]
    pub fn subscribes_to_stream(&self) -> bool {
        self.subscribes.iter().any(|pattern| pattern == "*")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProvidedCapability {
    pub capability: String,
    pub description: String,
    pub arguments_schema: String,
    pub result_schema: String,
    pub constraints_schema: Option<String>,
    pub idempotency: Idempotency,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum Idempotency {
    Inherent,
    Keyed,
    NonIdempotent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelProvider {
    /// JSON Pointer into validated configuration, resolving to the array of
    /// model identifiers this instance serves.
    pub models_pointer: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CredentialDeclaration {
    pub components: Vec<String>,
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub config_pointer: String,
    pub input_schema: String,
    pub flow_schema: String,
    pub flow: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RequestedCapability {
    pub name: String,
    pub required: bool,
    pub reason: String,
    pub constraints: Value,
}
