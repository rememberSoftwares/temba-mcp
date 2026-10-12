use anyhow::{Result, ensure};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use temba_core::{endpoint, https_override};
use tokio::sync::Mutex;
pub struct Verifier {
    issuer: String,
    audience: String,
    jwks: String,
    token: String,
    client_id: String,
    callback_port: u16,
    http: reqwest::Client,
    cache: Mutex<Cache>,
}
struct Cache {
    keys: HashMap<String, DecodingKey>,
    fetched: Option<Instant>,
    attempted: Option<Instant>,
}
#[derive(Deserialize)]
struct Claims {
    sub: String,
    typ: String,
    #[serde(default)]
    groups: Vec<String>,
}
pub struct Identity {
    pub groups: Vec<String>,
}
pub fn rejection_reason(error: &anyhow::Error) -> &'static str {
    use jsonwebtoken::errors::ErrorKind;
    if let Some(error) = error.downcast_ref::<jsonwebtoken::errors::Error>() {
        return match error.kind() {
            ErrorKind::InvalidAudience => "invalid audience",
            ErrorKind::InvalidIssuer => "invalid issuer",
            ErrorKind::ExpiredSignature => "expired access token",
            ErrorKind::ImmatureSignature => "access token not yet valid",
            ErrorKind::InvalidSignature => "invalid signature",
            ErrorKind::Json(_) => {
                "malformed claims; groups must be an array of strings when present"
            }
            _ => "invalid access token",
        };
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return if error.is_timeout() {
            "signing-key discovery timed out"
        } else if error.is_status() {
            "signing-key discovery returned an HTTP error"
        } else {
            "signing-key discovery connection failed"
        };
    }
    match error.to_string().as_str() {
        "Unknown key" => "unknown signing key",
        "Missing kid" => "missing signing key identifier",
        "JWKS refresh cooling down" => "signing-key refresh cooling down",
        "Issuer mismatch" => "discovery issuer mismatch",
        "Access token required" => "access-token subject or type invalid",
        "Invalid groups claim" => "groups claim exceeds limits",
        _ => "token verification failed",
    }
}
impl Verifier {
    pub fn from_env() -> Result<Self> {
        let get = |n| std::env::var(n).unwrap_or_default();
        let issuer = if get("CATALOGUE_AUTH_ENABLED") == "true" {
            get("CATALOGUE_AUTH_ISSUER")
        } else {
            String::new()
        };
        let jwks = get("CATALOGUE_AUTH_JWKS_URI");
        let token = get("CATALOGUE_AUTH_TOKEN_ENDPOINT");
        if !issuer.is_empty() {
            endpoint(&issuer, false)?;
            if !jwks.is_empty() {
                https_override(&jwks, &issuer)?;
            }
            if !token.is_empty() {
                https_override(&token, &issuer)?;
            }
        }
        ensure!(
            get("CATALOGUE_AUTH_ENABLED") != "true" || !issuer.is_empty(),
            "CATALOGUE_AUTH_ISSUER required"
        );
        ensure!(
            issuer.is_empty()
                || (!get("CATALOGUE_AUTH_AUDIENCE").is_empty()
                    && !get("CATALOGUE_AUTH_CLIENT_ID").is_empty()),
            "CATALOGUE_AUTH_AUDIENCE and CATALOGUE_AUTH_CLIENT_ID required"
        );
        Ok(Self {
            issuer,
            audience: get("CATALOGUE_AUTH_AUDIENCE"),
            jwks,
            token,
            client_id: get("CATALOGUE_AUTH_CLIENT_ID"),
            callback_port: get("CATALOGUE_AUTH_CALLBACK_PORT").parse().unwrap_or(19877),
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()?,
            cache: Mutex::new(Cache {
                keys: HashMap::new(),
                fetched: None,
                attempted: None,
            }),
        })
    }
    pub fn public_config(&self) -> Value {
        if self.issuer.is_empty() {
            return Value::Null;
        }
        json!({"issuer":self.issuer,"audience":self.audience,"clientId":self.client_id,"callbackPort":self.callback_port,"jwksUri":self.jwks,"tokenEndpoint":self.token})
    }
    pub async fn verify(&self, token: &str) -> Result<Identity> {
        ensure!(!self.issuer.is_empty(), "Catalog authentication disabled");
        let header = decode_header(token)?;
        ensure!(header.alg == Algorithm::RS256, "RS256 required");
        let kid = header.kid.ok_or_else(|| anyhow::anyhow!("Missing kid"))?;
        let mut cache = self.cache.lock().await;
        let elapsed = cache.fetched.map(|i| i.elapsed()).unwrap_or(Duration::MAX);
        if elapsed >= Duration::from_secs(300)
            || (!cache.keys.contains_key(&kid) && elapsed >= Duration::from_secs(30))
        {
            ensure!(
                cache
                    .attempted
                    .is_none_or(|i| i.elapsed() >= Duration::from_secs(30)),
                "JWKS refresh cooling down"
            );
            cache.attempted = Some(Instant::now());
            let response = self
                .http
                .get(format!(
                    "{}/.well-known/openid-configuration",
                    self.issuer.trim_end_matches('/')
                ))
                .send()
                .await?
                .error_for_status()?;
            let m: Value = temba_core::http_json(response).await?;
            ensure!(
                m["issuer"].as_str() == Some(&self.issuer),
                "Issuer mismatch"
            );
            let uri = if self.jwks.is_empty() {
                m["jwks_uri"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Missing JWKS"))?
            } else {
                &self.jwks
            };
            https_override(uri, &self.issuer)?;
            let response = self.http.get(uri).send().await?.error_for_status()?;
            let jwks: JwkSet = temba_core::http_json(response).await?;
            ensure!(jwks.keys.len() <= 64, "Too many signing keys");
            cache.keys = jwks
                .keys
                .iter()
                .filter_map(|j| {
                    j.common
                        .key_id
                        .clone()
                        .and_then(|id| DecodingKey::from_jwk(j).ok().map(|k| (id, k)))
                })
                .collect();
            cache.fetched = Some(Instant::now());
        }
        let key = cache
            .keys
            .get(&kid)
            .ok_or_else(|| anyhow::anyhow!("Unknown key"))?;
        let mut v = Validation::new(Algorithm::RS256);
        v.set_issuer(&[&self.issuer]);
        v.set_audience(&[&self.audience]);
        v.validate_nbf = true;
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<Claims>(token, key, &v)?.claims;
        ensure!(
            !claims.sub.is_empty() && claims.typ == "Bearer",
            "Access token required"
        );
        ensure!(
            claims.groups.len() <= 256 && claims.groups.iter().all(|g| g.len() <= 1024),
            "Invalid groups claim"
        );
        Ok(Identity {
            groups: claims.groups,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use std::time::{SystemTime, UNIX_EPOCH};
    #[test]
    fn rejection_diagnostics_do_not_include_tokens_or_provider_errors() {
        let error = anyhow::Error::new(jsonwebtoken::errors::Error::from(
            jsonwebtoken::errors::ErrorKind::InvalidAudience,
        ));
        assert_eq!(rejection_reason(&error), "invalid audience");
        assert_eq!(
            rejection_reason(&anyhow::anyhow!("provider secret-token-value")),
            "token verification failed"
        );
    }
    pub(crate) fn verifier() -> Verifier {
        let mut keys = HashMap::new();
        keys.insert(
            "test".into(),
            DecodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/test-only-public.pem"))
                .unwrap(),
        );
        Verifier {
            issuer: "https://auth.example/realm".into(),
            audience: "temba-mcp".into(),
            jwks: String::new(),
            token: String::new(),
            client_id: "temba".into(),
            callback_port: 19877,
            http: reqwest::Client::new(),
            cache: Mutex::new(Cache {
                keys,
                fetched: Some(Instant::now()),
                attempted: None,
            }),
        }
    }
    pub(crate) fn signed(claims: Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test".into());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/test-only-private.pem"))
                .unwrap(),
        )
        .unwrap()
    }
    #[tokio::test]
    async fn access_token_needs_no_custom_scope_and_rejects_wrong_identity() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({"iss":"https://auth.example/realm","aud":"temba-mcp","sub":"user","typ":"Bearer","exp":now+300});
        let v = verifier();
        assert!(v.verify(&signed(claims.clone())).await.is_ok());
        for (field, value) in [
            ("aud", json!("other-service")),
            ("iss", json!("https://other.example")),
            ("typ", json!("ID")),
            ("sub", json!("")),
            ("exp", json!(now - 300)),
            ("nbf", json!(now + 300)),
        ] {
            let mut bad = claims.clone();
            bad[field] = value;
            assert!(
                v.verify(&signed(bad)).await.is_err(),
                "accepted invalid {field}"
            );
        }
    }
    #[tokio::test]
    async fn groups_are_taken_only_from_the_verified_access_token() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut claims = json!({"iss":"https://auth.example/realm","aud":"temba-mcp","sub":"user","typ":"Bearer","exp":now+300});
        let v = verifier();
        assert!(
            v.verify(&signed(claims.clone()))
                .await
                .unwrap()
                .groups
                .is_empty()
        );
        claims["groups"] = json!(["/X", "/Y"]);
        assert_eq!(
            v.verify(&signed(claims.clone())).await.unwrap().groups,
            vec!["/X", "/Y"]
        );
        claims["groups"] = json!("/X");
        assert!(v.verify(&signed(claims)).await.is_err());
    }
    #[tokio::test]
    async fn failed_jwks_refresh_does_not_extend_old_key_lifetime() {
        let v = verifier();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = signed(
            json!({"iss":"https://auth.example/realm","aud":"temba-mcp","sub":"user","typ":"Bearer","exp":now+300}),
        );
        {
            let mut cache = v.cache.lock().await;
            cache.fetched = Some(Instant::now() - Duration::from_secs(301));
            cache.attempted = Some(Instant::now());
        }
        assert!(v.verify(&token).await.is_err());
    }
    #[tokio::test]
    async fn algorithm_confusion_is_rejected() {
        let token = encode(
            &Header::new(Algorithm::HS256),
            &json!({"sub":"user"}),
            &EncodingKey::from_secret(b"test"),
        )
        .unwrap();
        assert!(verifier().verify(&token).await.is_err());
    }
}
