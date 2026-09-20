use crate::{
    Config,
    common::*,
    http,
    pluribus::plugin::{credentials, types::Error},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use hmac::{Hmac, Mac};
use rsa::{Pkcs1v15Sign, RsaPrivateKey, pkcs1::DecodeRsaPrivateKey, pkcs8::DecodePrivateKey};
use serde_json::{Value, json};
use sha2_10::{Digest, Sha256};

const REFRESH_MARGIN_MS: i64 = 360_000;
const WEBHOOK_SECRET_TTL_MS: i64 = 600_000;

pub fn load(config: &Config) -> Result<(Option<Vec<u8>>, Value), Error> {
    let bytes = credentials::get(&config.credentials.app)?;
    let value = bytes
        .as_ref()
        .map(|b| serde_json::from_slice(b))
        .transpose()
        .map_err(|_| error("invalid credential record"))?
        .unwrap_or_else(|| json!({}));
    Ok((bytes, value))
}
pub fn save(config: &Config, previous: Option<&[u8]>, value: &Value) -> Result<(), Error> {
    if !credentials::compare_and_swap(
        &config.credentials.app,
        previous,
        &serde_json::to_vec(value).unwrap(),
    )? {
        return Err(error("credential changed during operation"));
    }
    Ok(())
}
pub fn jwt(app: &Value, now_ms: i64) -> Result<String, Error> {
    let pem = app["pem"]
        .as_str()
        .ok_or_else(|| error("App key unavailable"))?;
    let key = RsaPrivateKey::from_pkcs8_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
        .map_err(|_| error("invalid App key"))?;
    let input = format!("{}.{}", URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(json!({"iat":now_ms/1000-60,"exp":now_ms/1000+540,"iss":app["id"].as_u64().ok_or_else(||error("invalid App ID"))?.to_string()}).to_string()));
    let signature = key
        .sign(
            Pkcs1v15Sign::new::<Sha256>(),
            &Sha256::digest(input.as_bytes()),
        )
        .map_err(|_| error("App signing failed"))?;
    Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature)))
}
pub fn verify(secret: &str, signature: &str, body: &[u8]) -> bool {
    let Some(hex) = signature.strip_prefix("sha256=").filter(|s| s.len() == 64) else {
        return false;
    };
    let bytes: Option<Vec<u8>> = hex
        .as_bytes()
        .chunks_exact(2)
        .map(|p| {
            std::str::from_utf8(p)
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
        })
        .collect();
    let Some(bytes) = bytes else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    mac.verify_slice(&bytes).is_ok()
}
fn api(method: &str, path: &str, token: Option<&str>) -> Result<Value, Error> {
    let mut headers = vec![
        http::Header {
            name: "accept".into(),
            value: b"application/vnd.github+json".to_vec(),
        },
        http::Header {
            name: "user-agent".into(),
            value: b"pluribus".to_vec(),
        },
        http::Header {
            name: "x-github-api-version".into(),
            value: b"2022-11-28".to_vec(),
        },
    ];
    if let Some(token) = token {
        headers.push(http::Header {
            name: "authorization".into(),
            value: format!("Bearer {token}").into_bytes(),
        });
    }
    let response = http::exchange(&http::InlineRequest {
        method: method.into(),
        url: format!("https://api.github.com{path}"),
        headers,
        body: vec![],
        timeout_ms: 15000,
    })?;
    if !(200..300).contains(&response.status) {
        return Err(error("GitHub API request failed"));
    }
    serde_json::from_slice(&response.body).map_err(|_| error("invalid GitHub response"))
}
fn installation_matches(value: &Value, owner: &str, app_id: u64) -> bool {
    value["app_id"].as_u64() == Some(app_id)
        && value["suspended_at"].is_null()
        && matches!(
            value["account"]["type"].as_str(),
            Some("User" | "Organization")
        )
        && value["account"]["login"]
            .as_str()
            .is_some_and(|login| login.eq_ignore_ascii_case(owner))
}

