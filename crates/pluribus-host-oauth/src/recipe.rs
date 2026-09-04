use super::{
    BTreeMap, DeviceFlow, Duration, Encoding, EnrollmentPolicy, HttpSpec, Map, OAuthCredential,
    OAuthError, OAuthHttpRequest, RefreshSpec, RequestSpec, Serializer, Value, account_id_claim,
    add_duration, render, render_http, require_allowed_origin, require_allowed_url,
    required_string_pointer,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Expiry {
    relative: Option<DeadlineSource>,
    absolute: Option<DeadlineSource>,
    #[serde(default)]
    jwt_exp: bool,
    refresh_interval_seconds: Option<u64>,
    skew_seconds: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeadlineSource {
    pointer: String,
    unit: Unit,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Unit {
    Seconds,
    Milliseconds,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct AuthFailure {
    status: u16,
    error_pointer: Option<String>,
    #[serde(default)]
    codes: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Replay {
    origin: String,
    method: String,
    path: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FixedHeader {
    pub(super) name: String,
    pub(super) value: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RefreshResponse {
    access_token_pointer: String,
    refresh_token_pointer: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Refresh {
    request: Value,
    response: RefreshResponse,
    failure: Option<RefreshFailure>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RefreshFailure {
    retryable_statuses: Vec<u16>,
    backoff_seconds: u64,
}

/// Serialized only inside host credential storage.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRecipe {
    version: u32,
    digest: String,
    refresh: Refresh,
    expiry: Expiry,
    http: HttpSpec,
    input: Value,
    auth_failure: Vec<AuthFailure>,
    replay: Vec<Replay>,
    account_pointer: Option<String>,
    account: Option<String>,
}

impl StoredRecipe {
    pub(super) fn validate(
        flow: &DeviceFlow,
        input: &Value,
        policy: &EnrollmentPolicy,
    ) -> Result<(), OAuthError> {
        let recipe = Self::build(flow, input)?;
        if flow.expiry.is_some()
            && jwt_injection_pointer(&flow.http).is_some()
            && flow.account_pointer.is_none()
        {
            return Err(invalid(
                "JWT injection requires an account identity pointer",
            ));
        }
        require_allowed_url(recipe.endpoint(), &policy.enrollment_origins)?;
        if recipe.expiry.relative.is_none()
            && recipe.expiry.absolute.is_none()
            && !recipe.expiry.jwt_exp
            && recipe.expiry.refresh_interval_seconds.is_none()
        {
            return Err(invalid(
                "expiry requires a source or bounded refresh interval",
            ));
        }
        for matcher in &recipe.auth_failure {
            if !matches!(matcher.status, 401 | 403)
                || matcher.error_pointer.is_some() == matcher.codes.is_empty()
            {
                return Err(invalid("authentication matcher is malformed"));
            }
        }
        for rule in &recipe.replay {
            require_allowed_origin(&rule.origin, &recipe.http.origins)?;
            if !rule.path.starts_with('/') || rule.path.contains(['?', '#']) {
                return Err(invalid("replay path is malformed"));
            }
        }
        let request: RequestSpec = serde_json::from_value(materialize(
            &recipe.refresh.request,
            input,
            "fixture-access",
            "fixture-refresh",
        )?)
        .map_err(|_| invalid("refresh request is malformed"))?;
        let mut names = std::collections::BTreeSet::new();
        for field in &request.body.fields {
            if !names.insert(&field.name) {
                return Err(invalid("refresh request repeats a field"));
            }
            render(
                &field.value,
                input,
                &BTreeMap::new(),
                Some("fixture-access"),
            )?;
        }
        for header in &request.headers {
            let name = reqwest::header::HeaderName::from_bytes(header.name.as_bytes())
                .map_err(|_| invalid("refresh header is malformed"))?;
            reqwest::header::HeaderValue::from_str(&header.value)
                .map_err(|_| invalid("refresh header is malformed"))?;
            if matches!(
                name.as_str(),
                "host" | "authorization" | "content-length" | "content-type" | "cookie"
            ) {
                return Err(invalid("refresh header is reserved"));
            }
        }
        validate_references(
            &serde_json::to_value(&recipe.http).map_err(|_| OAuthError::Storage)?,
            false,
        )?;
        Ok(())
    }

    fn build(flow: &DeviceFlow, input: &Value) -> Result<Self, OAuthError> {
        let refresh = if flow.expiry.is_some() {
            serde_json::from_value(flow.refresh.clone())
                .map_err(|_| invalid("refresh declaration is malformed"))?
        } else {
            let legacy: RefreshSpec = serde_json::from_value(flow.refresh.clone())
                .map_err(|_| invalid("legacy refresh declaration is malformed"))?;
            Refresh {
                request: serde_json::json!({"method":"POST", "url":legacy.url,"body":{"encoding":"form","fields":[
                    {"name":"grant_type","value":{"parts":[{"literal":"refresh_token"}]}},
                    {"name":"client_id","value":{"parts":[{"literal":legacy.client_id}]}},
                    {"name":"refresh_token","value":{"parts":[{"token":"refreshToken"}]}}
                ]}}),
                response: RefreshResponse {
                    access_token_pointer: "/access_token".into(),
                    refresh_token_pointer: Some("/refresh_token".into()),
                },
                failure: None,
            }
        };
        validate_references(&refresh.request, true)?;
        let expiry = flow.expiry.clone().unwrap_or(Expiry {
            relative: Some(DeadlineSource {
                pointer: flow
                    .token
                    .expires_in
                    .clone()
                    .ok_or_else(|| invalid("legacy expiry pointer is missing"))?,
                unit: Unit::Seconds,
            }),
            absolute: None,
            jwt_exp: false,
            refresh_interval_seconds: None,
            skew_seconds: 60,
        });
        let mut retained = Map::new();
        retain_inputs(&refresh.request, input, &mut retained)?;
        retain_inputs(
            &serde_json::to_value(&flow.http).map_err(|_| OAuthError::Storage)?,
            input,
            &mut retained,
        )?;
        let mut recipe = Self {
            version: 2,
            digest: String::new(),
            refresh,
            expiry,
            http: flow.http.clone(),
            input: Value::Object(retained),
            auth_failure: flow.auth_failure.clone(),
            replay: flow.replay.clone(),
            account_pointer: flow.account_pointer.clone().or_else(|| {
                if flow.expiry.is_none() {
                    jwt_injection_pointer(&flow.http)
                } else {
                    None
                }
            }),
            account: None,
        };
        recipe.digest = recipe.declaration_digest()?;
        Ok(recipe)
    }

    pub(super) fn enroll(
        flow: &DeviceFlow,
        input: &Value,
        _policy: &EnrollmentPolicy,
        access: &str,
    ) -> Result<Self, OAuthError> {
        let mut recipe = Self::build(flow, input)?;
        recipe.account = recipe
            .account_pointer
            .as_deref()
            .map(|pointer| account_id_claim(access, pointer))
            .transpose()?;
        Ok(recipe)
    }

    fn declaration_digest(&self) -> Result<String, OAuthError> {
        let declaration = serde_json::json!({"version":self.version,"refresh":self.refresh,"expiry":self.expiry,"http":self.http,"authFailure":self.auth_failure,"replay":self.replay,"accountPointer":self.account_pointer});
        Ok(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(
                serde_json::to_vec(&declaration).map_err(|_| OAuthError::Storage)?,
            )),
        )
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, OAuthError> {
        let recipe: Self = serde_json::from_slice(bytes)
            .map_err(|_| invalid("stored refresh recipe is malformed"))?;
        if recipe.version != 2 || recipe.digest != recipe.declaration_digest()? {
            return Err(invalid("stored refresh recipe digest mismatch"));
        }
        Ok(recipe)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, OAuthError> {
        serde_json::to_vec(self).map_err(|_| OAuthError::Storage)
    }
    pub(crate) fn endpoint(&self) -> &str {
        self.refresh
            .request
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
    pub(crate) fn skew_ms(&self) -> i64 {
        i64::try_from(self.expiry.skew_seconds.saturating_mul(1000)).unwrap_or(i64::MAX)
    }

    pub(crate) fn expiry(
        &self,
        response: &Value,
        access: &str,
        now: i64,
    ) -> Result<i64, OAuthError> {
        let mut deadlines = Vec::new();
        for (source, relative) in [
            (&self.expiry.relative, true),
            (&self.expiry.absolute, false),
        ] {
            if let Some(source) = source
                && let Some(value) = response.pointer(&source.pointer)
            {
                let n = value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
                    .ok_or_else(|| invalid_response("expiry is malformed"))?;
                let ms = match source.unit {
                    Unit::Seconds => n.checked_mul(1000),
                    Unit::Milliseconds => Some(n),
                }
                .and_then(|n| i64::try_from(n).ok())
                .ok_or_else(|| invalid_response("expiry overflows"))?;
                deadlines.push(if relative {
                    now.checked_add(ms)
                        .ok_or_else(|| invalid_response("expiry overflows"))?
                } else {
                    ms
                });
            }
        }
        if self.expiry.jwt_exp {
            let payload = access
                .split('.')
                .nth(1)
                .ok_or_else(|| invalid_response("expiry JWT is malformed"))?;
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload)
                .map_err(|_| invalid_response("expiry JWT is malformed"))?;
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| invalid_response("expiry JWT is malformed"))?;
            if let Some(exp) = value.get("exp") {
                deadlines.push(
                    exp.as_i64()
                        .and_then(|n| n.checked_mul(1000))
                        .ok_or_else(|| invalid_response("JWT expiry is malformed"))?,
                );
            }
        }
        if let Some(interval) = self.expiry.refresh_interval_seconds {
            deadlines.push(add_duration(now, Duration::from_secs(interval))?);
        }
        let deadline = deadlines
            .into_iter()
            .min()
            .ok_or_else(|| invalid_response("expiry is missing"))?;
        if deadline <= now {
            return Err(invalid_response("replacement credential is expired"));
        }
        Ok(deadline)
    }

    pub(crate) fn backoff(&self, status: u16, now: i64) -> Result<Option<i64>, OAuthError> {
        self.refresh
            .failure
            .as_ref()
            .filter(|failure| failure.retryable_statuses.contains(&status))
            .map(|failure| add_duration(now, Duration::from_secs(failure.backoff_seconds)))
            .transpose()
    }

    pub(crate) fn matches(&self, status: u16, body: &[u8]) -> bool {
        if body.len() > 1024 * 1024 {
            return false;
        }
        self.auth_failure.iter().any(|matcher| {
            matcher.status == status
                && matcher.error_pointer.as_ref().is_none_or(|pointer| {
                    serde_json::from_slice::<Value>(body)
                        .ok()
                        .and_then(|value| {
                            value
                                .pointer(pointer)
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .is_some_and(|code| matcher.codes.contains(&code))
                })
        })
    }
    pub(crate) fn replay(&self, origin: &str, method: &str, path: &str) -> bool {
        self.replay
            .iter()
            .any(|rule| rule.origin == origin && rule.method == method && rule.path == path)
    }

    pub(crate) fn request(
        &self,
        current: &OAuthCredential,
        timeout: Duration,
    ) -> Result<OAuthHttpRequest, OAuthError> {
        let refresh =
            std::str::from_utf8(&current.refresh_token).map_err(|_| OAuthError::Storage)?;
        let access = current
            .access_token
            .as_deref()
            .and_then(|v| std::str::from_utf8(v).ok())
            .unwrap_or("");
        let spec: RequestSpec = serde_json::from_value(materialize(
            &self.refresh.request,
            &self.input,
            access,
            refresh,
        )?)
        .map_err(|_| invalid("refresh request is malformed"))?;
        let values = spec
            .body
            .fields
            .iter()
            .map(|field| {
                Ok((
                    field.name.clone(),
                    render(&field.value, &self.input, &BTreeMap::new(), Some(access))?,
                ))
            })
            .collect::<Result<Vec<_>, OAuthError>>()?;
        let (content_type, body) = match spec.body.encoding {
            Encoding::Json => (
                "application/json",
                serde_json::to_vec(
                    &values
                        .into_iter()
                        .map(|(k, v)| (k, Value::String(v)))
                        .collect::<Map<_, _>>(),
                )
                .map_err(|_| OAuthError::Storage)?,
            ),
            Encoding::Form => {
                let mut serializer = Serializer::new(String::new());
                serializer.extend_pairs(values);
                (
                    "application/x-www-form-urlencoded",
                    serializer.finish().into_bytes(),
                )
            }
        };
        Ok(OAuthHttpRequest {
            url: spec.url,
            content_type,
            body,
            headers: spec
                .headers
                .into_iter()
                .map(|h| (h.name, h.value))
                .collect(),
            timeout,
        })
    }

    pub(crate) fn apply(
        &self,
        current: &OAuthCredential,
        body: &[u8],
        now: i64,
    ) -> Result<OAuthCredential, OAuthError> {
        if body.len() > 1024 * 1024 {
            return Err(invalid_response("refresh response exceeds limit"));
        }
        let response: Value = serde_json::from_slice(body)
            .map_err(|_| invalid_response("refresh response is malformed"))?;
        let access =
            required_string_pointer(&response, &self.refresh.response.access_token_pointer)?;
        if let Some(pointer) = &self.account_pointer
            && Some(account_id_claim(&access, pointer)?) != self.account
        {
            return Err(OAuthError::AuthorizationDenied);
        }
        let refresh_token = self
            .refresh
            .response
            .refresh_token_pointer
            .as_deref()
            .filter(|pointer| response.pointer(pointer).is_some())
            .map(|pointer| required_string_pointer(&response, pointer))
            .transpose()?;
        let policy = EnrollmentPolicy {
            components: current.http.allowed_components.clone(),
            enrollment_origins: Vec::new(),
            injection_origins: current.http.allowed_origins.clone(),
        };
        let mut updated = current.clone();
        updated.http = render_http(
            &self.http,
            &self.input,
            &BTreeMap::new(),
            Some(&access),
            &policy,
        )?;
        updated
            .http
            .allowed_components
            .clone_from(&current.http.allowed_components);
        if let Some(token) = refresh_token {
            updated.refresh_token = token.into_bytes();
        }
        updated.expires_at_ms = self.expiry(&response, &access, now)?;
        if updated.expires_at_ms.saturating_sub(self.skew_ms()) <= now {
            return Err(invalid_response(
                "replacement credential expires within refresh skew",
            ));
        }
        updated.access_token = Some(access.into_bytes());
        Ok(updated)
    }
}

fn jwt_injection_pointer(http: &HttpSpec) -> Option<String> {
    http.headers
        .iter()
        .flat_map(|header| &header.value.parts)
        .chain(http.path_prefix.iter().flat_map(|template| &template.parts))
        .find_map(|part| match part {
            super::TemplatePart::JwtClaim(part) => Some(part.jwt_claim.pointer.clone()),
            _ => None,
        })
}

fn invalid(message: &str) -> OAuthError {
    OAuthError::InvalidRequest(message.into())
}
fn invalid_response(message: &str) -> OAuthError {
    OAuthError::InvalidResponse(message.into())
}

fn validate_references(value: &Value, refresh: bool) -> Result<(), OAuthError> {
    match value {
        Value::Object(map) => {
            if map.contains_key("response") || (!refresh && map.contains_key("token")) {
                return Err(invalid(
                    "refresh and injection cannot retain enrollment responses",
                ));
            }
            for value in map.values() {
                validate_references(value, refresh)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_references(value, refresh)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn retain_inputs(
    value: &Value,
    input: &Value,
    retained: &mut Map<String, Value>,
) -> Result<(), OAuthError> {
    match value {
        Value::Object(map) => {
            if let Some(name) = map.get("input").and_then(Value::as_str) {
                retained.insert(
                    name.into(),
                    input
                        .get(name)
                        .cloned()
                        .ok_or_else(|| invalid("retained input is missing"))?,
                );
            }
            for value in map.values() {
                retain_inputs(value, input, retained)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                retain_inputs(value, input, retained)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn materialize(
    value: &Value,
    input: &Value,
    access: &str,
    refresh: &str,
) -> Result<Value, OAuthError> {
    let _ = input;
    match value {
        Value::Object(map) if map.contains_key("token") => {
            match map.get("token").and_then(Value::as_str) {
                Some("accessToken") => Ok(serde_json::json!({"literal":access})),
                Some("refreshToken") => Ok(serde_json::json!({"literal":refresh})),
                _ => Err(invalid("unknown token slot")),
            }
        }
        Value::Object(map) => Ok(Value::Object(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), materialize(v, input, access, refresh)?)))
                .collect::<Result<_, OAuthError>>()?,
        )),
        Value::Array(values) => Ok(Value::Array(
            values
                .iter()
                .map(|v| materialize(v, input, access, refresh))
                .collect::<Result<_, _>>()?,
        )),
        _ => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{AccessTokenPart, Empty, HeaderSpec, Template, TemplatePart};
    use super::*;
    use pluribus_core::{PrincipalKind, PrincipalRef, SecretHeader};
    use serde_json::json;

    fn fixture() -> (StoredRecipe, OAuthCredential) {
        let flow: DeviceFlow = serde_json::from_str(include_str!(
            "../../../plugins/openai-codex/flows/subscription.json"
        ))
        .unwrap();
        let component = PrincipalRef::new(PrincipalKind::Component, "codex");
        let policy = EnrollmentPolicy {
            components: vec![component.clone()],
            enrollment_origins: vec!["https://auth.openai.com".into()],
            injection_origins: vec!["https://chatgpt.com".into()],
        };
        let access = jwt("account", None);
        let recipe = StoredRecipe::enroll(&flow, &json!({}), &policy, &access).unwrap();
        let http = render_http(
            &flow.http,
            &json!({}),
            &BTreeMap::new(),
            Some(&access),
            &policy,
        )
        .unwrap();
        let credential = OAuthCredential {
            provider: "fixture".into(),
            http,
            refresh_token: b"old-refresh-secret".to_vec(),
            access_token: Some(access.into_bytes()),
            refresh_recipe: Some(recipe.encode().unwrap()),
            expires_at_ms: 1000,
            token_url: recipe.endpoint().into(),
            client_id: "client".into(),
        };
        (recipe, credential)
    }
    fn jwt(account: &str, exp: Option<i64>) -> String {
        let mut claims = json!({"https://api.openai.com/auth":{"chatgpt_account_id":account}});
        if let Some(exp) = exp {
            claims["exp"] = json!(exp);
        }
        format!(
            "e30.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    #[test]
    fn account_claim_injection_requires_explicit_v2_identity() {
        let mut flow: DeviceFlow = serde_json::from_str(include_str!(
            "../../../plugins/openai-codex/flows/subscription.json"
        ))
        .unwrap();
        flow.account_pointer = None;
        let policy = EnrollmentPolicy {
            components: vec![PrincipalRef::new(PrincipalKind::Component, "codex")],
            enrollment_origins: vec!["https://auth.openai.com".into()],
            injection_origins: vec!["https://chatgpt.com".into()],
        };
        assert!(StoredRecipe::validate(&flow, &json!({}), &policy).is_err());
    }

    #[test]
    fn two_encodings_and_distinct_response_fields_render_every_header() {
        for encoding in ["form", "json"] {
            let (mut recipe, mut current) = fixture();
            recipe.refresh.request = json!({"method":"POST","url":"https://auth.example.com/token","headers":[{"name":"x-client","value":"fixed"}],"body":{"encoding":encoding,"fields":[{"name":"renewal","value":{"parts":[{"token":"refreshToken"}]}},{"name":"tenant","value":{"parts":[{"input":"tenant"}]}}]}});
            recipe.input = json!({"tenant":"retained"});
            recipe.refresh.response = RefreshResponse {
                access_token_pointer: "/credentials/access".into(),
                refresh_token_pointer: Some("/credentials/renewal".into()),
            };
            recipe.expiry = Expiry {
                relative: Some(DeadlineSource {
                    pointer: "/lifetime".into(),
                    unit: Unit::Milliseconds,
                }),
                absolute: None,
                jwt_exp: false,
                refresh_interval_seconds: None,
                skew_seconds: 0,
            };
            recipe.http.headers.push(HeaderSpec {
                name: "x-access".into(),
                value: Template {
                    parts: vec![TemplatePart::AccessToken(AccessTokenPart {
                        access_token: Empty {},
                    })],
                },
            });
            current.http.headers.push(SecretHeader {
                name: "x-access".into(),
                value: b"old".to_vec(),
            });
            let request = recipe.request(&current, Duration::from_secs(2)).unwrap();
            assert_eq!(request.headers, vec![("x-client".into(), "fixed".into())]);
            if encoding == "json" {
                assert_eq!(
                    serde_json::from_slice::<Value>(&request.body).unwrap(),
                    json!({"renewal":"old-refresh-secret","tenant":"retained"})
                );
            } else {
                assert_eq!(request.body, b"renewal=old-refresh-secret&tenant=retained");
            }
            let access = jwt("account", None);
            let updated=recipe.apply(&current,&serde_json::to_vec(&json!({"credentials":{"access":access,"renewal":"new-refresh-secret"},"lifetime":10000})).unwrap(),1000).unwrap();
            assert_eq!(updated.http.headers[2].value, access.as_bytes());
            assert_eq!(updated.refresh_token, b"new-refresh-secret");
            assert_eq!(updated.expires_at_ms, 11000);
        }
    }

    #[test]
    fn expiry_uses_earliest_deadline_and_rejects_invalid_sources() {
        let (mut recipe, _) = fixture();
        recipe.expiry.absolute = Some(DeadlineSource {
            pointer: "/absolute".into(),
            unit: Unit::Milliseconds,
        });
        assert_eq!(
            recipe
                .expiry(
                    &json!({"expires_in":90,"absolute":8000}),
                    &jwt("account", Some(5)),
                    1000
                )
                .unwrap(),
            5000
        );
        assert!(
            recipe
                .expiry(&json!({"expires_in":90}), "invalid", 1000)
                .is_err()
        );
        assert!(
            recipe
                .expiry(&json!({}), &jwt("account", None), 1000)
                .is_err()
        );
        assert!(
            recipe
                .expiry(&json!({"expires_in":0}), &jwt("account", None), 1000)
                .is_err()
        );
        assert!(
            recipe
                .expiry(&json!({"expires_in":u64::MAX}), &jwt("account", None), 1000)
                .is_err()
        );
        recipe.expiry.refresh_interval_seconds = Some(3);
        assert_eq!(
            recipe
                .expiry(&json!({}), &jwt("account", None), 1000)
                .unwrap(),
            4000
        );
    }

    #[test]
    fn account_switch_empty_rotation_and_expired_replacement_fail_without_secrets() {
        let (recipe, current) = fixture();
        for response in [
            json!({"access_token":jwt("other-account",None),"expires_in":3600}),
            json!({"access_token":jwt("account",None),"refresh_token":"","expires_in":3600}),
            json!({"access_token":jwt("account",Some(1)),"expires_in":3600}),
        ] {
            let error = recipe
                .apply(&current, &serde_json::to_vec(&response).unwrap(), 1000)
                .unwrap_err();
            assert!(!error.to_string().contains("other-account"));
            assert!(!error.to_string().contains("old-refresh-secret"));
        }
        let updated = recipe
            .apply(
                &current,
                &serde_json::to_vec(&json!({"access_token":jwt("account",None),"expires_in":3600}))
                    .unwrap(),
                1000,
            )
            .unwrap();
        assert_eq!(updated.refresh_token, current.refresh_token);
        assert_eq!(
            StoredRecipe::decode(&recipe.encode().unwrap())
                .unwrap()
                .digest,
            recipe.digest
        );
    }

    #[test]
    fn replacement_must_outlive_refresh_skew() {
        let (recipe, current) = fixture();
        assert!(
            recipe
                .apply(
                    &current,
                    &serde_json::to_vec(
                        &json!({"access_token":jwt("account",None),"expires_in":1})
                    )
                    .unwrap(),
                    1000
                )
                .is_err()
        );
    }

    #[test]
    fn recipe_digest_rejects_changed_authority_and_debug_redacts_inputs() {
        let (recipe, current) = fixture();
        let mut value: Value = serde_json::from_slice(&recipe.encode().unwrap()).unwrap();
        value["http"]["origins"] = json!(["https://other.example"]);
        assert!(StoredRecipe::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        let debug = format!("{current:?}");
        assert!(!debug.contains("old-refresh-secret"));
        assert!(!debug.contains("old-refresh-secret"));
    }
}
