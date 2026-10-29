//! OAuth credentials live only in this process. No disk or operating-system store.
use anyhow::{Result, bail, ensure};
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, oneshot},
};
use url::Url;
use zeroize::Zeroizing;

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogAuth {
    pub issuer: String,
    pub client_id: String,
    pub callback_port: u16,
    #[serde(default)]
    pub token_endpoint: String,
}
impl CatalogAuth {
    pub fn validate(&self) -> Result<()> {
        temba_core::endpoint(&self.issuer, false)?;
        ensure!(
            !self.client_id.is_empty(),
            "Catalog OAuth clientId required"
        );
        if !self.token_endpoint.is_empty() {
            temba_core::https_override(&self.token_endpoint, &self.issuer)?;
        }
        Ok(())
    }

    pub async fn pending(&self, http: &reqwest::Client) -> Result<Pending> {
        self.validate()?;
        let provider = metadata_override(
            http,
            &self.issuer,
            std::slice::from_ref(&self.issuer),
            false,
            (!self.token_endpoint.is_empty()).then_some(self.token_endpoint.as_str()),
        )
        .await?;
        Pending::new(
            http.clone(),
            provider,
            Some(self.client_id.clone()),
            vec!["openid".into()],
            None,
            self.callback_port,
        )
        .await
    }
}
#[derive(Clone)]
pub struct Provider {
    pub issuer: String,
    pub authorize: String,
    pub token: String,
    pub registration: Option<String>,
    pub require_iss: bool,
}
#[derive(Debug)]
pub struct OriginNotAllowed(pub String);
impl std::fmt::Display for OriginNotAllowed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "OAuth origin {} is not approved. Add it to this MCP definition's oauth.allowedOrigins. This check is independent of Temba catalog authentication.",
            self.0
        )
    }
}
impl std::error::Error for OriginNotAllowed {}
#[cfg(test)]
fn trusted(value: &str, origins: &[String], allow_http: bool) -> Result<Url> {
    super::oauth_network::Policy::restricted(origins, allow_http).url(value)
}
pub async fn metadata_override(
    http: &reqwest::Client,
    issuer: &str,
    origins: &[String],
    allow_http: bool,
    token_override: Option<&str>,
) -> Result<Provider> {
    let policy = super::oauth_network::Policy::restricted(origins, allow_http);
    metadata_with_policy(http, issuer, &policy, token_override).await
}
async fn metadata_with_policy(
    http: &reqwest::Client,
    issuer: &str,
    policy: &super::oauth_network::Policy,
    token_override: Option<&str>,
) -> Result<Provider> {
    let u = policy.validate(issuer).await?;
    let path = u.path().trim_end_matches('/');
    let candidates = [
        format!(
            "{}/.well-known/oauth-authorization-server{}",
            u.origin().ascii_serialization(),
            path
        ),
        format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        ),
    ];
    let mut last_network_error = None;
    for uri in candidates {
        let r = match http.get(uri).timeout(Duration::from_secs(15)).send().await {
            Ok(response) => response,
            Err(error) => {
                last_network_error = Some(error);
                continue;
            }
        };
        if !r.status().is_success() {
            continue;
        }
        let m: Value = temba_core::http_json(r).await?;
        ensure!(
            m["issuer"].as_str() == Some(issuer),
            "OAuth issuer mismatch"
        );
        ensure!(
            m["code_challenge_methods_supported"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "S256")),
            "OAuth server must support PKCE S256"
        );
        let field = |name| -> Result<String> {
            let s = if name == "token_endpoint" {
                token_override.or_else(|| m[name].as_str())
            } else {
                m[name].as_str()
            }
            .ok_or_else(|| anyhow::anyhow!("Missing OAuth metadata"))?;
            policy.url(s)?;
            Ok(s.to_owned())
        };
        let authorize = field("authorization_endpoint")?;
        let token = field("token_endpoint")?;
        policy.validate(&authorize).await?;
        policy.validate(&token).await?;
        let registration = match m["registration_endpoint"].as_str() {
            Some(uri) if policy.validate(uri).await.is_ok() => Some(uri.to_owned()),
            _ => None,
        };
        return Ok(Provider {
            issuer: issuer.to_owned(),
            authorize,
            token,
            // An unusable DCR endpoint must not break a pre-registered client.
            // Never retain or call a registration URL rejected by the selected policy.
            registration,
            require_iss: m["authorization_response_iss_parameter_supported"]
                .as_bool()
                .unwrap_or(false),
        });
    }
    if let Some(error) = last_network_error {
        return Err(error.into());
    }
    bail!("OAuth metadata unavailable")
}
#[cfg(test)]
async fn discover(
    http: &reqwest::Client,
    resource: &str,
    origins: &[String],
    allow_http: bool,
    headers: reqwest::header::HeaderMap,
) -> Result<Provider> {
    let policy = super::oauth_network::Policy::restricted(origins, allow_http);
    discover_with_policy(http, resource, &policy, headers).await
}

