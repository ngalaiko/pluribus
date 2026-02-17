use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Truncate a string for display: collapse newlines to spaces, append `...`
/// if shortened, and ensure the cut is on a char boundary.
fn truncate(s: &str, max_len: usize) -> String {
    let collapsed: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if collapsed.len() <= max_len {
        collapsed
    } else {
        let mut end = max_len;
        while !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &collapsed[..end])
    }
}

/// Unique node identifier.
///
/// Format: `<schema>:<id>`, e.g. `tailscale:nKp3Z5CNTRL`.
/// Schema: `[a-z][a-z0-9]*`. Id: non-empty.
/// Lexicographic sort on the full string is well-defined.
///
/// Validated on construction — accessors are infallible.
#[derive(Debug, Clone)]
pub struct NodeId {
    schema: String,
    id: String,
}

impl NodeId {
    pub fn try_new(s: &str) -> Result<Self, InvalidNodeId> {
        let (schema, id) = s
            .split_once(':')
            .ok_or_else(|| InvalidNodeId(format!("missing ':' in \"{s}\"")))?;

        if schema.is_empty() || !is_valid_schema(schema) {
            return Err(InvalidNodeId(format!(
                "schema must match [a-z][a-z0-9]*, got \"{schema}\""
            )));
        }

        if id.is_empty() {
            return Err(InvalidNodeId("id must be non-empty".into()));
        }

        Ok(Self {
            schema: schema.to_owned(),
            id: id.to_owned(),
        })
    }

    /// Panicking constructor. Use when the value is known at compile time.
    ///
    /// # Panics
    ///
    /// Panics if `s` is not a valid `<schema>:<id>` string.
    #[must_use]
    pub fn new(s: &str) -> Self {
        Self::try_new(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

// Ord/Eq/Hash on the wire representation so sort order matches the spec.

impl PartialEq for NodeId {
    fn eq(&self, other: &Self) -> bool {
        self.schema == other.schema && self.id == other.id
    }
}

impl Eq for NodeId {}

impl PartialOrd for NodeId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NodeId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.schema
            .cmp(&other.schema)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl std::hash::Hash for NodeId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.schema.hash(state);
        self.id.hash(state);
    }
}

impl TryFrom<&str> for NodeId {
    type Error = InvalidNodeId;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::try_new(s)
    }
}

impl TryFrom<String> for NodeId {
    type Error = InvalidNodeId;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::try_new(&s)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.schema, self.id)
    }
}

impl Serialize for NodeId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::try_new(&s).map_err(serde::de::Error::custom)
    }
}

/// Error returned when parsing an invalid [`NodeId`].
#[derive(Debug, Clone)]
pub struct InvalidNodeId(String);

impl fmt::Display for InvalidNodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid node ID: {}", self.0)
    }
}

impl std::error::Error for InvalidNodeId {}

fn is_valid_schema(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Describes a node on the network.
///
/// Exchanged as the join payload during the Loro sync handshake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: NodeId,
    pub name: String,
    pub tools: Vec<ToolDef>,
}

impl fmt::Display for Manifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({}, {} tools)", self.name, self.id, self.tools.len())
    }
}

/// A tool call requested by an LLM assistant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl fmt::Display for ToolCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.name, truncate(&self.arguments, 60))
    }
}

/// Helper for `#[serde(skip_serializing_if)]` on bool fields.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(v: &bool) -> bool {
    !*v
}

/// A content block within a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { mime_type: String, data: String },
}

impl ContentPart {
    /// Create a text content part.
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    /// Create an image content part from base64-encoded data.
    #[must_use]
    pub fn image(mime_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image {
            mime_type: mime_type.into(),
            data: data.into(),
        }
    }

    /// Return the text content, if this is a text part.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            Self::Image { .. } => None,
        }
    }
}

