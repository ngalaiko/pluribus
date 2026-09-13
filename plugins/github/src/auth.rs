use crate::{
    Config,
    common::*,
    pluribus::plugin::{credentials, http, types::Error},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use hmac::{Hmac, Mac};
use rsa::{Pkcs1v15Sign, RsaPrivateKey, pkcs1::DecodeRsaPrivateKey, pkcs8::DecodePrivateKey};
use serde_json::{Value, json};
use sha2_10::{Digest, Sha256};

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
fn installation(config: &Config, app: &Value, now: i64) -> Result<Value, Error> {
    let value = api(
        "GET",
        &format!("/users/{}/installation", config.owner),
        Some(&jwt(app, now)?),
    )?;
    if value["account"]["type"] != "User"
        || !value["account"]["login"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(&config.owner))
        || value["repository_selection"] != "all"
        || !value["suspended_at"].is_null()
        || value["app_id"] != app["id"]
    {
        return Err(error("App must be installed on all personal repositories"));
    }
    Ok(value)
}
pub fn refresh(config: &Config, now: i64) -> Result<(), Error> {
    let (previous, mut doc) = load(config)?;
    if doc["app"].is_null() {
        return Ok(());
    }
    let result = (|| {
        let install = installation(config, &doc["app"], now)?;
        doc["installation_id"] = install["id"].clone();
        if doc["exports"]["installation-token"]["expires_at_ms"]
            .as_i64()
            .unwrap_or(0)
            > now + 360000
        {
            return Ok(());
        }
        let token = api(
            "POST",
            &format!(
                "/app/installations/{}/access_tokens",
                install["id"]
                    .as_u64()
                    .ok_or_else(|| error("invalid installation"))?
            ),
            Some(&jwt(&doc["app"], now)?),
        )?;
        let expires = time::OffsetDateTime::parse(
            token["expires_at"].as_str().unwrap_or(""),
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| error("invalid token expiry"))?
        .unix_timestamp()
            * 1000;
        let value = token["token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| error("missing installation token"))?;
        if expires <= now + 360000 {
            return Err(error("installation token expires too soon"));
        }
        doc["exports"] = json!({"installation-token":{"value":value,"expires_at_ms":expires}});
        Ok(())
    })();
    if result.is_err() {
        doc["exports"] = json!({});
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
    })
    .and_then(|app| {
        if app["owner"]["type"] != "User"
            || !app["owner"]["login"]
                .as_str()
                .is_some_and(|owner| owner.eq_ignore_ascii_case(&config.owner))
        {
            return Err(error("App owner mismatch"));
        }
        Ok(app)
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
}