fn installation(config: &Config, app: &Value, now: i64) -> Result<Value, Error> {
    let app_id = app["id"].as_u64().ok_or_else(|| error("invalid App ID"))?;
    let token = jwt(app, now)?;
    for page in 1..=100 {
        let value = api(
            "GET",
            &format!("/app/installations?per_page=100&page={page}"),
            Some(&token),
        )?;
        let installations = value
            .as_array()
            .ok_or_else(|| error("invalid GitHub installations response"))?;
        if let Some(install) = installations
            .iter()
            .find(|value| installation_matches(value, &config.owner, app_id))
        {
            return Ok(install.clone());
        }
        if installations.len() < 100 {
            break;
        }
    }
    Err(error("App is not installed for the configured owner"))
}

fn token_expiry(token: &Value) -> Result<i64, Error> {
    Ok(time::OffsetDateTime::parse(
        token["expires_at"].as_str().unwrap_or(""),
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|_| error("invalid token expiry"))?
    .unix_timestamp()
        * 1000)
}

fn make_exports(app: &Value, token: &Value, now: i64) -> Result<Value, Error> {
    let expires = token_expiry(token)?;
    let value = token["token"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| error("missing installation token"))?;
    let secret = app["webhook_secret"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| error("missing webhook secret"))?;
    if expires <= now + REFRESH_MARGIN_MS {
        return Err(error("installation token expires too soon"));
    }
    Ok(json!({
        "installation-token": {"value": value, "expires_at_ms": expires},
        "webhook-secret": {"value": secret, "expires_at_ms": now + WEBHOOK_SECRET_TTL_MS}
    }))
}

fn cached_exports(app: &Value, exports: &Value, now: i64) -> Result<Option<Value>, Error> {
    let token = &exports["installation-token"];
    let token_value = token["value"].as_str().filter(|value| !value.is_empty());
    let token_expiry = token["expires_at_ms"].as_i64().unwrap_or(0);
    if token_value.is_none() || token_expiry <= now + REFRESH_MARGIN_MS {
        return Ok(None);
    }
    let secret = app["webhook_secret"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| error("missing webhook secret"))?;
    let current_secret = &exports["webhook-secret"];
    if current_secret["value"].as_str() == Some(secret)
        && current_secret["expires_at_ms"]
            .as_i64()
            .is_some_and(|expires| expires > now + REFRESH_MARGIN_MS)
    {
        return Ok(Some(exports.clone()));
    }
    Ok(Some(json!({
        "installation-token": {"value": token_value.unwrap(), "expires_at_ms": token_expiry},
        "webhook-secret": {"value": secret, "expires_at_ms": now + WEBHOOK_SECRET_TTL_MS}
    })))
}

fn cache_matches_installation(doc: &Value, installation_id: u64) -> bool {
    doc["installation_id"].as_u64() == Some(installation_id)
}

fn clear_installation(doc: &mut Value) {
    doc["installation_id"] = Value::Null;
    doc["exports"] = json!({});
}

fn repository_response_matches(repository_id: u64, repository: &Value) -> bool {
    repository["id"].as_u64() == Some(repository_id)
}

pub fn authorize_repository(repository_id: u64, token: &str) -> Result<bool, Error> {
    for page in 1..=1000 {
        let value = api(
            "GET",
            &format!("/installation/repositories?per_page=100&page={page}"),
            Some(token),
        )?;
        let repositories = value["repositories"]
            .as_array()
            .ok_or_else(|| error("invalid GitHub repositories response"))?;
        if repositories
            .iter()
            .any(|repository| repository_response_matches(repository_id, repository))
        {
            return Ok(true);
        }
        if repositories.len() < 100 {
            return Ok(false);
        }
    }
    Err(error("GitHub repositories pagination exceeded limit"))
}
pub fn refresh(config: &Config, now: i64) -> Result<(), Error> {
    let (previous, mut doc) = load(config)?;
    if doc["app"].is_null() {
        return Ok(());
    }
    let result = (|| {
        let install = installation(config, &doc["app"], now)?;
        let installation_id = install["id"]
            .as_u64()
            .ok_or_else(|| error("invalid installation"))?;
        let cache_matches = cache_matches_installation(&doc, installation_id);
        doc["installation_id"] = json!(installation_id);
        if cache_matches && let Some(exports) = cached_exports(&doc["app"], &doc["exports"], now)? {
            doc["exports"] = exports;
            return Ok(());
        }
        let token = api(
            "POST",
            &format!("/app/installations/{}/access_tokens", installation_id),
            Some(&jwt(&doc["app"], now)?),
        )?;
        doc["exports"] = make_exports(&doc["app"], &token, now)?;
        Ok(())
    })();
    if result.is_err() {
        clear_installation(&mut doc);
    }
    save(config, previous.as_deref(), &doc)?;
    result
}
fn app_from_input(
    input: &Value,
    now: i64,
    lookup: impl FnOnce(&str) -> Result<Value, Error>,
) -> Result<Value, Error> {
    let id = input["app_id"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| error("invalid App ID"))?;
    let pem = input["private_key"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| error("missing App key"))?;
    let secret = input["webhook_secret"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| error("missing webhook secret"))?;
    let mut app = json!({"id":id,"pem":pem,"webhook_secret":secret});
    let remote = lookup(&jwt(&app, now)?)?;
    if remote["id"] != id {
        return Err(error("App ID mismatch"));
    }
    let slug = remote["slug"]
        .as_str()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .ok_or_else(|| error("invalid App slug"))?;
    app["slug"] = json!(slug);
    app["owner"] = remote["owner"].clone();
    Ok(app)
}