/// A message in an LLM conversation.
///
/// Pure LLM-native type — no distributed metadata.
/// Serialized as a flat map with a `"role"` tag discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "system")]
    System { content: Vec<ContentPart> },

    #[serde(rename = "scheduled")]
    Scheduled { content: Vec<ContentPart> },

    #[serde(rename = "user")]
    User { content: Vec<ContentPart> },

    #[serde(rename = "assistant")]
    Assistant {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ContentPart>,
        #[serde(rename = "toolCalls", default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(
            rename = "reasoningContent",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        reasoning_content: Option<String>,
    },

    #[serde(rename = "tool")]
    Tool {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: Vec<ContentPart>,
        #[serde(rename = "isError", default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

impl Message {
    /// Create a system message from a text string.
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self::System {
            content: vec![ContentPart::text(text)],
        }
    }

    /// Create a scheduled message from a text string.
    #[must_use]
    pub fn scheduled(text: impl Into<String>) -> Self {
        Self::Scheduled {
            content: vec![ContentPart::text(text)],
        }
    }

    /// Create a user message from a text string.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentPart::text(text)],
        }
    }

    /// Create an assistant message from a text string.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![ContentPart::text(text)],
            tool_calls: Vec::new(),
            reasoning_content: None,
        }
    }

    /// Create an assistant message with content, tool calls, and optional reasoning.
    #[must_use]
    pub const fn assistant_with_tool_calls(
        content: Vec<ContentPart>,
        tool_calls: Vec<ToolCall>,
        reasoning_content: Option<String>,
    ) -> Self {
        Self::Assistant {
            content,
            tool_calls,
            reasoning_content,
        }
    }

    /// Create a tool result message.
    #[must_use]
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::Tool {
            tool_call_id: tool_call_id.into(),
            content: vec![ContentPart::text(content)],
            is_error,
        }
    }

    /// Concatenate all text parts into a single string.
    #[must_use]
    pub fn text(&self) -> String {
        let parts = match self {
            Self::System { content }
            | Self::Scheduled { content }
            | Self::User { content }
            | Self::Assistant { content, .. }
            | Self::Tool { content, .. } => content,
        };
        parts
            .iter()
            .filter_map(ContentPart::as_text)
            .collect::<Vec<_>>()
            .join("")
    }

    /// Return the reasoning content on this message (`None` for non-assistant).
    #[must_use]
    pub fn reasoning_content(&self) -> Option<&str> {
        if let Self::Assistant {
            reasoning_content, ..
        } = self
        {
            reasoning_content.as_deref()
        } else {
            None
        }
    }

    /// Return a clone of this message with `reasoning_content` cleared.
    #[must_use]
    pub fn without_reasoning(&self) -> Self {
        match self {
            Self::Assistant {
                content,
                tool_calls,
                ..
            } => Self::Assistant {
                content: content.clone(),
                tool_calls: tool_calls.clone(),
                reasoning_content: None,
            },
            other => other.clone(),
        }
    }

    /// Return the tool calls on this message (empty slice for non-assistant).
    #[must_use]
    pub fn tool_calls(&self) -> &[ToolCall] {
        if let Self::Assistant { tool_calls, .. } = self {
            tool_calls
        } else {
            &[]
        }
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MAX: usize = 60;
        match self {
            Self::System { .. } => {
                write!(f, "[system] {}", truncate(&self.text(), MAX))
            }
            Self::Scheduled { .. } => {
                write!(f, "[scheduled] {}", truncate(&self.text(), MAX))
            }
            Self::User { content } => {
                let has_image = content
                    .iter()
                    .any(|p| matches!(p, ContentPart::Image { .. }));
                let text = self.text();
                if has_image && text.is_empty() {
                    write!(f, "[user] [image]")
                } else if has_image {
                    write!(f, "[user] [image] {}", truncate(&text, MAX))
                } else {
                    write!(f, "[user] {}", truncate(&text, MAX))
                }
            }
            Self::Assistant {
                tool_calls,
                reasoning_content,
                ..
            } => {
                let text = self.text();
                let thinking = if reasoning_content.is_some() {
                    "[thinking] "
                } else {
                    ""
                };
                if tool_calls.is_empty() {
                    write!(f, "[assistant] {thinking}{}", truncate(&text, MAX))
                } else {
                    let names: Vec<&str> = tool_calls.iter().map(|c| c.name.as_str()).collect();
                    if text.is_empty() {
                        write!(f, "[assistant] {thinking}tools=[{}]", names.join(", "))
                    } else {
                        write!(
                            f,
                            "[assistant] {thinking}{} tools=[{}]",
                            truncate(&text, MAX),
                            names.join(", ")
                        )
                    }
                }
            }
            Self::Tool {
                tool_call_id,
                is_error,
                ..
            } => {
                let label = if *is_error { "tool:err" } else { "tool:ok" };
                write!(
                    f,
                    "[{label}] call={} {}",
                    tool_call_id,
                    truncate(&self.text(), MAX)
                )
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToolName(String);

impl ToolName {
    /// Validates and constructs a `ToolName`.
    ///
    /// Pattern: `^[a-zA-Z0-9_-]+$` (non-empty, alphanumeric plus underscore/hyphen).
    pub fn try_new(s: impl Into<String>) -> Result<Self, InvalidToolName> {
        let s = s.into();
        if s.is_empty()
            || !s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(InvalidToolName(format!(
                "must match [a-zA-Z0-9_-]+, got \"{s}\""
            )));
        }
        Ok(Self(s))
    }

    /// Panicking constructor. Use when the value is known at compile time.
    ///
    /// # Panics
    ///
    /// Panics if `s` does not match `^[a-zA-Z0-9_-]+$`.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self::try_new(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ToolName {
    type Error = InvalidToolName;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::try_new(s)
    }
}

impl TryFrom<String> for ToolName {
    type Error = InvalidToolName;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::try_new(s)
    }
}

impl AsRef<str> for ToolName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ToolName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

/// Error returned when parsing an invalid [`ToolName`].
#[derive(Debug, Clone)]
pub struct InvalidToolName(String);

impl fmt::Display for InvalidToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid tool name: {}", self.0)
    }
}

impl std::error::Error for InvalidToolName {}

/// A tool definition exchanged in the node manifest.
///
/// Nodes advertise their capabilities as a list of `ToolDef`s.
/// Routing uses `name` to match requests to capable nodes.
/// The `description` and `input_schema` are passed to the LLM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    name: ToolName,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_schema: Option<schemars::Schema>,
}

impl ToolDef {
    /// Create a new `ToolDef` with no input schema.
    #[must_use]
    pub fn new(name: ToolName, description: impl Into<String>) -> Self {
        Self {
            name,
            description: description.into(),
            input_schema: None,
        }
    }

    /// Derive and set the input schema from `I`.
    #[must_use]
    pub fn with_input_schema<I: JsonSchema>(mut self) -> Self {
        self.input_schema = Some(schemars::schema_for!(I));
        self
    }

    /// The tool name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Human-readable description for the LLM.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// JSON Schema describing the tool's input, if any.
    #[must_use]
    pub const fn input_schema(&self) -> Option<&schemars::Schema> {
        self.input_schema.as_ref()
    }
}
