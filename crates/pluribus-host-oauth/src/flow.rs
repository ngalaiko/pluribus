use super::{
    Clock, DeviceCodePrompt, DeviceCodeStatus, OAuthError, OAuthHttpRequest, OAuthHttpResponse,
    OAuthTransport, account_id_claim, add_duration, require_success,
};
use jsonschema::Validator;
use pluribus_core::{
    HttpCredential, OAuthCredential, OAuthCredentialStore, PrincipalKind, PrincipalRef,
    SecretHandle, SecretHeader, SecretPathPrefix,
};
use serde::{Deserialize, Serialize};

#[path = "recipe.rs"]
mod recipe;
pub(crate) use recipe::StoredRecipe;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use url::Url;
use url::form_urlencoded::Serializer;

const STATIC_SCHEMA: &str = include_str!("../../../schemas/credential-static-http-1.schema.json");
const DEVICE_SCHEMA: &str = include_str!("../../../schemas/credential-oauth-device-1.schema.json");
const MAX_TEMPLATE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnrollmentPolicy {
    pub components: Vec<PrincipalRef>,
    pub enrollment_origins: Vec<String>,
    pub injection_origins: Vec<String>,
}

pub struct CredentialEnrollment {
    store: Arc<dyn OAuthCredentialStore>,
    transport: Arc<dyn OAuthTransport>,
    clock: Arc<dyn Clock>,
}

