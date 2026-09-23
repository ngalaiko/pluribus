use pluribus_core::{
    CapabilityName, ConstraintSet, Grant, HttpGrant, PrincipalKind, PrincipalRef, StartTls,
    StreamEndpoint, StreamGrant,
};
use pluribus_runtime_wasm::RuntimeLimits;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use crate::MAX_BLOB_BYTES;

#[derive(Clone, Debug, Serialize)]
pub struct PluginInstance {
    pub package: crate::package_source::PackageSource,
    #[serde(skip)]
    pub config: Value,
    #[serde(rename = "config", skip_serializing_if = "empty_object")]
    pub config_overrides: Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(skip)]
    pub components: BTreeMap<String, ComponentAccess>,
    #[serde(rename = "components", skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access: Option<Value>,
}

fn empty_object(value: &Value) -> bool {
    value.as_object().is_some_and(serde_json::Map::is_empty)
}
fn object() -> Value {
    json!({})
}

impl<'de> Deserialize<'de> for PluginInstance {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            package: crate::package_source::PackageSource,
            #[serde(default = "object")]
            config: Value,
            #[serde(default)]
            aliases: Vec<String>,
            #[serde(default)]
            components: BTreeMap<String, Value>,
            #[serde(default)]
            access: Option<Value>,
        }
        let input = Input::deserialize(deserializer)?;
        let components = input
            .components
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    serde_json::from_value(value.clone()).unwrap_or_default(),
                )
            })
            .collect();
        Ok(Self {
            package: input.package,
            config: input.config.clone(),
            config_overrides: input.config,
            aliases: input.aliases,
            components,
            overrides: input.components,
            access: input.access,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentAccess {
    #[serde(default)]
    pub http: HttpAccess,
    /// Endpoints this component may reach, by the name `socket.connect`
    /// selects them with. A component asking for one endpoint gets one entry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stream: BTreeMap<String, StreamAccess>,
    #[serde(default)]
    pub limits: Option<InstanceLimits>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceLimits {
    #[serde(default = "default_memory_bytes")]
    pub memory_bytes: usize,
    #[serde(default = "default_call_timeout_ms")]
    pub call_timeout_ms: u64,
}

fn default_memory_bytes() -> usize {
    RuntimeLimits::default().memory_bytes
}

fn default_call_timeout_ms() -> u64 {
    u64::try_from(RuntimeLimits::default().call_timeout.as_millis()).unwrap_or(u64::MAX)
}

impl InstanceLimits {
    pub fn runtime(self) -> RuntimeLimits {
        RuntimeLimits {
            memory_bytes: self.memory_bytes,
            call_timeout: Duration::from_millis(self.call_timeout_ms),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamAccess {
    /// A local endpoint: absolute socket path, plus the accounts allowed to
    /// answer it. Mutually exclusive with `tls`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
    /// Accounts allowed to answer the socket. See `StreamEndpoint`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peer_uids: Vec<u32>,
    /// A remote endpoint, authenticated by its certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsAccess>,
    #[serde(default = "default_stream_bytes")]
    pub max_bytes: u64,
    #[serde(default = "default_stream_timeout_ms")]
    pub max_timeout_ms: u32,
    /// Connections one instance may hold open at once. Unbounded unless the
    /// grant sets a ceiling: a local endpoint's concurrency is the plugin's
    /// business, and capping it by default would throttle existing ones.
    #[serde(default = "default_stream_connections")]
    pub max_connections: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsAccess {
    pub hostname: String,
    pub port: u16,
    /// Permits a loopback or private-network endpoint, as the HTTP grant does.
    #[serde(default)]
    pub allow_private_network: bool,
    /// Names a plaintext preamble the host speaks before the handshake, for
    /// an endpoint that upgrades rather than answering in TLS. The guest is
    /// handed an encrypted connection either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starttls: Option<StartTlsAccess>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StartTlsAccess {
    Smtp,
}

impl From<StartTlsAccess> for StartTls {
    fn from(value: StartTlsAccess) -> Self {
        match value {
            StartTlsAccess::Smtp => Self::Smtp,
        }
    }
}

fn default_stream_bytes() -> u64 {
    16 * 1024 * 1024
}

fn default_stream_timeout_ms() -> u32 {
    300_000
}

const fn default_stream_connections() -> u32 {
    u32::MAX
}

impl StreamAccess {
    /// # Errors
    /// Returns an error unless exactly one endpoint kind is configured.
    pub fn grant(&self) -> Result<StreamGrant, String> {
        let endpoint = match (&self.socket, &self.tls) {
            (Some(path), None) => StreamEndpoint::Unix {
                path: path.clone(),
                peer_uids: self.peer_uids.clone(),
            },
            (None, Some(tls)) => {
                if !self.peer_uids.is_empty() {
                    return Err("peer_uids belongs to a socket endpoint, not a TLS one".into());
                }
                StreamEndpoint::Tls {
                    hostname: tls.hostname.clone(),
                    port: tls.port,
                    allow_private_network: tls.allow_private_network,
                    starttls: tls.starttls.map(Into::into),
                }
            }
            (None, None) => return Err("stream access configures no endpoint".into()),
            (Some(_), Some(_)) => {
                return Err("stream access configures both a socket and a TLS endpoint".into());
            }
        };
        Ok(StreamGrant {
            endpoint,
            max_bytes: self.max_bytes,
            max_timeout_ms: self.max_timeout_ms,
            max_connections: self.max_connections,
        })
    }

    /// Whether this endpoint is a local socket, which needs a private runtime
    /// directory the transport checks before connecting.
    #[must_use]
    pub const fn is_local(&self) -> bool {
        self.socket.is_some()
    }

    /// # Errors
    /// Returns an error for a malformed endpoint or a limit of zero.
    pub fn validate(&self) -> Result<(), String> {
        let grant = self.grant()?;
        match &grant.endpoint {
            StreamEndpoint::Unix { path, peer_uids } => {
                if !path.is_absolute() || peer_uids.is_empty() || peer_uids.contains(&0) {
                    return Err(
                        "endpoint requires an absolute socket and non-root peer UIDs".into(),
                    );
                }
            }
            StreamEndpoint::Tls { hostname, port, .. } => {
                if *port == 0 || hostname.is_empty() || hostname.parse::<std::net::IpAddr>().is_ok()
                {
                    return Err("TLS endpoint requires a hostname and a port".into());
                }
            }
        }
        if self.max_bytes == 0
            || self.max_connections == 0
            || self.max_timeout_ms == 0
            || self.max_timeout_ms > 300_000
        {
            return Err("endpoint requires positive limits within the ceiling".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpAccess {
    pub origins: Vec<String>,
    pub methods: Vec<String>,
    pub max_request_bytes: u64,
    pub max_response_bytes: u64,
    pub max_timeout_ms: u32,
}

impl Default for HttpAccess {
    fn default() -> Self {
        Self {
            origins: Vec::new(),
            methods: Vec::new(),
            max_request_bytes: 1024 * 1024,
            max_response_bytes: 1024 * 1024,
            max_timeout_ms: 30_000,
        }
    }
}

impl HttpAccess {
    pub fn grant(&self, instance: &str) -> HttpGrant {
        HttpGrant {
            component: PrincipalRef::new(PrincipalKind::Component, instance),
            origins: self.origins.clone(),
            methods: self.methods.clone(),
            allow_http: false,
            allow_private_network: false,
            max_request_bytes: self.max_request_bytes,
            max_response_bytes: self.max_response_bytes,
            max_redirects: 0,
            max_timeout_ms: self.max_timeout_ms,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub agent_id: String,
    pub identity: String,
    pub model: String,
    pub maximum_blob_bytes: u64,
    pub plugin_instances: BTreeMap<String, PluginInstance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_instance: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capability_instances: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub trusted_capabilities: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub trusted_constraints: BTreeMap<String, BTreeMap<String, Value>>,
}

impl Config {
    pub fn instance(&self, name: &str) -> Result<(&str, &PluginInstance), String> {
        self.plugin_instances
            .iter()
            .find(|(id, instance)| {
                id.as_str() == name || instance.aliases.iter().any(|alias| alias == name)
            })
            .map(|(id, instance)| (id.as_str(), instance))
            .ok_or_else(|| {
                if self.plugin_instances.is_empty() {
                    "no plugin instances are configured".to_owned()
                } else {
                    format!(
                        "unknown plugin instance {name}; configured: {}",
                        self.plugin_instances
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            })
    }

    pub fn component(
        &self,
        name: &str,
    ) -> Result<(String, &PluginInstance, &ComponentAccess), String> {
        let (package, component) = name.split_once('/').unwrap_or((name, ""));
        let (id, instance) = self.instance(package)?;
        let component = if component.is_empty() && instance.components.len() == 1 {
            instance.components.keys().next().unwrap().as_str()
        } else {
            component
        };
        let access = instance
            .components
            .get(component)
            .ok_or_else(|| format!("unknown component: {name}"))?;
        Ok((
            pluribus_plugin_package::component_id(id, component),
            instance,
            access,
        ))
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut names = BTreeSet::new();
        for (id, instance) in &self.plugin_instances {
            for name in std::iter::once(id).chain(&instance.aliases) {
                if name.is_empty()
                    || name.contains('/')
                    || name.chars().any(char::is_whitespace)
                    || !names.insert(name)
                {
                    return Err(
                        "plugin instance IDs and aliases must be nonempty and unique".into(),
                    );
                }
            }
            instance
                .package
                .validate()
                .map_err(|error| format!("instance {id}: {error}"))?;
            if !instance.config.is_object() {
                return Err(format!(
                    "instance {id} requires a valid package source and object configuration"
                ));
            }
            for (component, access) in &instance.components {
                if component.contains('/') || component.chars().any(char::is_whitespace) {
                    return Err(format!("invalid component name: {component}"));
                }
                if access.http.max_request_bytes == 0
                    || access.http.max_response_bytes == 0
                    || access.http.max_timeout_ms == 0
                    || access.http.max_timeout_ms > 300_000
                {
                    return Err(format!("invalid HTTP limits for instance {id}"));
                }
                for (endpoint, stream) in &access.stream {
                    if endpoint.is_empty() || endpoint.chars().any(char::is_whitespace) {
                        return Err(format!("invalid endpoint name: {endpoint}"));
                    }
                    stream
                        .validate()
                        .map_err(|error| format!("instance {id} endpoint {endpoint}: {error}"))?;
                }
            }
        }
        if let Some(selector) = &self.model_instance {
            self.selector(selector)?;
        }
        let mut selected = BTreeSet::new();
        for name in &self.capability_instances {
            let id = self.selector(name)?;
            if !selected.insert(id.clone()) {
                return Err(format!("duplicate capability instance: {id}"));
            }
        }
        let mut granted = BTreeSet::new();
        for (name, capabilities) in &self.trusted_capabilities {
            let id = self.selector(name)?;
            if !granted.insert(id.clone()) {
                return Err(format!(
                    "grant requires a unique selected capability instance: {id}"
                ));
            }
            let mut names = BTreeSet::new();
            if capabilities.iter().any(|name| {
                name.is_empty() || name.chars().any(char::is_whitespace) || !names.insert(name)
            }) {
                return Err(format!("invalid capability grant for instance {id}"));
            }
        }
        for (instance, constraints) in &self.trusted_constraints {
            let capabilities = self
                .trusted_capabilities
                .get(instance)
                .ok_or_else(|| format!("constraints require grants for {instance}"))?;
            if constraints
                .iter()
                .any(|(name, value)| !capabilities.contains(name) || !value.is_object())
            {
                return Err(format!(
                    "constraints require granted capabilities for {instance}"
                ));
            }
        }
        Ok(())
    }

    fn selector(&self, name: &str) -> Result<String, String> {
        let (package, component) = name.split_once('/').unwrap_or((name, ""));
        let (id, _) = self.instance(package)?;
        if component.contains('/')
            || component.chars().any(char::is_whitespace)
            || name.ends_with('/')
        {
            return Err(format!("invalid component selector: {name}"));
        }
        Ok(pluribus_plugin_package::component_id(id, component))
    }

    pub fn validate_resolved(&self) -> Result<(), String> {
        self.validate()?;
        for name in self
            .model_instance
            .iter()
            .chain(&self.capability_instances)
            .chain(self.trusted_capabilities.keys())
        {
            self.component(name)?;
        }
        let mut selected = BTreeSet::new();
        for name in &self.capability_instances {
            let (id, _, _) = self.component(name)?;
            if !selected.insert(id) {
                return Err(format!("duplicate capability instance: {name}"));
            }
        }
        let mut granted = BTreeSet::new();
        for name in self.trusted_capabilities.keys() {
            let (id, _, _) = self.component(name)?;
            if !selected.contains(&id) || !granted.insert(id) {
                return Err(format!(
                    "grant requires a unique selected capability instance: {name}"
                ));
            }
        }
        Ok(())
    }

    pub fn trusted_grants(&self) -> Result<BTreeMap<CapabilityName, Vec<Grant>>, String> {
        let mut grants: BTreeMap<CapabilityName, Vec<Grant>> = BTreeMap::new();
        for (name, capabilities) in &self.trusted_capabilities {
            let (id, _, _) = self.component(name)?;
            let constraints = self.trusted_constraints.get(name);
            for name in capabilities {
                let capability = CapabilityName::new(name);
                grants.entry(capability.clone()).or_default().push(Grant {
                    capability,
                    provider: Some(PrincipalRef::new(PrincipalKind::Component, id.clone())),
                    constraints: ConstraintSet::canonical_json(
                        serde_json::to_vec(
                            &constraints
                                .and_then(|c| c.get(name))
                                .cloned()
                                .unwrap_or_else(|| json!({})),
                        )
                        .map_err(|e| e.to_string())?,
                    ),
                });
            }
        }
        Ok(grants)
    }
}

impl Default for Config {
    fn default() -> Self {
        let model = "gpt-5.6-luna";
        Self {
            agent_id: "personal".into(),
            identity: "You are a persistent personal agent. Answer tersely. Answer the current inbound message through the yield tool reply field; the host delivers it. Never call Telegram tools to answer the current message. Use Telegram tools only for additional proactive messages or media.".into(),
            model: model.into(),
            maximum_blob_bytes: MAX_BLOB_BYTES,
            // An agent starts with no plugins. `pluribus init` prints what a
            // working configuration needs.
            plugin_instances: BTreeMap::new(),
            model_instance: None,
            capability_instances: Vec::new(),
            trusted_capabilities: BTreeMap::new(),
            trusted_constraints: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_selectors_resolve_aliases_without_sharing_grants() {
        let mut config = crate::fixtures::config();
        config
            .plugin_instances
            .get_mut("telegram-1")
            .unwrap()
            .components
            .get_mut("send")
            .unwrap()
            .http
            .origins
            .clear();
        let (receive, _, access) = config.component("telegram/receive").unwrap();
        assert_eq!(receive, "telegram-1/receive");
        assert_eq!(access.http.origins, ["https://api.telegram.org"]);
        let (send, _, access) = config.component("telegram/send").unwrap();
        assert_eq!(send, "telegram-1/send");
        assert!(access.http.origins.is_empty());
        assert!(config.component("telegram").is_err());
        assert!(config.component("telegram/missing").is_err());
    }

    fn stream(value: Value) -> StreamAccess {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn an_endpoint_is_either_local_or_remote_never_both_or_neither() {
        let tls = json!({"tls":{"hostname":"imap.mail.me.com","port":993}});
        let unix = json!({"socket":"/run/p/e.sock","peer_uids":[1001]});
        assert!(stream(tls.clone()).grant().is_ok());
        assert!(stream(unix.clone()).grant().is_ok());
        assert!(stream(json!({})).grant().is_err());
        let mut both = tls.clone();
        both["socket"] = json!("/run/p/e.sock");
        assert!(stream(both).grant().is_err());
        // peer UIDs mean nothing to a certificate, so they are not silently ignored.
        let mut confused = tls;
        confused["peer_uids"] = json!([1001]);
        assert!(stream(confused).grant().is_err());
    }

    #[test]
    fn a_tls_endpoint_needs_a_name_a_port_and_a_connection() {
        assert!(
            stream(json!({"tls":{"hostname":"imap.mail.me.com","port":993}}))
                .validate()
                .is_ok()
        );
        for endpoint in [
            json!({"tls":{"hostname":"imap.mail.me.com","port":0}}),
            json!({"tls":{"hostname":"","port":993}}),
            // An address cannot be verified against a certificate name.
            json!({"tls":{"hostname":"17.253.144.10","port":993}}),
            json!({"tls":{"hostname":"imap.mail.me.com","port":993},"max_connections":0}),
            json!({"tls":{"hostname":"imap.mail.me.com","port":993},"max_timeout_ms":300_001}),
        ] {
            assert!(
                stream(endpoint.clone()).validate().is_err(),
                "{endpoint} must be rejected"
            );
        }
    }

    #[test]
    fn a_local_endpoint_keeps_its_defaults_and_its_peer_rules() {
        let access = stream(json!({"socket":"/run/p/e.sock","peer_uids":[1001]}));
        assert_eq!(access.max_bytes, 16 * 1024 * 1024);
        // A local endpoint is not capped unless the grant says so.
        assert_eq!(access.max_connections, u32::MAX);
        assert!(access.is_local());
        assert!(
            stream(json!({"socket":"relative.sock","peer_uids":[1001]}))
                .validate()
                .is_err()
        );
        assert!(
            stream(json!({"socket":"/run/p/e.sock","peer_uids":[0]}))
                .validate()
                .is_err()
        );
        assert!(
            stream(json!({"socket":"/run/p/e.sock"}))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn package_access_fields_are_rejected() {
        let mut value = serde_json::to_value(crate::fixtures::config()).unwrap();
        value["plugin_instances"]["telegram-1"]["http"] = json!({});
        assert!(serde_json::from_value::<Config>(value).is_err());
    }

    #[test]
    fn default_identity_uses_yield_reply() {
        assert!(
            Config::default()
                .identity
                .contains("yield tool reply field")
        );
    }

    #[test]
    fn scoped_constraints_survive_config_and_grant_resolution() {
        let mut config = crate::fixtures::config();
        config.capability_instances.push("telegram/send".into());
        config
            .trusted_capabilities
            .insert("telegram/send".into(), vec!["memory.recall".into()]);
        config.trusted_constraints.insert(
            "telegram/send".into(),
            BTreeMap::from([("memory.recall".into(), json!({"scopes":["project:p"]}))]),
        );
        let config: Config = serde_json::from_value(serde_json::to_value(config).unwrap()).unwrap();
        config.validate().unwrap();
        let grants = config.trusted_grants().unwrap();
        let grant = &grants[&CapabilityName::new("memory.recall")][0];
        assert_eq!(
            grant.provider,
            Some(PrincipalRef::new(
                PrincipalKind::Component,
                "telegram-1/send"
            ))
        );
        assert_eq!(
            serde_json::from_slice::<Value>(grant.constraints.as_bytes()).unwrap(),
            json!({"scopes":["project:p"]})
        );
    }
    #[test]
    fn registry_roundtrips() {
        let config = Config::default();
        config.validate().unwrap();
        let serialized = serde_json::to_value(&config).unwrap();
        assert!(serialized.get("telegram_plugin").is_none());
        let roundtrip: Config = serde_json::from_value(serialized.clone()).unwrap();
        assert_eq!(serde_json::to_value(roundtrip).unwrap(), serialized);
    }

    #[test]
    fn a_new_configuration_installs_no_plugins() {
        let config = Config::default();
        assert!(config.plugin_instances.is_empty());
        assert!(config.model_instance.is_none());
        assert_eq!(
            config.instance("telegram").unwrap_err(),
            "no plugin instances are configured"
        );
    }

    #[test]
    fn ambiguous_names_and_legacy_fields_are_rejected() {
        for alias in ["telegram-1", "telegram", "", "two words"] {
            let mut config = crate::fixtures::config();
            config
                .plugin_instances
                .get_mut("codex-1")
                .unwrap()
                .aliases
                .push(alias.into());
            assert!(config.validate().is_err(), "{alias}");
        }
        let mut value = serde_json::to_value(crate::fixtures::config()).unwrap();
        value["telegram_plugin"] = json!("plugins/telegram");
        assert!(serde_json::from_value::<Config>(value).is_err());
    }

    #[test]
    fn omitted_grants_deny_network_access() {
        let instance: PluginInstance = serde_json::from_value(json!({
            "package": "file:///plugins/example", "config": {}, "components":{"main":{}}
        }))
        .unwrap();
        let grant = instance.components["main"].http.grant("example/main");
        assert!(grant.origins.is_empty());
        assert!(grant.methods.is_empty());
        assert!(!grant.allow_http && !grant.allow_private_network);
        assert_eq!(grant.max_redirects, 0);
        assert_eq!(
            grant.component,
            PrincipalRef::new(PrincipalKind::Component, "example/main")
        );
    }
}