pub async fn discover_remote(
    http: &reqwest::Client,
    resource: &str,
    allowed_origins: &[String],
    allow_http: bool,
    headers: reqwest::header::HeaderMap,
) -> Result<(Provider, reqwest::Client)> {
    let policy = if allowed_origins.is_empty() {
        super::oauth_network::Policy::automatic(resource, allow_http)?
    } else {
        let mut origins = allowed_origins.to_vec();
        origins.push(resource.to_owned());
        super::oauth_network::Policy::restricted(&origins, allow_http)
    };
    let oauth_http = policy.client(http)?;
    let provider = discover_with_policy(&oauth_http, resource, &policy, headers).await?;
    Ok((provider, oauth_http))
}

async fn discover_with_policy(
    http: &reqwest::Client,
    resource: &str,
    policy: &super::oauth_network::Policy,
    headers: reqwest::header::HeaderMap,
) -> Result<Provider> {
    let u = policy.url(resource)?;
    // No OAuth bearer yet. Extra headers are bound to this exact MCP resource only.
    let r = http
        .get(resource)
        .headers(headers)
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    let challenge = r
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let hinted = challenge
        .split("resource_metadata=\"")
        .nth(1)
        .and_then(|v| v.split('"').next());
    let uri = hinted.map(str::to_owned).unwrap_or_else(|| {
        format!(
            "{}/.well-known/oauth-protected-resource{}",
            u.origin().ascii_serialization(),
            u.path()
        )
    });
    policy.validate(&uri).await?;
    let r = http
        .get(&uri)
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    if r.status().is_success() {
        let m: Value = temba_core::http_json(r).await?;
        let declared = m["resource"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing OAuth resource identifier"))?;
        let d = policy.url(declared)?;
        ensure!(
            d.origin() == u.origin() && d.path() == u.path(),
            "OAuth metadata describes another resource"
        );
        let issuer = m["authorization_servers"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing authorization server"))?;
        return metadata_with_policy(http, issuer, policy, None).await;
    }
    metadata_with_policy(http, &u.origin().ascii_serialization(), policy, None).await
}
struct Tokens {
    access: Zeroizing<String>,
    refresh: Option<Zeroizing<String>>,
    received: Instant,
    lifetime: Option<Duration>,
}
pub struct Session {
    http: reqwest::Client,
    provider: Provider,
    client_id: String,
    resource: Option<String>,
    tokens: Mutex<Option<Tokens>>,
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}
fn margin(lifetime: Duration) -> Duration {
    Duration::from_secs(60).min(lifetime / 10)
}
impl Session {
    #[cfg(test)]
    pub(crate) fn test_session(token: &str) -> Arc<Self> {
        Arc::new(Self {
            http: reqwest::Client::new(),
            provider: Provider {
                issuer: "https://test.invalid".into(),
                authorize: "https://test.invalid".into(),
                token: "https://test.invalid".into(),
                registration: None,
                require_iss: false,
            },
            client_id: "test".into(),
            resource: None,
            tokens: Mutex::new(Some(Tokens {
                access: Zeroizing::new(token.into()),
                refresh: None,
                received: Instant::now(),
                lifetime: Some(Duration::from_secs(3600)),
            })),
        })
    }

    pub async fn access(&self) -> Result<Zeroizing<String>> {
        let mut guard = self.tokens.lock().await;
        let t = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Reconnection required"))?;
        if t.lifetime
            .is_some_and(|d| t.received.elapsed() + margin(d) >= d)
        {
            let refresh = t
                .refresh
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Token expired; reconnect"))?;
            let mut form = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.as_str()),
                ("client_id", &self.client_id),
            ];
            if let Some(resource) = &self.resource {
                form.push(("resource", resource));
            }
            let r = self
                .http
                .post(&self.provider.token)
                .form(&form)
                .timeout(Duration::from_secs(20))
                .send()
                .await?;
            if !r.status().is_success() {
                if r.status() == StatusCode::BAD_REQUEST {
                    let body: Value = temba_core::http_json(r).await?;
                    if body["error"] == "invalid_grant" {
                        *guard = None;
                    }
                }
                bail!("OAuth refresh failed; reconnect if authorization was revoked")
            }
            let mut new = parse_tokens(r).await?;
            if new.refresh.is_none() {
                new.refresh = guard.as_mut().and_then(|old| old.refresh.take());
            }
            *guard = Some(new);
        }
        Ok(Zeroizing::new(guard.as_ref().unwrap().access.to_string()))
    }
    pub async fn clear(&self) {
        self.tokens.lock().await.take();
    }
}
async fn parse_tokens(r: reqwest::Response) -> Result<Tokens> {
    // Zeroize our owned response buffer; serde/HTTP temporary copies cannot be guaranteed wiped.
    let bytes = temba_core::http_bytes(r).await?;
    let t: TokenResponse = serde_json::from_slice(&bytes)?;
    ensure!(
        t.token_type.eq_ignore_ascii_case("bearer")
            && !t.access_token.is_empty()
            && t.expires_in != Some(0),
        "Invalid OAuth token response"
    );
    Ok(Tokens {
        access: Zeroizing::new(t.access_token),
        refresh: t.refresh_token.map(Zeroizing::new),
        received: Instant::now(),
        lifetime: t.expires_in.map(Duration::from_secs),
    })
}
fn random() -> Zeroizing<String> {
    Zeroizing::new(format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ))
}
struct Callback {
    code: Zeroizing<String>,
    iss: Option<String>,
    reply: oneshot::Sender<bool>,
}
struct CallbackState {
    state: Zeroizing<String>,
    ticket: Zeroizing<String>,
    authorize: String,
    sender: Mutex<Option<oneshot::Sender<Callback>>>,
}
#[derive(Deserialize)]
struct Params {
    code: Option<String>,
    state: Option<String>,
    iss: Option<String>,
    t: Option<String>,
}
async fn start(State(s): State<Arc<CallbackState>>, Query(p): Query<Params>) -> Response {
    if p.t.as_deref() != Some(s.ticket.as_str()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    Redirect::temporary(&s.authorize).into_response()
}
// Only fixed application messages are rendered; no OAuth query/provider data.
fn authorization_page(title: &str, message: &str, success: bool) -> Html<String> {
    Html(
        include_str!("../assets/oauth-result.html")
            .replace("{{TITLE}}", title)
            .replace("{{MESSAGE}}", message)
            .replace("{{CLASS}}", if success { "success" } else { "error" })
            .replace(
                "{{ICON}}",
                if success {
                    "<path d=\"m5 12 4 4 10-10\"/>"
                } else {
                    "<path d=\"M12 5v8M12 17v.5\"/>"
                },
            ),
    )
}
async fn callback(State(s): State<Arc<CallbackState>>, Query(p): Query<Params>) -> Response {
    if p.state.as_deref() != Some(s.state.as_str()) || p.code.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            authorization_page("Unable to authorize", "This authorization link is invalid, expired, or access was denied. Return to your AI client and start a new connection.", false),
        )
            .into_response();
    }
    let Some(tx) = s.sender.lock().await.take() else {
        return (StatusCode::BAD_REQUEST, authorization_page("Link already used", "This authorization link has already been used. Return to your AI client to continue or start a new connection.", false)).into_response();
    };
    let (reply, rx) = oneshot::channel();
    if tx
        .send(Callback {
            code: Zeroizing::new(p.code.unwrap()),
            iss: p.iss,
            reply,
        })
        .is_err()
    {
        return (StatusCode::GONE, authorization_page("Connection expired", "This connection request is no longer active. Return to your AI client and start a new connection.", false)).into_response();
    }
    match tokio::time::timeout(Duration::from_secs(45), rx).await {
        Ok(Ok(true)) => {
            authorization_page("Authentication complete", "You have successfully authorized this connection. Return to your AI client to continue.", true).into_response()
        }
        _ => (
            StatusCode::BAD_GATEWAY,
            authorization_page("Connection unsuccessful", "Authentication could not be completed. Return to your AI client and try connecting again.", false),
        )
            .into_response(),
    }
}
pub struct Pending {
    pub url: String,
    http: reqwest::Client,
    provider: Provider,
    client_id: String,
    redirect: String,
    verifier: Zeroizing<String>,
    resource: Option<String>,
    receiver: oneshot::Receiver<Callback>,
    server: tokio::task::JoinHandle<()>,
    shutdown: Option<oneshot::Sender<()>>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.server.abort();
    }
}
impl Pending {
    pub async fn new(
        http: reqwest::Client,
        provider: Provider,
        client_id: Option<String>,
        scopes: Vec<String>,
        resource: Option<String>,
        port: u16,
    ) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let redirect = format!(
            "http://127.0.0.1:{}/oauth/callback",
            listener.local_addr()?.port()
        );
        let client_id = match client_id {
            Some(id) => id,
            None => {
                let registration = provider.registration.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("Provider requires a pre-registered clientId in the catalog")
                })?;
                let r:Value=http.post(registration).json(&json!({"client_name":"Temba Local","redirect_uris":[redirect],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none","application_type":"native"})).timeout(Duration::from_secs(20)).send().await?.error_for_status()?.json().await?;
                ensure!(
                    r["token_endpoint_auth_method"].as_str().unwrap_or("none") == "none",
                    "A public OAuth client is required"
                );
                r["client_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Registration missing client_id"))?
                    .to_owned()
            }
        };
        let verifier = random();
        let state = random();
        let ticket = random();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut authorize = Url::parse(&provider.authorize)?;
        {
            let mut q = authorize.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("client_id", &client_id)
                .append_pair("redirect_uri", &redirect)
                .append_pair("state", &state)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256");
            if !scopes.is_empty() {
                q.append_pair("scope", &scopes.join(" "));
            }
            if let Some(r) = &resource {
                q.append_pair("resource", r);
            }
        }
        let url = format!(
            "http://127.0.0.1:{}/start?t={}",
            listener.local_addr()?.port(),
            ticket.as_str()
        );
        let (tx, receiver) = oneshot::channel();
        let (shutdown, closed) = oneshot::channel();
        let s = Arc::new(CallbackState {
            state,
            ticket,
            authorize: authorize.to_string(),
            sender: Mutex::new(Some(tx)),
        });
        let router = Router::new()
            .route("/start", get(start))
            .route("/oauth/callback", get(callback))
            .with_state(s);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = closed.await;
                })
                .await;
        });
        Ok(Self {
            url,
            http,
            provider,
            client_id,
            redirect,
            verifier,
            resource,
            receiver,
            server,
            shutdown: Some(shutdown),
        })
    }
    pub async fn finish(mut self) -> Result<Arc<Session>> {
        let cb = tokio::time::timeout(Duration::from_secs(600), &mut self.receiver).await??;
        let result = async {
            ensure!(
                (!self.provider.require_iss || cb.iss.is_some())
                    && cb.iss.as_ref().is_none_or(|s| s == &self.provider.issuer),
                "Callback issuer mismatch"
            );
            let mut form = vec![
                ("grant_type", "authorization_code"),
                ("client_id", self.client_id.as_str()),
                ("redirect_uri", &self.redirect),
                ("code", cb.code.as_str()),
                ("code_verifier", self.verifier.as_str()),
            ];
            if let Some(r) = &self.resource {
                form.push(("resource", r));
            }
            let r = self
                .http
                .post(&self.provider.token)
                .form(&form)
                .timeout(Duration::from_secs(20))
                .send()
                .await?
                .error_for_status()?;
            let tokens = parse_tokens(r).await?;
            Ok(Arc::new(Session {
                http: self.http.clone(),
                provider: self.provider.clone(),
                client_id: self.client_id.clone(),
                resource: self.resource.clone(),
                tokens: Mutex::new(Some(tokens)),
            }))
        }
        .await;
        let _ = cb.reply.send(result.is_ok());
        // Let the browser receive its final response before closing the callback listener.
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.server).await;
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_lived_tokens() {
        assert_eq!(margin(Duration::from_secs(30)), Duration::from_secs(3));
        assert_eq!(margin(Duration::from_secs(3600)), Duration::from_secs(60));
    }
    #[test]
    fn oauth_origin_boundary() {
        assert!(
            trusted(
                "https://evil.example/token",
                &["https://auth.example".into()],
                false
            )
            .is_err()
        );
        assert!(
            trusted(
                "https://auth.example/token",
                &["https://auth.example".into()],
                false
            )
            .is_ok()
        );
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use axum::extract::Form;
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicUsize, Ordering},
    };
    struct Fixture {
        exchanges: AtomicUsize,
        refreshes: AtomicUsize,
        challenge: Mutex<String>,
    }
    async fn token(
        State(f): State<Arc<Fixture>>,
        Form(p): Form<HashMap<String, String>>,
    ) -> Json<Value> {
        assert_eq!(p["client_id"], "native-client");
        assert_eq!(p["resource"], "https://mcp.example/mcp");
        match p["grant_type"].as_str() {
            "authorization_code" => {
                assert_eq!(p["code"], "valid-code");
                assert_eq!(
                    URL_SAFE_NO_PAD.encode(Sha256::digest(p["code_verifier"].as_bytes())),
                    *f.challenge.lock().await
                );
                f.exchanges.fetch_add(1, Ordering::SeqCst);
                Json(
                    json!({"access_token":"first-token","token_type":"Bearer","refresh_token":"first-refresh","expires_in":30}),
                )
            }
            "refresh_token" => {
                let n = f.refreshes.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    p["refresh_token"],
                    if n == 0 {
                        "first-refresh"
                    } else {
                        "rotated-refresh"
                    }
                );
                Json(
                    json!({"access_token":"new-token","token_type":"Bearer","refresh_token":"rotated-refresh","expires_in":30}),
                )
            }
            _ => panic!("unexpected grant"),
        }
    }
    use axum::Json;
    #[tokio::test]
    async fn pkce_callback_and_serialized_rotating_refresh() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let fixture = Arc::new(Fixture {
            exchanges: AtomicUsize::new(0),
            refreshes: AtomicUsize::new(0),
            challenge: Mutex::new(String::new()),
        });
        let router = Router::new()
            .route("/token", axum::routing::post(token))
            .with_state(fixture.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let p = Provider {
            issuer: issuer.clone(),
            authorize: format!("{issuer}/authorize"),
            token: format!("{issuer}/token"),
            registration: None,
            require_iss: false,
        };
        let pending = Pending::new(
            http.clone(),
            p,
            Some("native-client".into()),
            vec!["openid".into()],
            Some("https://mcp.example/mcp".into()),
            0,
        )
        .await
        .unwrap();
        let response = http.get(&pending.url).send().await.unwrap();
        assert!(response.status().is_redirection());
        let authorize = Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
        let parameters: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
        assert_eq!(parameters["code_challenge_method"], "S256");
        assert_eq!(parameters["scope"], "openid");
        *fixture.challenge.lock().await = parameters["code_challenge"].clone();
        let mut callback = Url::parse(&parameters["redirect_uri"]).unwrap();
        callback
            .query_pairs_mut()
            .append_pair("code", "valid-code")
            .append_pair("state", "wrong");
        assert_eq!(
            http.get(callback.clone()).send().await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        let flow = tokio::spawn(pending.finish());
        callback.set_query(None);
        callback
            .query_pairs_mut()
            .append_pair("code", "valid-code")
            .append_pair("state", &parameters["state"])
            .append_pair("iss", &issuer);
        assert_eq!(
            http.get(callback.clone()).send().await.unwrap().status(),
            StatusCode::OK
        );
        let session = flow.await.unwrap().unwrap();
        assert_eq!(fixture.exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(session.access().await.unwrap().as_str(), "first-token");
        assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 0);
        session.tokens.lock().await.as_mut().unwrap().received =
            Instant::now() - Duration::from_secs(30);
        let values = futures::future::join_all((0..10).map(|_| session.access())).await;
        assert!(
            values
                .into_iter()
                .all(|v| v.unwrap().as_str() == "new-token")
        );
        assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 1);
        session.tokens.lock().await.as_mut().unwrap().received =
            Instant::now() - Duration::from_secs(30);
        session.access().await.unwrap();
        assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 2);
        session.clear().await;
        assert!(session.access().await.is_err());
        let replay = http.get(callback).send().await;
        assert!(replay.is_err() || replay.unwrap().status() != StatusCode::OK);
        server.abort();
    }
    #[tokio::test]
    async fn resource_metadata_requires_explicit_external_origin() {
        let al = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth = format!("http://{}", al.local_addr().unwrap());
        let issuer = format!("{auth}/tenant");
        let metadata = json!({"issuer":issuer,"authorization_endpoint":format!("{auth}/authorize"),"token_endpoint":format!("{auth}/token"),"registration_endpoint":format!("{auth}/register"),"code_challenge_methods_supported":["S256"]});
        let ar = Router::new().route(
            "/.well-known/oauth-authorization-server/tenant",
            get(move |headers: axum::http::HeaderMap| {
                let m = metadata.clone();
                async move {
                    assert!(!headers.contains_key("x-api-key"));
                    assert!(!headers.contains_key("x-tenant"));
                    Json(m)
                }
            }),
        );
        let at = tokio::spawn(async move { axum::serve(al, ar).await.unwrap() });
        let rl = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resource = format!("http://{}/mcp", rl.local_addr().unwrap());
        let rm = json!({"resource":resource,"authorization_servers":[issuer]});
        let rr = Router::new()
            .route(
                "/mcp",
                get(|headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["x-api-key"], "local-api-key");
                    assert_eq!(headers["x-tenant"], "local-tenant");
                    StatusCode::UNAUTHORIZED
                }),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(move |headers: axum::http::HeaderMap| {
                    let m = rm.clone();
                    async move {
                        assert!(!headers.contains_key("x-api-key"));
                        assert!(!headers.contains_key("x-tenant"));
                        Json(m)
                    }
                }),
            );
        let rt = tokio::spawn(async move { axum::serve(rl, rr).await.unwrap() });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-api-key",
            reqwest::header::HeaderValue::from_static("local-api-key"),
        );
        headers.insert(
            "x-tenant",
            reqwest::header::HeaderValue::from_static("local-tenant"),
        );
        let error = discover(
            &http,
            &resource,
            std::slice::from_ref(&resource),
            true,
            headers.clone(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.downcast_ref::<OriginNotAllowed>().unwrap().0, auth);
        assert!(error.to_string().contains("oauth.allowedOrigins"));
        let p = discover(
            &http,
            &resource,
            &[resource.clone(), auth.clone()],
            true,
            headers.clone(),
        )
        .await
        .unwrap();
        assert_eq!(p.issuer, issuer);
        assert_eq!(p.token, format!("{auth}/token"));
        at.abort();
        rt.abort();
    }
    #[tokio::test]
    async fn automatic_discovery_keeps_resource_origin_and_blocks_private_provider_endpoints() {
        for token_destination in [
            None,
            Some("https://10.0.0.1/token"),
            Some("https://localhost/token"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let resource = format!("{base}/mcp");
            let resource_metadata = json!({"resource":resource,"authorization_servers":[base]});
            let metadata = json!({
                "issuer":base,"authorization_endpoint":format!("{base}/authorize"),
                "token_endpoint":token_destination.map(str::to_owned).unwrap_or_else(|| format!("{base}/token")),
                "registration_endpoint":format!("{base}/register"),
                "code_challenge_methods_supported":["S256"]
            });
            let router = Router::new()
                .route("/mcp", get(|| async { StatusCode::UNAUTHORIZED }))
                .route(
                    "/.well-known/oauth-protected-resource/mcp",
                    get(move || {
                        let m = resource_metadata.clone();
                        async move { Json(m) }
                    }),
                )
                .route(
                    "/.well-known/oauth-authorization-server",
                    get(move || {
                        let m = metadata.clone();
                        async move { Json(m) }
                    }),
                );
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let result = discover_remote(
                &reqwest::Client::new(),
                &resource,
                &[],
                true,
                Default::default(),
            )
            .await;
            if token_destination.is_none() {
                let (provider, _) = result.unwrap();
                assert_eq!(provider.issuer, base);
                assert_eq!(provider.token, format!("{base}/token"));
            } else {
                assert!(result.is_err());
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn endpoint_override_precedes_discovery_validation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let metadata = json!({"issuer":base,"authorization_endpoint":format!("{base}/authorize"),"token_endpoint":"http://internal.invalid/token","registration_endpoint":"http://internal.invalid/register","authorization_response_iss_parameter_supported":true,"code_challenge_methods_supported":["S256"]});
        let router = Router::new().route(
            "/.well-known/openid-configuration",
            get(move || {
                let m = metadata.clone();
                async move { Json(m) }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let origins = vec![base.clone()];
        assert!(
            metadata_override(&http, &base, &origins, true, None)
                .await
                .is_err()
        );
        let override_uri = format!("{base}/token");
        let p = metadata_override(&http, &base, &origins, true, Some(&override_uri))
            .await
            .unwrap();
        assert_eq!(p.token, override_uri);
        assert!(p.registration.is_none());
        assert!(p.require_iss);
        server.abort();
    }
    #[tokio::test]
    async fn rejected_refresh_clears_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/token",
                    axum::routing::post(|| async {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error":"invalid_grant"})),
                        )
                    }),
                ),
            )
            .await
            .unwrap()
        });
        let s = Session {
            http: reqwest::Client::new(),
            provider: Provider {
                issuer: base.clone(),
                authorize: base.clone(),
                token: format!("{base}/token"),
                registration: None,
                require_iss: false,
            },
            client_id: "id".into(),
            resource: None,
            tokens: Mutex::new(Some(Tokens {
                access: Zeroizing::new("expired".into()),
                refresh: Some(Zeroizing::new("rejected".into())),
                received: Instant::now() - Duration::from_secs(30),
                lifetime: Some(Duration::from_secs(30)),
            })),
        };
        assert!(s.access().await.is_err());
        assert!(s.tokens.lock().await.is_none());
        server.abort();
    }
}