impl CredentialEnrollment {
    #[must_use]
    pub fn new(
        store: Arc<dyn OAuthCredentialStore>,
        transport: Arc<dyn OAuthTransport>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            transport,
            clock,
        }
    }

    /// Validates a static declaration and seals its rendered HTTP credential.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, policy, templates, or storage.
    pub async fn enroll_static(
        &self,
        input_schema: &Value,
        flow: &Value,
        input: &Value,
        handle: &SecretHandle,
        policy: &EnrollmentPolicy,
    ) -> Result<(), OAuthError> {
        validate_json(STATIC_SCHEMA, flow, "static credential flow")?;
        validate_input(input_schema, input)?;
        let flow: StaticFlow = serde_json::from_value(flow.clone()).map_err(|_| {
            OAuthError::InvalidRequest("static credential flow is malformed".into())
        })?;
        let http = render_http(&flow.http, input, &BTreeMap::new(), None, policy)?;
        self.store
            .put_http(handle, &http)
            .await
            .map_err(|_| OAuthError::Storage)
    }

    /// Adopts a declared recipe for a legacy credential without rotating its tokens.
    ///
    /// # Errors
    ///
    /// Returns an error for changed scope, insufficient identity, or concurrent mutation.
    pub async fn adopt_device_recipe(
        &self,
        input_schema: &Value,
        flow: &Value,
        input: &Value,
        handle: &SecretHandle,
        policy: &EnrollmentPolicy,
    ) -> Result<(), OAuthError> {
        Self::validate_device_flow("pluribus:credential/oauth-device@1", flow)?;
        validate_input(input_schema, input)?;
        let flow: DeviceFlow = serde_json::from_value(flow.clone())
            .map_err(|_| OAuthError::InvalidRequest("device flow is malformed".into()))?;
        validate_device_policy(&flow, policy)?;
        StoredRecipe::validate(&flow, input, policy)?;
        for component in &policy.components {
            for origin in &flow.http.origins {
                self.store
                    .resolve_http(handle, component, origin)
                    .await
                    .map_err(|_| OAuthError::AuthorizationDenied)?;
            }
        }
        let snapshot = self
            .store
            .load_oauth_snapshot(handle)
            .await
            .map_err(|_| OAuthError::Storage)?;
        let mut credential = snapshot.credential;
        let access = credential
            .access_token
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .or_else(|| {
                credential
                    .http
                    .headers
                    .iter()
                    .find(|header| header.name.eq_ignore_ascii_case("authorization"))
                    .and_then(|header| std::str::from_utf8(&header.value).ok())
                    .and_then(|value| value.strip_prefix("Bearer "))
            })
            .ok_or(OAuthError::AuthorizationDenied)?;
        let recipe = StoredRecipe::enroll(&flow, input, policy, access)?;
        let rendered = render_http(&flow.http, input, &BTreeMap::new(), Some(access), policy)?;
        if credential.provider != flow.provider
            || credential.token_url != recipe.endpoint()
            || credential.http != rendered
        {
            return Err(OAuthError::AuthorizationDenied);
        }
        let request = recipe.request(&credential, super::REQUEST_TIMEOUT)?;
        let client_id = if request.content_type == "application/x-www-form-urlencoded" {
            url::form_urlencoded::parse(&request.body)
                .find(|(name, _)| name == "client_id")
                .map(|(_, value)| value.into_owned())
        } else {
            serde_json::from_slice::<Value>(&request.body)
                .ok()
                .and_then(|value| {
                    value
                        .get("client_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
        };
        if credential.refresh_recipe.is_none()
            && client_id.as_deref() != Some(&credential.client_id)
        {
            return Err(OAuthError::AuthorizationDenied);
        }
        let encoded = recipe.encode()?;
        if credential
            .refresh_recipe
            .as_ref()
            .is_some_and(|stored| stored != &encoded)
        {
            return Err(OAuthError::AuthorizationDenied);
        }
        credential.access_token = Some(access.as_bytes().to_vec());
        credential.refresh_recipe = Some(encoded);
        if !self
            .store
            .adopt_oauth_recipe(
                handle,
                snapshot.generation,
                &credential,
                self.clock.now_ms(),
            )
            .await
            .map_err(|_| OAuthError::Storage)?
        {
            return Err(OAuthError::AuthorizationDenied);
        }
        Ok(())
    }

    /// Validates the exact schema advertised by an installed package.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported schemas or invalid declarations.
    pub fn validate_device_flow(schema: &str, flow: &Value) -> Result<(), OAuthError> {
        if schema != "pluribus:credential/oauth-device@1" {
            return Err(OAuthError::InvalidRequest(
                "unsupported device flow schema".into(),
            ));
        }
        validate_json(DEVICE_SCHEMA, flow, "OAuth device flow")
    }

    /// Starts a declared OAuth device flow.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, policy, templates, transport, or provider output.
    pub async fn begin_device(
        &self,
        input_schema: &Value,
        flow: &Value,
        input: &Value,
        handle: SecretHandle,
        policy: EnrollmentPolicy,
    ) -> Result<DeviceEnrollmentSession, OAuthError> {
        validate_json(DEVICE_SCHEMA, flow, "OAuth device flow")?;
        validate_input(input_schema, input)?;
        let flow: DeviceFlow = serde_json::from_value(flow.clone())
            .map_err(|_| OAuthError::InvalidRequest("OAuth device flow is malformed".into()))?;
        validate_device_policy(&flow, &policy)?;
        StoredRecipe::validate(&flow, input, &policy)?;
        let responses = BTreeMap::new();
        let response = self.send(&flow.start, input, &responses).await?;
        require_success(&response, "device code request")?;
        let start = response_json(&response, "device code response")?;
        let mut responses = BTreeMap::new();
        responses.insert(Step::Start, start);
        let interval = number_source(&flow.prompt.interval_seconds, &responses);
        let expires = number_source(&flow.prompt.expires_in_seconds, &responses);
        let prompt = DeviceCodePrompt {
            verification_url: render(&flow.prompt.verification_uri, input, &responses, None)?,
            user_code: render(&flow.prompt.user_code, input, &responses, None)?,
            interval: Duration::from_secs(interval),
            expires_at_ms: add_duration(self.clock.now_ms(), Duration::from_secs(expires))?,
        };
        Ok(DeviceEnrollmentSession {
            flow,
            input: input.clone(),
            responses,
            prompt,
            handle,
            policy,
        })
    }

    /// Polls once and seals the credential after authorization.
    ///
    /// # Errors
    ///
    /// Returns an error for expiry, denial, malformed output, policy, transport, or storage.
    pub async fn poll_device(
        &self,
        session: &mut DeviceEnrollmentSession,
    ) -> Result<DeviceCodeStatus, OAuthError> {
        if self.clock.now_ms() >= session.prompt.expires_at_ms {
            return Err(OAuthError::Expired);
        }
        let response = self
            .send(
                &session.flow.poll.request,
                &session.input,
                &session.responses,
            )
            .await?;
        if session
            .flow
            .poll
            .pending_statuses
            .contains(&response.status)
        {
            return Ok(DeviceCodeStatus::Pending);
        }
        if !(200..300).contains(&response.status) {
            return classify_poll_error(&session.flow.poll, &response);
        }
        let poll = response_json(&response, "device polling response")?;
        session.responses.insert(Step::Poll, poll.clone());
        let token_response = if let Some(exchange) = &session.flow.exchange {
            let response = self
                .send(exchange, &session.input, &session.responses)
                .await?;
            require_success(&response, "token exchange")?;
            response_json(&response, "token response")?
        } else {
            poll
        };
        let mut token =
            parse_declared_token(&session.flow.token, &token_response, self.clock.now_ms())?;
        let recipe = StoredRecipe::enroll(
            &session.flow,
            &session.input,
            &session.policy,
            &token.access_token,
        )?;
        token.expires_at_ms =
            recipe.expiry(&token_response, &token.access_token, self.clock.now_ms())?;
        let http = render_http(
            &session.flow.http,
            &session.input,
            &session.responses,
            Some(&token.access_token),
            &session.policy,
        )?;
        self.store
            .put_oauth(
                &session.handle,
                &OAuthCredential {
                    provider: session.flow.provider.clone(),
                    http,
                    refresh_token: token.refresh_token.into_bytes(),
                    expires_at_ms: token.expires_at_ms,
                    token_url: recipe.endpoint().to_owned(),
                    client_id: "declared".into(),
                    refresh_recipe: Some(recipe.encode()?),
                    access_token: Some(token.access_token.into_bytes()),
                },
            )
            .await
            .map_err(|_| OAuthError::Storage)?;
        Ok(DeviceCodeStatus::Authorized)
    }

    async fn send(
        &self,
        request: &RequestSpec,
        input: &Value,
        responses: &BTreeMap<Step, Value>,
    ) -> Result<OAuthHttpResponse, OAuthError> {
        let values = request
            .body
            .fields
            .iter()
            .map(|field| {
                render(&field.value, input, responses, None)
                    .map(|value| (field.name.as_str(), value))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (content_type, body) = match request.body.encoding {
            Encoding::Json => {
                let object = values
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), Value::String(value)))
                    .collect::<Map<_, _>>();
                (
                    "application/json",
                    serde_json::to_vec(&Value::Object(object))
                        .map_err(|_| OAuthError::InvalidRequest("cannot encode request".into()))?,
                )
            }
            Encoding::Form => {
                let mut serializer = Serializer::new(String::new());
                for (name, value) in values {
                    serializer.append_pair(name, &value);
                }
                (
                    "application/x-www-form-urlencoded",
                    serializer.finish().into_bytes(),
                )
            }
        };
        self.transport
            .post(&OAuthHttpRequest {
                url: request.url.clone(),
                content_type,
                body,
                headers: request
                    .headers
                    .iter()
                    .map(|header| (header.name.clone(), header.value.clone()))
                    .collect(),
                timeout: super::REQUEST_TIMEOUT,
            })
            .await
    }
}

pub struct DeviceEnrollmentSession {
    flow: DeviceFlow,
    input: Value,
    responses: BTreeMap<Step, Value>,
    prompt: DeviceCodePrompt,
    handle: SecretHandle,
    policy: EnrollmentPolicy,
}

impl DeviceEnrollmentSession {
    #[must_use]
    pub fn prompt(&self) -> &DeviceCodePrompt {
        &self.prompt
    }
}

impl fmt::Debug for DeviceEnrollmentSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceEnrollmentSession")
            .field("provider", &self.flow.provider)
            .field("prompt", &self.prompt)
            .field("handle", &self.handle)
            .field("components", &self.policy.components)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StaticFlow {
    http: HttpSpec,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceFlow {
    provider: String,
    start: RequestSpec,
    prompt: PromptSpec,
    poll: PollSpec,
    exchange: Option<RequestSpec>,
    token: TokenSpec,
    refresh: Value,
    http: HttpSpec,
    expiry: Option<recipe::Expiry>,
    #[serde(default)]
    auth_failure: Vec<recipe::AuthFailure>,
    #[serde(default)]
    replay: Vec<recipe::Replay>,
    account_pointer: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PromptSpec {
    verification_uri: Template,
    user_code: Template,
    interval_seconds: NumberSource,
    expires_in_seconds: NumberSource,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PollSpec {
    request: RequestSpec,
    #[serde(default)]
    pending_statuses: Vec<u16>,
    error_pointer: Option<String>,
    #[serde(default)]
    pending_errors: Vec<String>,
    #[serde(default)]
    slow_down_errors: Vec<String>,
    #[serde(default)]
    denied_errors: Vec<String>,
    #[serde(default)]
    expired_errors: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TokenSpec {
    #[serde(rename = "accessTokenPointer")]
    access_token: String,
    #[serde(rename = "refreshTokenPointer")]
    refresh_token: String,
    #[serde(rename = "expiresInPointer")]
    expires_in: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RefreshSpec {
    url: String,
    client_id: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RequestSpec {
    #[serde(rename = "method")]
    _method: Method,
    url: String,
    body: BodySpec,
    #[serde(default)]
    headers: Vec<recipe::FixedHeader>,
}

#[derive(Clone, Deserialize, Serialize)]
enum Method {
    #[serde(rename = "POST")]
    Post,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BodySpec {
    encoding: Encoding,
    fields: Vec<FieldSpec>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Encoding {
    Json,
    Form,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FieldSpec {
    name: String,
    value: Template,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NumberSource {
    step: Option<Step>,
    pointer: Option<String>,
    default: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
enum Step {
    Start,
    Poll,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HttpSpec {
    origins: Vec<String>,
    #[serde(default)]
    headers: Vec<HeaderSpec>,
    path_prefix: Option<Template>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HeaderSpec {
    name: String,
    value: Template,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Template {
    parts: Vec<TemplatePart>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum TemplatePart {
    Literal(LiteralPart),
    Input(InputPart),
    Response(ResponsePart),
    AccessToken(AccessTokenPart),
    JwtClaim(JwtClaimPart),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LiteralPart {
    literal: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InputPart {
    input: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ResponsePart {
    response: ResponseRef,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccessTokenPart {
    access_token: Empty,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JwtClaimPart {
    jwt_claim: ClaimRef,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ClaimRef {
    pointer: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ResponseRef {
    step: Step,
    pointer: String,
}

struct ParsedToken {
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
}

fn validate_input(schema: &Value, input: &Value) -> Result<(), OAuthError> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|_| OAuthError::InvalidRequest("credential input schema is invalid".into()))?;
    validator
        .validate(input)
        .map_err(|_| OAuthError::InvalidRequest("credential input is invalid".into()))
}

fn validate_json(schema: &str, value: &Value, label: &str) -> Result<(), OAuthError> {
    let schema: Value = serde_json::from_str(schema)
        .map_err(|_| OAuthError::InvalidRequest("built-in credential schema is invalid".into()))?;
    let validator: Validator = jsonschema::validator_for(&schema)
        .map_err(|_| OAuthError::InvalidRequest("built-in credential schema is invalid".into()))?;
    validator
        .validate(value)
        .map_err(|_| OAuthError::InvalidRequest(format!("{label} is invalid")))
}

fn validate_consumers(policy: &EnrollmentPolicy) -> Result<(), OAuthError> {
    let mut seen = std::collections::HashSet::new();
    if policy.components.is_empty()
        || policy.components.iter().any(|component| {
            component.kind != PrincipalKind::Component
                || component.id.as_str().is_empty()
                || !seen.insert(component)
        })
    {
        return Err(OAuthError::AuthorizationDenied);
    }
    Ok(())
}

fn validate_device_policy(flow: &DeviceFlow, policy: &EnrollmentPolicy) -> Result<(), OAuthError> {
    validate_consumers(policy)?;
    for request in [&flow.start, &flow.poll.request]
        .into_iter()
        .chain(flow.exchange.iter())
    {
        require_allowed_url(&request.url, &policy.enrollment_origins)?;
    }
    require_allowed_url(
        flow.refresh
            .pointer("/request/url")
            .or_else(|| flow.refresh.get("url"))
            .and_then(Value::as_str)
            .ok_or_else(|| OAuthError::InvalidRequest("refresh endpoint is missing".into()))?,
        &policy.enrollment_origins,
    )?;
    for origin in &flow.http.origins {
        require_allowed_origin(origin, &policy.injection_origins)?;
    }
    if flow.expiry.is_none() {
        validate_bearer_injection(&flow.http)?;
    }
    Ok(())
}

fn validate_bearer_injection(http: &HttpSpec) -> Result<(), OAuthError> {
    let authorization = http
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("authorization"))
        .collect::<Vec<_>>();
    let valid = matches!(
        authorization.as_slice(),
        [header]
            if matches!(
                header.value.parts.as_slice(),
                [TemplatePart::Literal(prefix), TemplatePart::AccessToken(_)]
                    if prefix.literal == "Bearer "
            )
    );
    if valid {
        Ok(())
    } else {
        Err(OAuthError::InvalidRequest(
            "OAuth flow must inject one bearer authorization header".into(),
        ))
    }
}

fn require_allowed_url(url: &str, allowed: &[String]) -> Result<(), OAuthError> {
    let parsed = secure_url(url)?;
    let origin = parsed.origin().ascii_serialization();
    if allowed.iter().any(|allowed| allowed == &origin) {
        Ok(())
    } else {
        Err(OAuthError::InvalidRequest(
            "credential endpoint exceeds its grant".into(),
        ))
    }
}

fn require_allowed_origin(origin: &str, allowed: &[String]) -> Result<(), OAuthError> {
    let parsed = secure_url(origin)?;
    if parsed.path() != "/" || parsed.query().is_some() {
        return Err(OAuthError::InvalidRequest(
            "credential origin is malformed".into(),
        ));
    }
    let normalized = parsed.origin().ascii_serialization();
    if normalized != origin || !allowed.iter().any(|allowed| allowed == origin) {
        return Err(OAuthError::InvalidRequest(
            "credential injection exceeds its grant".into(),
        ));
    }
    Ok(())
}

fn secure_url(value: &str) -> Result<Url, OAuthError> {
    let url = Url::parse(value)
        .map_err(|_| OAuthError::InvalidRequest("credential URL is malformed".into()))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.port_or_known_default() != Some(443)
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(OAuthError::InvalidRequest(
            "credential URL is not permitted".into(),
        ));
    }
    Ok(url)
}

fn render_http(
    spec: &HttpSpec,
    input: &Value,
    responses: &BTreeMap<Step, Value>,
    access_token: Option<&str>,
    policy: &EnrollmentPolicy,
) -> Result<HttpCredential, OAuthError> {
    validate_consumers(policy)?;
    for origin in &spec.origins {
        require_allowed_origin(origin, &policy.injection_origins)?;
    }
    let headers = spec
        .headers
        .iter()
        .map(|header| {
            Ok(SecretHeader {
                name: header.name.clone(),
                value: render(&header.value, input, responses, access_token)?.into_bytes(),
            })
        })
        .collect::<Result<Vec<_>, OAuthError>>()?;
    let path_prefix = spec
        .path_prefix
        .as_ref()
        .map(|template| {
            render(template, input, responses, access_token)
                .map(|value| SecretPathPrefix::new(value.into_bytes()))
        })
        .transpose()?;
    Ok(HttpCredential {
        headers,
        path_prefix,
        allowed_origins: spec.origins.clone(),
        allowed_components: policy.components.clone(),
    })
}

fn render(
    template: &Template,
    input: &Value,
    responses: &BTreeMap<Step, Value>,
    access_token: Option<&str>,
) -> Result<String, OAuthError> {
    let mut result = String::new();
    for part in &template.parts {
        let value = match part {
            TemplatePart::Literal(part) => part.literal.clone(),
            TemplatePart::Input(part) => input
                .get(&part.input)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| OAuthError::InvalidRequest("credential input is missing".into()))?,
            TemplatePart::Response(part) => response_scalar(responses, &part.response)?,
            TemplatePart::AccessToken(part) => {
                let _ = &part.access_token;
                access_token
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| OAuthError::InvalidResponse("access token is missing".into()))?
            }
            TemplatePart::JwtClaim(part) => account_id_claim(
                access_token
                    .ok_or_else(|| OAuthError::InvalidResponse("access token is missing".into()))?,
                &part.jwt_claim.pointer,
            )?,
        };
        if result.len().saturating_add(value.len()) > MAX_TEMPLATE_BYTES {
            return Err(OAuthError::InvalidResponse(
                "credential template exceeds 64 KiB".into(),
            ));
        }
        result.push_str(&value);
    }
    Ok(result)
}

fn response_scalar(
    responses: &BTreeMap<Step, Value>,
    reference: &ResponseRef,
) -> Result<String, OAuthError> {
    let value = responses
        .get(&reference.step)
        .and_then(|value| value.pointer(&reference.pointer))
        .ok_or_else(|| OAuthError::InvalidResponse("declared response field is missing".into()))?;
    match value {
        Value::String(value) if !value.is_empty() => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        _ => Err(OAuthError::InvalidResponse(
            "declared response field is not scalar".into(),
        )),
    }
}

fn number_source(source: &NumberSource, responses: &BTreeMap<Step, Value>) -> u64 {
    let Some(step) = source.step else {
        return source.default;
    };
    let Some(pointer) = &source.pointer else {
        return source.default;
    };
    let value = responses
        .get(&step)
        .and_then(|value| value.pointer(pointer));
    let parsed = value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    });
    parsed.filter(|value| *value > 0).unwrap_or(source.default)
}

fn response_json(response: &OAuthHttpResponse, label: &str) -> Result<Value, OAuthError> {
    let value: Value = serde_json::from_slice(&response.body)
        .map_err(|_| OAuthError::InvalidResponse(format!("{label} is not JSON")))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(OAuthError::InvalidResponse(format!(
            "{label} is not an object"
        )))
    }
}

fn classify_poll_error(
    spec: &PollSpec,
    response: &OAuthHttpResponse,
) -> Result<DeviceCodeStatus, OAuthError> {
    let code = spec.error_pointer.as_deref().and_then(|pointer| {
        serde_json::from_slice::<Value>(&response.body)
            .ok()
            .and_then(|value| value.pointer(pointer).cloned())
            .and_then(|value| match value {
                Value::String(value) => Some(value),
                Value::Object(value) => {
                    value.get("code").and_then(Value::as_str).map(str::to_owned)
                }
                _ => None,
            })
    });
    if code
        .as_ref()
        .is_some_and(|code| spec.pending_errors.contains(code))
    {
        Ok(DeviceCodeStatus::Pending)
    } else if code
        .as_ref()
        .is_some_and(|code| spec.slow_down_errors.contains(code))
    {
        Ok(DeviceCodeStatus::SlowDown)
    } else if code
        .as_ref()
        .is_some_and(|code| spec.denied_errors.contains(code))
    {
        Err(OAuthError::AuthorizationDenied)
    } else if code
        .as_ref()
        .is_some_and(|code| spec.expired_errors.contains(code))
    {
        Err(OAuthError::Expired)
    } else {
        Err(OAuthError::Unavailable(format!(
            "device polling failed with status {}",
            response.status
        )))
    }
}

fn parse_declared_token(
    spec: &TokenSpec,
    response: &Value,
    now_ms: i64,
) -> Result<ParsedToken, OAuthError> {
    let access_token = required_string_pointer(response, &spec.access_token)?;
    let refresh_token = required_string_pointer(response, &spec.refresh_token)?;
    let expires_in = spec
        .expires_in
        .as_deref()
        .and_then(|pointer| response.pointer(pointer))
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .filter(|value| *value > 0)
        .unwrap_or(1);
    Ok(ParsedToken {
        access_token,
        refresh_token,
        expires_at_ms: add_duration(now_ms, Duration::from_secs(expires_in))?,
    })
}

fn required_string_pointer(value: &Value, pointer: &str) -> Result<String, OAuthError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| OAuthError::InvalidResponse("token field is missing".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use pluribus_core::{CredentialStore, InMemoryCredentialStore, PrincipalKind};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FixedClock;

    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            1_000
        }
    }

    struct FakeTransport(Mutex<VecDeque<OAuthHttpResponse>>);

    #[async_trait::async_trait]

    impl OAuthTransport for FakeTransport {
        async fn post(&self, _request: &OAuthHttpRequest) -> Result<OAuthHttpResponse, OAuthError> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| OAuthError::Unavailable("missing response".into()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn static_flow_seals_input_without_returning_it() {
        let store = Arc::new(InMemoryCredentialStore::default());
        let enrollment = CredentialEnrollment::new(
            store.clone(),
            Arc::new(FakeTransport(Mutex::new(VecDeque::new()))),
            Arc::new(FixedClock),
        );
        let component = PrincipalRef::new(PrincipalKind::Component, "telegram-1");
        let handle = SecretHandle::new("telegram:primary");

        enrollment
            .enroll_static(
                &json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["token"],
                    "properties": {"token": {"type": "string"}}
                }),
                &json!({
                    "http": {
                        "origins": ["https://api.telegram.org"],
                        "pathPrefix": {"parts": [
                            {"literal": "/bot"},
                            {"input": "token"}
                        ]}
                    }
                }),
                &json!({"token": "123:secret"}),
                &handle,
                &EnrollmentPolicy {
                    components: vec![component.clone()],
                    enrollment_origins: Vec::new(),
                    injection_origins: vec!["https://api.telegram.org".into()],
                },
            )
            .await
            .unwrap();

        let credential = store
            .resolve_http(&handle, &component, "https://api.telegram.org")
            .await
            .unwrap();
        assert_eq!(
            credential.path_prefix.unwrap().as_bytes(),
            b"/bot123:secret"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn device_flow_seals_tokens_and_claims() {
        let access_token = jwt("account-1");
        let transport = Arc::new(FakeTransport(Mutex::new(
            vec![
                response(&json!({
                    "device_auth_id": "device-1",
                    "user_code": "ABCD-EFGH",
                    "interval": "5"
                })),
                OAuthHttpResponse {
                    status: 403,
                    body: b"{}".to_vec(),
                },
                response(&json!({
                    "authorization_code": "code",
                    "code_verifier": "verifier"
                })),
                response(&json!({
                    "access_token": access_token,
                    "refresh_token": "refresh-1",
                    "expires_in": 3600
                })),
            ]
            .into(),
        )));
        let store = Arc::new(InMemoryCredentialStore::default());
        let enrollment = CredentialEnrollment::new(store.clone(), transport, Arc::new(FixedClock));
        let component = PrincipalRef::new(PrincipalKind::Component, "codex-1");
        let mut session = enrollment
            .begin_device(
                &json!({"type": "object", "additionalProperties": false}),
                &device_flow(),
                &json!({}),
                SecretHandle::new("codex:primary"),
                EnrollmentPolicy {
                    components: vec![component.clone()],
                    enrollment_origins: vec!["https://auth.example.com".into()],
                    injection_origins: vec!["https://api.example.com".into()],
                },
            )
            .await
            .unwrap();
        assert_eq!(session.prompt().user_code, "ABCD-EFGH");
        assert_eq!(
            enrollment.poll_device(&mut session).await.unwrap(),
            DeviceCodeStatus::Pending
        );
        assert_eq!(
            enrollment.poll_device(&mut session).await.unwrap(),
            DeviceCodeStatus::Authorized
        );

        let credential = store
            .resolve_http(
                &SecretHandle::new("codex:primary"),
                &component,
                "https://api.example.com",
            )
            .await
            .unwrap();
        assert!(credential.headers[0].value.starts_with(b"Bearer ey"));
        assert_eq!(credential.headers[1].value, b"account-1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_renders_declared_token_headers() {
        let store = Arc::new(InMemoryCredentialStore::default());
        let transport = Arc::new(FakeTransport(Mutex::new(vec![
            response(&json!({"device_auth_id":"device", "user_code":"code"})),
            response(&json!({"authorization_code":"code"})),
            response(&json!({"access_token":jwt("account-1"), "refresh_token":"refresh", "expires_in":1})),
            response(&json!({"access_token":jwt("account-1"), "refresh_token":"rotated", "expires_in":3600})),
        ].into())));
        let enrollment =
            CredentialEnrollment::new(store.clone(), transport.clone(), Arc::new(FixedClock));
        let mut flow = device_flow();
        flow["http"]["headers"].as_array_mut().unwrap().push(json!({
            "name":"x-derived-token", "value":{"parts":[{"accessToken":{}}]}
        }));
        let component = PrincipalRef::new(PrincipalKind::Component, "example");
        let handle = SecretHandle::new("refresh-headers");
        let mut session = enrollment
            .begin_device(
                &json!({"type":"object"}),
                &flow,
                &json!({}),
                handle.clone(),
                EnrollmentPolicy {
                    components: vec![component.clone()],
                    enrollment_origins: vec!["https://auth.example.com".into()],
                    injection_origins: vec!["https://api.example.com".into()],
                },
            )
            .await
            .unwrap();
        enrollment.poll_device(&mut session).await.unwrap();
        // A distinct token with the same account exercises injection regeneration.
        transport.0.lock().unwrap().back_mut().unwrap().body = serde_json::to_vec(
            &json!({"access_token":format!("{}.new", jwt("account-1")), "expires_in":3600}),
        )
        .unwrap();
        let refreshing =
            super::super::RefreshingCredentialStore::new(store, transport, Arc::new(FixedClock));
        let credential = refreshing
            .resolve_http(&handle, &component, "https://api.example.com")
            .await
            .unwrap();
        assert_eq!(
            credential.headers[2].value,
            format!("{}.new", jwt("account-1")).as_bytes()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn future_expiry_auth_rejection_refreshes_once() {
        let store = Arc::new(InMemoryCredentialStore::default());
        let transport = Arc::new(FakeTransport(Mutex::new(vec![
            response(&json!({"device_auth_id":"device", "user_code":"code"})),
            response(&json!({"authorization_code":"code"})),
            response(&json!({"access_token":jwt("account-1"), "refresh_token":"refresh", "expires_in":3600})),
            response(&json!({"access_token":jwt("account-1"), "refresh_token":"rotated", "expires_in":3600})),
        ].into())));
        let enrollment =
            CredentialEnrollment::new(store.clone(), transport.clone(), Arc::new(FixedClock));
        let mut flow = device_flow();
        flow["authFailure"] =
            json!([{"status":401,"errorPointer":"/error/code","codes":["expired_token"]}]);
        flow["replay"] =
            json!([{"origin":"https://api.example.com","method":"POST","path":"/completion"}]);
        let component = PrincipalRef::new(PrincipalKind::Component, "example");
        let handle = SecretHandle::new("future-expiry");
        let mut session = enrollment
            .begin_device(
                &json!({"type":"object"}),
                &flow,
                &json!({}),
                handle.clone(),
                EnrollmentPolicy {
                    components: vec![component.clone()],
                    enrollment_origins: vec!["https://auth.example.com".into()],
                    injection_origins: vec!["https://api.example.com".into()],
                },
            )
            .await
            .unwrap();
        enrollment.poll_device(&mut session).await.unwrap();
        let refreshing = super::super::RefreshingCredentialStore::new(
            store,
            transport.clone(),
            Arc::new(FixedClock),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let resolved = refreshing
            .resolve_http_versioned(&handle, &component, "https://api.example.com", deadline)
            .await
            .unwrap();
        let recovered = refreshing
            .recover_http(
                &handle,
                &component,
                "https://api.example.com",
                resolved.generation.unwrap_or(1),
                401,
                br#"{"error":{"code":"expired_token"}}"#,
                "POST",
                "/completion",
                deadline,
            )
            .await
            .unwrap();
        assert_eq!(
            recovered,
            pluribus_core::AuthRecovery::Refreshed {
                replay_allowed: true
            }
        );
        assert!(transport.0.lock().unwrap().is_empty());
        refreshing
            .recover_http(
                &handle,
                &component,
                "https://api.example.com",
                resolved.generation.unwrap(),
                401,
                br#"{"error":{"code":"expired_token"}}"#,
                "POST",
                "/completion",
                deadline,
            )
            .await
            .unwrap();
    }

    fn device_flow() -> Value {
        json!({
            "provider": "example",
            "start": request("https://auth.example.com/start", &[
                json!({"name": "client_id", "value": literal("client")})
            ], "json"),
            "prompt": {
                "verificationUri": literal("https://auth.example.com/device"),
                "userCode": response_template("start", "/user_code"),
                "intervalSeconds": {"step": "start", "pointer": "/interval", "default": 5},
                "expiresInSeconds": {"default": 900}
            },
            "poll": {
                "request": request("https://auth.example.com/poll", &[
                    json!({"name": "device", "value": response_template("start", "/device_auth_id")})
                ], "json"),
                "pendingStatuses": [403],
                "errorPointer": "/error",
                "pendingErrors": ["pending"],
                "slowDownErrors": ["slow_down"],
                "deniedErrors": ["denied"],
                "expiredErrors": ["expired"]
            },
            "exchange": request("https://auth.example.com/token", &[
                json!({"name": "code", "value": response_template("poll", "/authorization_code")})
            ], "form"),
            "token": {
                "accessTokenPointer": "/access_token",
                "refreshTokenPointer": "/refresh_token",
                "expiresInPointer": "/expires_in"
            },
            "refresh": {
                "request": request("https://auth.example.com/token", &[
                    json!({"name": "refresh_token", "value": {"parts": [{"token": "refreshToken"}]}})
                ], "form"),
                "response": {
                    "accessTokenPointer": "/access_token",
                    "refreshTokenPointer": "/refresh_token"
                }
            },
            "expiry": {"relative": {"pointer": "/expires_in", "unit": "seconds"}, "skewSeconds": 60},
            "accountPointer": "/account/id",
            "authFailure": [],
            "replay": [],
            "http": {
                "origins": ["https://api.example.com"],
                "headers": [
                    {"name": "authorization", "value": {"parts": [
                        {"literal": "Bearer "}, {"accessToken": {}}
                    ]}},
                    {"name": "account-id", "value": {"parts": [
                        {"jwtClaim": {"pointer": "/account/id"}}
                    ]}}
                ]
            }
        })
    }

    fn request(url: &str, fields: &[Value], encoding: &str) -> Value {
        json!({"method": "POST", "url": url, "body": {"encoding": encoding, "fields": fields}})
    }

    fn literal(value: &str) -> Value {
        json!({"parts": [{"literal": value}]})
    }

    fn response_template(step: &str, pointer: &str) -> Value {
        json!({"parts": [{"response": {"step": step, "pointer": pointer}}]})
    }

    fn response(value: &Value) -> OAuthHttpResponse {
        OAuthHttpResponse {
            status: 200,
            body: serde_json::to_vec(&value).unwrap(),
        }
    }

    fn jwt(account: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&json!({"account": {"id": account}})).unwrap());
        format!("{header}.{payload}.signature")
    }
    fn shared_static_flow() -> Value {
        json!({"http":{"origins":["https://api.telegram.org"],"pathPrefix":{"parts":[{"literal":"/bot"},{"input":"token"}]}}})
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_credential_is_scoped_to_every_declared_consumer() {
        let store = Arc::new(InMemoryCredentialStore::default());
        let enrollment = CredentialEnrollment::new(
            store.clone(),
            Arc::new(FakeTransport(Mutex::new(VecDeque::new()))),
            Arc::new(FixedClock),
        );
        let consumers = vec![
            PrincipalRef::new(PrincipalKind::Component, "telegram/receive"),
            PrincipalRef::new(PrincipalKind::Component, "telegram/send"),
        ];
        let policy = EnrollmentPolicy {
            components: consumers.clone(),
            enrollment_origins: vec![],
            injection_origins: vec!["https://api.telegram.org".into()],
        };
        let handle = SecretHandle::new("shared");
        enrollment
            .enroll_static(
                &json!({"type":"object"}),
                &shared_static_flow(),
                &json!({"token":"secret"}),
                &handle,
                &policy,
            )
            .await
            .unwrap();
        for consumer in consumers {
            let credential = store
                .resolve_http(&handle, &consumer, "https://api.telegram.org")
                .await
                .unwrap();
            assert_eq!(credential.allowed_components, policy.components);
        }
        assert!(
            store
                .resolve_http(
                    &handle,
                    &PrincipalRef::new(PrincipalKind::Component, "other/send"),
                    "https://api.telegram.org"
                )
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enrollment_rejects_empty_duplicate_and_noncomponent_consumers() {
        let component = PrincipalRef::new(PrincipalKind::Component, "telegram/send");
        for consumers in [
            vec![],
            vec![component.clone(), component],
            vec![PrincipalRef::new(PrincipalKind::Agent, "agent")],
        ] {
            let store = Arc::new(InMemoryCredentialStore::default());
            let enrollment = CredentialEnrollment::new(
                store,
                Arc::new(FakeTransport(Mutex::new(VecDeque::new()))),
                Arc::new(FixedClock),
            );
            let policy = EnrollmentPolicy {
                components: consumers,
                enrollment_origins: vec!["https://auth.openai.com".into()],
                injection_origins: vec![
                    "https://api.telegram.org".into(),
                    "https://chatgpt.com".into(),
                ],
            };
            assert!(matches!(
                enrollment
                    .enroll_static(
                        &json!({"type":"object"}),
                        &shared_static_flow(),
                        &json!({"token":"secret"}),
                        &SecretHandle::new("shared"),
                        &policy
                    )
                    .await,
                Err(OAuthError::AuthorizationDenied)
            ));
            let flow: Value = serde_json::from_str(include_str!(
                "../../../plugins/openai-codex/flows/subscription.json"
            ))
            .unwrap();
            assert!(matches!(
                enrollment
                    .begin_device(
                        &json!({"type":"object"}),
                        &flow,
                        &json!({}),
                        SecretHandle::new("shared"),
                        policy
                    )
                    .await,
                Err(OAuthError::AuthorizationDenied)
            ));
        }
    }
}