pub fn enroll(config: &Config, now: i64, enrollment_id: &str) -> Result<String, Error> {
    let (previous, mut doc) = load(config)?;
    if !enrollment_id.is_empty() && doc["enrollment_result"]["id"] == enrollment_id {
        return doc["enrollment_result"]["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| error("invalid enrollment result"));
    }
    if enrollment_id.is_empty()
        || doc["enrollment"]["id"] != enrollment_id
        || doc["enrollment"]["expires_at_ms"].as_i64().unwrap_or(0) <= now
    {
        return Err(error("credential input unavailable"));
    }
    let result = app_from_input(&doc["enrollment"]["input"], now, |token| {
        api("GET", "/app", Some(token))
    });
    doc.as_object_mut()
        .ok_or_else(|| error("invalid credential record"))?
        .remove("enrollment");
    match result {
        Ok(app) => {
            let url = format!(
                "https://github.com/apps/{}/installations/new",
                app["slug"].as_str().unwrap()
            );
            doc["app"] = app;
            doc["installation_id"] = Value::Null;
            doc["exports"] = json!({});
            doc["enrollment_result"] = json!({"id":enrollment_id,"url":url});
            save(config, previous.as_deref(), &doc)?;
            Ok(url)
        }
        Err(failure) => {
            save(config, previous.as_deref(), &doc)?;
            Err(failure)
        }
    }
}

pub fn response(status: u16, body: &str) -> Value {
    json!({"status":status,"body":STANDARD.encode(body),"headers":[["content-type","text/plain; charset=utf-8"],["cache-control","no-store"]]})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_app_checks_the_key_and_remote_identity() {
        let input = json!({"app_id":"7","private_key":include_str!("../tests/fixtures/test-app.pem"),"webhook_secret":"fixture-secret"});
        let app = app_from_input(&input, 1_000_000, |token| {
            assert_eq!(token.split('.').count(), 3);
            Ok(json!({"id":7,"slug":"fixture","owner":{"type":"User","login":"me"}}))
        })
        .unwrap();
        assert_eq!(app["id"], 7);
        assert_eq!(app["slug"], "fixture");
        assert_eq!(app["webhook_secret"], "fixture-secret");
        let other_owner = json!({
            "app_id":"7",
            "private_key":include_str!("../tests/fixtures/test-app.pem"),
            "webhook_secret":"fixture-secret"
        });
        let app = app_from_input(&other_owner, 1_000_000, |_| {
            Ok(json!({"id":7,"slug":"fixture","owner":{"type":"User","login":"publisher"}}))
        })
        .unwrap();
        assert_eq!(app["owner"]["login"], "publisher");
        assert!(app_from_input(&input, 1_000_000, |_| Ok(json!({"id":8,"slug":"other"}))).is_err());
        let mut bad = input;
        bad["private_key"] = json!("invalid-secret-key");
        let failure = app_from_input(&bad, 1_000_000, |_| {
            panic!("invalid key must not call GitHub")
        })
        .unwrap_err();
        assert!(!failure.message.contains("invalid-secret-key"));
    }

    #[test]
    fn signature_checks_exact_bytes_and_key() {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(b"raw\n");
        let signature = format!(
            "sha256={}",
            mac.finalize()
                .into_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        assert!(verify("secret", &signature, b"raw\n"));
        assert!(!verify("secret", &signature, b"raw"));
        assert!(!verify("wrong", &signature, b"raw\n"));
        assert!(!verify("secret", "sha256=zz", b"raw\n"));
    }
    #[test]
    fn jwt_signature_and_expiry() {
        let pem = include_str!("../tests/fixtures/test-app.pem");
        let token = jwt(&json!({"id":7,"pem":pem}), 1_000_000).unwrap();
        let parts: Vec<_> = token.split('.').collect();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["exp"], 1540);
        assert_eq!(claims["iss"], "7");
        let key = RsaPrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
            .unwrap();
        key.to_public_key()
            .verify(
                Pkcs1v15Sign::new::<Sha256>(),
                &Sha256::digest(format!("{}.{}", parts[0], parts[1]).as_bytes()),
                &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn exports_include_a_renewable_webhook_secret() {
        let app = json!({"webhook_secret":"fixture-secret"});
        let token = json!({"token":"ghs_fixture","expires_at":"1970-01-01T00:30:00Z"});
        let exports = make_exports(&app, &token, 1_000_000).unwrap();
        assert_eq!(exports["installation-token"]["value"], "ghs_fixture");
        assert_eq!(exports["installation-token"]["expires_at_ms"], 1_800_000);
        assert_eq!(exports["webhook-secret"]["value"], "fixture-secret");
        assert_eq!(exports["webhook-secret"]["expires_at_ms"], 1_600_000);
    }

    #[test]
    fn cached_token_backfills_missing_webhook_secret_export() {
        let app = json!({"webhook_secret":"fixture-secret"});
        let current = json!({
            "installation-token":{"value":"ghs_fixture","expires_at_ms":1_800_000}
        });
        let exports = cached_exports(&app, &current, 1_000_000).unwrap().unwrap();
        assert_eq!(exports["webhook-secret"]["value"], "fixture-secret");
    }

    #[test]
    fn cached_token_at_refresh_margin_is_not_reused() {
        let app = json!({"webhook_secret":"fixture-secret"});
        let current = json!({
            "installation-token":{"value":"ghs_fixture","expires_at_ms":1_360_000},
            "webhook-secret":{"value":"fixture-secret","expires_at_ms":1_800_000}
        });
        assert!(cached_exports(&app, &current, 1_000_000).unwrap().is_none());
    }

    #[test]
    fn inaccessible_installation_clears_id_and_exports() {
        let mut doc = json!({
            "installation_id":9,
            "exports":{"installation-token":{"value":"ghs_fixture"},"webhook-secret":{"value":"secret"}}
        });
        clear_installation(&mut doc);
        assert!(doc["installation_id"].is_null());
        assert_eq!(doc["exports"], json!({}));
    }

    #[test]
    fn cached_exports_are_bound_to_the_current_installation() {
        let doc = json!({"installation_id":9});
        assert!(cache_matches_installation(&doc, 9));
        assert!(!cache_matches_installation(&doc, 10));
    }

    #[test]
    fn installation_selection_accepts_users_orgs_and_selected_repositories() {
        assert!(installation_matches(
            &json!({"id":1,"app_id":7,"account":{"login":"me","type":"User"},"repository_selection":"all","suspended_at":null}),
            "me",
            7
        ));
        assert!(installation_matches(
            &json!({"id":2,"app_id":7,"account":{"login":"acme","type":"Organization"},"repository_selection":"selected","suspended_at":null}),
            "acme",
            7
        ));
        assert!(!installation_matches(
            &json!({"id":3,"app_id":8,"account":{"login":"acme","type":"Organization"},"repository_selection":"selected","suspended_at":null}),
            "acme",
            7
        ));
    }

    #[test]
    fn repository_authorization_requires_the_requested_id() {
        assert!(repository_response_matches(42, &json!({"id":42})));
        assert!(!repository_response_matches(42, &json!({"id":43})));
        assert!(!repository_response_matches(
            42,
            &json!({"full_name":"acme/repo"})
        ));
    }
}
