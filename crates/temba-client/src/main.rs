mod config;
mod local_catalog;
mod oauth;
mod search;
mod transport;
use anyhow::{Result, ensure};
use clap::Parser;
use rmcp::{
    ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt,
    model::*,
    service::{RequestContext, RunningService},
    transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use temba_core::{Catalog, Server};
use tokio::sync::{Mutex, RwLock};
use zeroize::Zeroizing;
#[derive(Parser)]
#[command(
    version,
    about = "Local stdio MCP; upstream credentials stay in this process"
)]
struct Args {
    #[arg(long, env = "TEMBA_CONFIG", default_value = "temba.toml")]
    config: String,
}
struct Entry {
    retired: AtomicBool,
    definition: Server,
    personal: Option<local_catalog::Personal>,
    state: RwLock<Remote>,
    operation: Mutex<()>,
    login: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
struct Remote {
    status: &'static str,
    tools: Vec<Tool>,
    connection: Option<RunningService<RoleClient, ()>>,
    session: Option<Arc<oauth::Session>>,
}
struct Local {
    config: config::Config,
    http: reqwest::Client,
    servers: RwLock<BTreeMap<String, Arc<Entry>>>,
    personal: RwLock<BTreeMap<String, Arc<Entry>>>,
    etag: Mutex<Option<String>>,
    catalog_session: Mutex<Option<Arc<oauth::Session>>>,
    catalog_login: Mutex<Option<tokio::task::JoinHandle<()>>>,
    catalog_auth: RwLock<Option<oauth::CatalogAuth>>,
    catalog_status: RwLock<&'static str>,
    refresh: Mutex<()>,
}
#[derive(Debug)]
struct MissingHeaders(Vec<temba_core::Header>);
impl std::fmt::Display for MissingHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Mandatory local headers missing: {}. Configure their environment variables in the local TOML and restart the client; do not provide secrets in chat.",
            self.0
                .iter()
                .map(|h| format!(
                    "{} ({})",
                    h.name,
                    h.credential.as_deref().unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}
impl std::error::Error for MissingHeaders {}
#[derive(Clone)]
struct Handler(Arc<Local>);
fn result(v: Value) -> CallToolResponse {
    CallToolResult::success(vec![ContentBlock::text(v.to_string())]).into()
}
fn failure(message: &str) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(message.to_owned())]).into()
}
fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()?)
}
impl Entry {
    fn new(definition: Server) -> Self {
        let status = if definition.oauth.is_some() {
            "auth_required"
        } else {
            "connecting"
        };
        Self {
            retired: AtomicBool::new(false),
            definition,
            personal: None,
            state: RwLock::new(Remote {
                status,
                tools: vec![],
                connection: None,
                session: None,
            }),
            operation: Mutex::new(()),
            login: Mutex::new(None),
        }
    }
    fn label(&self) -> &str {
        self.personal
            .as_ref()
            .map_or(self.definition.id.as_str(), |p| p.label.as_str())
    }
    async fn close(&self) {
        if let Some(task) = self.login.lock().await.take() {
            task.abort();
        }
        let _lock = self.operation.lock().await;
        let mut s = self.state.write().await;
        if let Some(session) = s.session.take() {
            session.clear().await;
        }
        if let Some(connection) = s.connection.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), connection.cancel()).await;
        }
        s.tools.clear();
        s.status = "disconnected";
        if let Some(personal) = &self.personal {
            personal.headers.lock().unwrap().clear();
        }
    }
}
impl Local {
    async fn bootstrap(&self) -> Result<()> {
        let r = self
            .http
            .get(format!("{}/v1/bootstrap", self.config.catalog_url))
            .timeout(Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?;
        let r: Value = temba_core::http_json(r).await?;
        ensure!(r["schemaVersion"] == 2, "Unsupported bootstrap version");
        let auth: Option<oauth::CatalogAuth> = serde_json::from_value(r["catalogueAuth"].clone())?;
        if let Some(a) = &auth {
            temba_core::endpoint(&a.issuer, false)?;
            ensure!(!a.client_id.is_empty(), "Catalog OAuth clientId required");
            if !a.token_endpoint.is_empty() {
                temba_core::https_override(&a.token_endpoint, &a.issuer)?;
            }
        }
        let mut status = self.catalog_status.write().await;
        if auth.is_none() {
            *status = "public";
        } else if *status == "public" {
            *status = "auth_required";
        }
        *self.catalog_auth.write().await = auth;
        Ok(())
    }
    async fn refresh(self: &Arc<Self>) -> Result<()> {
        let _lock = self.refresh.lock().await;
        let mut req = self
            .http
            .get(format!("{}/v1/catalog", self.config.catalog_url))
            .timeout(Duration::from_secs(20));
        if let Some(etag) = self.etag.lock().await.as_ref() {
            req = req.header("If-None-Match", etag);
        }
        let catalog_session = self.catalog_session.lock().await.clone();
        if let Some(session) = catalog_session {
            match session.access().await {
                Ok(token) => req = req.bearer_auth(token.as_str()),
                Err(_) => {
                    session.clear().await;
                    self.catalog_session.lock().await.take();
                    *self.catalog_status.write().await = "auth_required";
                    self.drop_authenticated().await;
                    return Err(anyhow::anyhow!("Catalog reconnection required"));
                }
            }
        }
        let response = req.send().await?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            if self.catalog_session.lock().await.is_some() {
                *self.catalog_status.write().await = "authenticated";
            }
            return Ok(());
        }
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(session) = self.catalog_session.lock().await.take() {
                session.clear().await;
            }
            *self.etag.lock().await = None;
            *self.catalog_status.write().await = "auth_required";
            self.drop_authenticated().await;
            bail_catalog();
            return Err(anyhow::anyhow!("Catalog reconnection required"));
        }
        let response = response.error_for_status()?;
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut catalog: Catalog = temba_core::http_json(response).await?;
        catalog.validate(self.config.allow_loopback_http)?;
        catalog
            .servers
            .retain(|s| s.labels.iter().all(|l| self.config.labels.contains(l)));
        let old = self.servers.read().await.clone();
        let mut next = BTreeMap::new();
        let mut connect = vec![];
        for definition in catalog.servers {
            let e = if let Some(existing) = old
                .get(&definition.id)
                .filter(|e| e.definition == definition)
            {
                existing.clone()
            } else {
                let e = Arc::new(Entry::new(definition));
                if let Err(error) = self.credential_headers(&e) {
                    e.state.write().await.status = if error.is::<MissingHeaders>() {
                        "credentials_required"
                    } else {
                        "connection_failed"
                    };
                    eprintln!("MCP {}: {}", e.label(), setup_error(&error));
                } else if e.definition.oauth.is_none() {
                    connect.push(e.clone());
                }
                e
            };
            next.insert(e.definition.id.clone(), e);
        }
        let removed: Vec<_> = old
            .iter()
            .filter(|(id, e)| next.get(*id).is_none_or(|n| !Arc::ptr_eq(n, e)))
            .map(|(_, e)| e.clone())
            .collect();
        *self.servers.write().await = next;
        *self.etag.lock().await = etag;
        for e in removed {
            e.retired.store(true, Ordering::SeqCst);
            e.close().await;
        }
        if self.catalog_session.lock().await.is_some() {
            *self.catalog_status.write().await = "authenticated";
        }
        self.log_progress("Catalog: visible MCPs loaded").await;
        let local = self.clone();
        tokio::spawn(async move {
            use futures::{StreamExt, stream};
            let mut pending = stream::iter(connect)
                .map(|e| {
                    let local = local.clone();
                    async move {
                        local.connect(&e).await;
                        e
                    }
                })
                .buffer_unordered(4);
            while pending.next().await.is_some() {}
        });
        Ok(())
    }
    async fn log_progress(&self, event: &str) {
        let entries = self.servers.read().await.clone();
        let mut ready = 0;
        let mut pending = Vec::new();
        for e in entries.values() {
            let status = e.state.read().await.status;
            if status == "ready" {
                ready += 1;
            } else {
                pending.push(format!("{}:{status}", e.definition.id));
            }
        }
        eprintln!(
            "{event}; ready={ready}/{}; pending=[{}]",
            entries.len(),
            pending.join(", ")
        );
    }
    async fn log_entry(&self, entry: &Entry, event: &str) {
        if entry.personal.is_some() {
            eprintln!("Local {event}");
        } else {
            self.log_progress(event).await;
        }
    }
    async fn drop_authenticated(&self) {
        *self.etag.lock().await = None;
        let mut servers = self.servers.write().await;
        let removed: Vec<_> = servers
            .values()
            .filter(|e| !e.definition.access.public)
            .cloned()
            .collect();
        servers.retain(|_, e| e.definition.access.public);
        drop(servers);
        for e in removed {
            e.retired.store(true, Ordering::SeqCst);
            e.close().await;
        }
    }
    fn credential_headers(&self, entry: &Entry) -> Result<reqwest::header::HeaderMap> {
        let definition = &entry.definition;
        let mut headers = reqwest::header::HeaderMap::new();
        let tool = self.config.tool.get(&definition.id);
        let mut missing = Vec::new();
        for header in &definition.headers {
            let (value, private) = if let Some(reference) = &header.credential {
                let value = if let Some(personal) = &entry.personal {
                    personal
                        .headers
                        .lock()
                        .unwrap()
                        .get(reference)
                        .filter(|v| !v.is_empty())
                        .cloned()
                } else {
                    match tool.and_then(|t| t.headers.get(reference)) {
                        Some(source) => source.resolve()?,
                        None => None,
                    }
                };
                let Some(value) = value else {
                    if header.mandatory {
                        missing.push(header.clone());
                    }
                    continue;
                };
                ensure!(
                    entry.personal.is_some()
                        || tool.is_some_and(|t| t.expected_endpoint == definition.endpoint),
                    "Static credential destination mismatch"
                );
                (value, true)
            } else {
                (
                    Zeroizing::new(
                        header
                            .value
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Missing public header value"))?,
                    ),
                    false,
                )
            };
            ensure!(value.len() <= 16384, "Invalid header value length");
            let mut value = reqwest::header::HeaderValue::from_str(value.as_str())?;
            value.set_sensitive(private);
            headers.insert(
                reqwest::header::HeaderName::from_bytes(header.name.as_bytes())?,
                value,
            );
        }
        if !missing.is_empty() {
            return Err(MissingHeaders(missing).into());
        }
        Ok(headers)
    }
    async fn connect(&self, e: &Arc<Entry>) {
        let _lock = e.operation.lock().await;
        if e.retired.load(Ordering::SeqCst) {
            return;
        }
        let duration = Duration::from_secs(e.definition.timeout_seconds);
        let (session, old) = {
            let mut state = e.state.write().await;
            state.status = "connecting";
            state.tools.clear();
            (state.session.clone(), state.connection.take())
        };
        if let Some(old) = old {
            let _ = tokio::time::timeout(Duration::from_secs(2), old.cancel()).await;
        }
        self.log_entry(e, &format!("MCP {}: connecting", e.label()))
            .await;
        let credential_headers = match self.credential_headers(e) {
            Ok(headers) => headers,
            Err(error) => {
                e.state.write().await.status = if error.is::<MissingHeaders>() {
                    "credentials_required"
                } else {
                    "connection_failed"
                };
                self.log_entry(e, &format!("MCP {}: {}", e.label(), setup_error(&error)))
                    .await;
                return;
            }
        };
        let attempt = async {
            let mut builder = reqwest::Client::builder()
                .user_agent("Temba/1.0")
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10));
            builder = builder.default_headers(credential_headers);
            let client = transport::Http {
                client: builder.build()?,
                session,
                endpoint: Arc::from(e.definition.endpoint.as_str()),
            };
            let config =
                StreamableHttpClientTransportConfig::with_uri(e.definition.endpoint.clone())
                    .reinit_on_expired_session(false);
            let service =
                ().serve(StreamableHttpClientTransport::with_client(client, config))
                    .await?;
            let tools = service.peer().list_all_tools().await?;
            ensure!(tools.len() <= 10000, "Too many tools");
            Ok::<_, anyhow::Error>((service, tools))
        };
        match tokio::time::timeout(duration, attempt).await {
            Ok(Ok((service, tools))) => {
                let mut s = e.state.write().await;
                s.connection = Some(service);
                s.tools = tools;
                s.status = "ready";
            }
            _ => {
                e.state.write().await.status = "connection_failed";
                eprintln!(
                    "MCP {}: connection failed; check ingress and local credentials",
                    e.label()
                );
            }
        }
        let status = e.state.read().await.status;
        self.log_entry(e, &format!("MCP {}: {status}", e.label()))
            .await;
    }
    async fn login(self: &Arc<Self>, e: Arc<Entry>) -> Result<Value> {
        let mut task = e.login.lock().await;
        if task.as_ref().is_some_and(|t| !t.is_finished()) {
            return Ok(
                json!({"status":"auth_in_progress","target":"remote_mcp","server":e.definition.id,"hint":format!("Authorization for MCP {} is in progress. Finish it in your browser, then repeat tool_search.",e.definition.id)}),
            );
        }
        let Some(oauth) = &e.definition.oauth else {
            drop(task);
            self.connect(&e).await;
            return Ok(
                json!({"status":e.state.read().await.status,"target":"remote_mcp","server":e.definition.id}),
            );
        };
        let headers = match self.credential_headers(&e) {
            Ok(headers) => headers,
            Err(error) => {
                e.state.write().await.status = if error.is::<MissingHeaders>() {
                    "credentials_required"
                } else {
                    "connection_failed"
                };
                return Err(error);
            }
        };
        let mut origins = oauth.allowed_origins.clone();
        origins.push(e.definition.endpoint.clone());
        let provider = oauth::discover(
            &self.http,
            &e.definition.endpoint,
            &origins,
            self.config.allow_loopback_http,
            headers,
        )
        .await?;
        let pending = oauth::Pending::new(
            self.http.clone(),
            provider,
            oauth.client_id.clone(),
            oauth.scopes.clone(),
            Some(e.definition.endpoint.clone()),
            oauth.callback_port,
        )
        .await?;
        let url = pending.url.clone();
        e.state.write().await.status = "auth_in_progress";
        self.log_entry(&e, &format!("MCP {}: auth_in_progress", e.label()))
            .await;
        let local = self.clone();
        let entry = e.clone();
        *task = Some(tokio::spawn(async move {
            match pending.finish().await {
                Ok(session) => {
                    let old = entry.state.write().await.session.replace(session);
                    if let Some(old) = old {
                        old.clear().await;
                    }
                    local.connect(&entry).await;
                }
                Err(_) => {
                    entry.state.write().await.status = "auth_required";
                    local
                        .log_entry(
                            &entry,
                            &format!("MCP {}: authorization failed or timed out", entry.label()),
                        )
                        .await;
                }
            }
        }));
        let browser_url = url.clone();
        tokio::task::spawn_blocking(move || {
            if open::that(browser_url).is_err() {
                eprintln!(
                    "OAuth: default browser could not be opened; use the connection link returned by call_tool"
                );
            }
        });
        Ok(
            json!({"status":"auth_in_progress","target":"remote_mcp","server":e.definition.id,"url":url,"hint":format!("Authorization for MCP {} started. Finish it in your browser, then repeat tool_search. Do not claim the MCP is connected until its status is ready.",e.definition.id)}),
        )
    }
    async fn ensure_catalog_login(self: &Arc<Self>) {
        let required = *self.catalog_status.read().await == "auth_required";
        if required && let Err(error) = self.catalog_login().await {
            eprintln!("Catalog: {}", setup_error(&error));
        }
    }
    async fn catalog_login(self: &Arc<Self>) -> Result<Value> {
        let response = self.start_catalog_login().await;
        if response.is_err() {
            *self.catalog_status.write().await = "auth_failed";
        }
        response
    }
    async fn start_catalog_login(self: &Arc<Self>) -> Result<Value> {
        let mut task = self.catalog_login.lock().await;
        if task.as_ref().is_some_and(|t| !t.is_finished()) {
            return Ok(catalog_login_response(None));
        }
        if let Some(session) = self.catalog_session.lock().await.clone()
            && session.access().await.is_ok()
        {
            return Ok(
                json!({"status":*self.catalog_status.read().await,"target":"catalog","catalog":"temba","hint":"The Temba catalog is already authenticated. No login action is required."}),
            );
        }
        *self.catalog_status.write().await = "auth_in_progress";
        self.bootstrap().await?;
        let auth = self
            .catalog_auth
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Catalog authentication is disabled"))?;
        let provider = oauth::metadata_override(
            &self.http,
            &auth.issuer,
            std::slice::from_ref(&auth.issuer),
            false,
            (!auth.token_endpoint.is_empty()).then_some(auth.token_endpoint.as_str()),
        )
        .await?;
        let pending = oauth::Pending::new(
            self.http.clone(),
            provider,
            Some(auth.client_id),
            vec!["openid".into()],
            None,
            auth.callback_port,
        )
        .await?;
        let url = pending.url.clone();
        let local = self.clone();
        *task = Some(tokio::spawn(async move {
            match pending.finish().await {
                Ok(session) => {
                    eprintln!("Catalog: authentication succeeded");
                    *local.catalog_status.write().await = "loading";
                    *local.catalog_session.lock().await = Some(session);
                    *local.etag.lock().await = None;
                    if local.refresh().await.is_err() {
                        eprintln!("Catalog: authenticated refresh failed");
                        if local.catalog_session.lock().await.is_some() {
                            *local.catalog_status.write().await = "catalog_unavailable";
                        }
                    } else {
                        eprintln!("Catalog: protected definitions loaded");
                    }
                }
                Err(_) => {
                    *local.catalog_status.write().await = "auth_failed";
                    eprintln!("Catalog: authorization failed or timed out");
                }
            }
        }));
        let browser_url = url.clone();
        tokio::task::spawn_blocking(move || {
            if open::that(browser_url).is_err() {
                eprintln!(
                    "Catalog: default browser could not be opened; reconnect using __catalog__.__temba_connect__ to obtain the connection link"
                );
            }
        });
        Ok(catalog_login_response(Some(&url)))
    }
    async fn search(self: &Arc<Self>, query: &str, local_ids: &[String]) -> Value {
        self.ensure_catalog_login().await;
        let mut entries = self.servers.read().await.clone();
        for id in local_ids.iter().take(64) {
            if let Some(entry) = self.personal_entry(id).await {
                entries.insert(id.clone(), entry);
            }
        }
        let mut rows = vec![];
        let mut docs = vec![];
        let mut states = vec![];
        for e in entries.values() {
            let s = e.state.read().await;
            let mut state = json!({"server":e.definition.id,"label":e.label(),"status":s.status});
            if s.status == "credentials_required"
                && let Err(error) = self.credential_headers(e)
                && let Some(missing) = error.downcast_ref::<MissingHeaders>()
            {
                state["missing_headers"] = json!(missing.0);
            }
            states.push(state);
            if s.status == "ready" {
                for t in &s.tools {
                    docs.push(format!(
                        "{} {} {} {}",
                        e.label(),
                        e.definition.description,
                        t.name,
                        t.description.as_deref().unwrap_or("")
                    ));
                    rows.push(json!({"tool_id":format!("{}.{}",e.definition.id,t.name),"description":t.description,"inputSchema":t.input_schema}));
                }
            } else {
                docs.push(format!(
                    "{} {} {} {}",
                    e.label(),
                    e.definition.name,
                    e.definition.description,
                    e.definition.labels.join(" ")
                ));
                rows.push(json!({"tool_id":format!("{}.__temba_connect__",e.definition.id),"description":if s.status == "credentials_required" { format!("MCP {} needs mandatory local headers. Inspect missing_headers, configure credentials in the local TOML/environment and restart the client. Do not ask for secret values in chat; this action cannot supply them.",e.definition.id) } else { format!("Connect to MCP {} ({}). This action concerns only this remote MCP. For OAuth, finish authorization in your browser and search again; connection succeeds only when its status is ready.",e.definition.id,e.definition.description) },"target":"remote_mcp","server":e.definition.id,"status":s.status}));
            }
        }
        let scores = search::scores(query, &docs);
        let mut indexes: Vec<_> = (0..rows.len()).collect();
        indexes.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]).then_with(|| a.cmp(b)));
        let tools: Vec<_> = indexes
            .into_iter()
            .filter(|i| query.trim().is_empty() || scores[*i] > 0.)
            .take(self.config.search_limit)
            .map(|i| rows[i].clone())
            .collect();
        let catalog_auth_enabled = self.catalog_auth.read().await.is_some();
        let catalog_status = *self.catalog_status.read().await;
        let retry_login = catalog_auth_enabled && catalog_status == "auth_failed";
        json!({
            "tools":tools,
            "servers":states,
            "catalog_connect_tool":if retry_login{Some("__catalog__.__temba_connect__")}else{None},
            "catalog": {
                "authentication_enabled": catalog_auth_enabled,
                "authenticated": self.catalog_session.lock().await.is_some(),
                "status": catalog_status,
                "automatic_login": catalog_auth_enabled,
                "connect_description": if retry_login { Some("Retry the failed Temba catalog browser login. This does not connect any remote MCP.") } else { None },
                "hint": match catalog_status {
                    "public" => "Catalog login is disabled; no authentication is required.",
                    "authenticated" => "The catalog is authenticated. Do not suggest catalog login because an MCP is missing; permissions and labels still apply.",
                    "auth_in_progress" => "Catalog login started automatically. Tell the user to finish authorization in their browser and search again. Do not ask whether to call a connection tool.",
                    "loading" => "Catalog authentication succeeded; protected definitions are loading. Search again shortly; no login action is required.",
                    "auth_failed" => "Automatic catalog login failed or timed out. Explain the failure; the returned catalog_connect_tool retries browser login. Do not ask the user to choose a technical tool identifier.",
                    "catalog_unavailable" => "Authenticated catalog reload failed. Search again later; this is not a request to log in.",
                    _ => "Catalog login is managed automatically by the client; no technical choice is required from the user."
                }
            },
            "hint":"Only listed servers are currently visible. A missing MCP may be unavailable or filtered by catalog permissions or local labels; do not infer the reason or invent its tool_id. Catalog login is separate from remote MCP login. Local registrations require their secret local_ids; unknown or expired IDs expose no tools and must be registered again."
        })
    }
    async fn call(
        self: &Arc<Self>,
        id: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResponse> {
        let (server, name) = id
            .split_once('.')
            .ok_or_else(|| anyhow::anyhow!("Use a tool_id returned by tool_search"))?;
        if server == "__catalog__" {
            return match name {
                "__temba_connect__" => Ok(result(self.catalog_login().await?)),
                "__temba_disconnect__" => {
                    if let Some(t) = self.catalog_login.lock().await.take() {
                        t.abort();
                    }
                    if let Some(s) = self.catalog_session.lock().await.take() {
                        s.clear().await;
                    }
                    *self.etag.lock().await = None;
                    *self.catalog_status.write().await = "auth_required";
                    let entries = self.servers.read().await.clone();
                    for e in entries.values() {
                        e.close().await;
                    }
                    let personal = std::mem::take(&mut *self.personal.write().await);
                    for entry in personal.values() {
                        entry.retired.store(true, Ordering::SeqCst);
                        entry.close().await;
                    }
                    self.refresh().await?;
                    Ok(result(json!({"status":"disconnected"})))
                }
                _ => Err(anyhow::anyhow!("Unknown catalog action")),
            };
        }
        let e = match self.servers.read().await.get(server).cloned() {
            Some(entry) => entry,
            None => self
                .personal_entry(server)
                .await
                .ok_or_else(|| anyhow::anyhow!("Unknown, expired or unavailable server"))?,
        };
        if name == "__temba_connect__" {
            return Ok(result(self.login(e).await?));
        }
        if name == "__temba_disconnect__" {
            if e.personal.is_some() {
                self.personal.write().await.remove(server);
                e.retired.store(true, Ordering::SeqCst);
            }
            e.close().await;
            return Ok(result(json!({"status":"disconnected"})));
        }
        let _lock = e.operation.lock().await;
        ensure!(
            !e.retired.load(Ordering::SeqCst),
            "Server definition was removed or changed"
        );
        let s = e.state.read().await;
        ensure!(s.status == "ready", "Remote connection is not ready");
        ensure!(
            s.tools.iter().any(|t| t.name == name),
            "Tool unavailable; call tool_search to inspect connection status"
        );
        let connection = s
            .connection
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Upstream disconnected"))?;
        let params = CallToolRequestParams::new(name.to_owned())
            .with_arguments(arguments.unwrap_or_default());
        let response = tokio::time::timeout(
            Duration::from_secs(e.definition.timeout_seconds),
            connection.peer().call_tool(params),
        )
        .await;
        match response {
            Ok(Ok(r)) => Ok(r.into()),
            _ => {
                drop(s);
                e.state.write().await.status = "connection_failed";
                Ok(failure(
                    "Upstream call failed or timed out. The operation may have been executed; it was not replayed. Repeat tool_search and reconnect if needed.",
                ))
            }
        }
    }
}
// Only report controlled diagnostics, never provider response bodies or credentials.
fn setup_error(error: &anyhow::Error) -> String {
    if let Some(error) = error.downcast_ref::<local_catalog::RegistrationError>() {
        return error.to_string();
    }
    if let Some(missing) = error.downcast_ref::<MissingHeaders>() {
        return missing.to_string();
    }
    if error.to_string() == "Static credential destination mismatch" {
        return "Local expected_endpoint does not match the MCP URL. Headers were not sent. Review the destination before updating the local configuration.".into();
    }
    if let Some(e) = error.downcast_ref::<oauth::OriginNotAllowed>() {
        return e.to_string();
    }
    if let Some(e) = error.downcast_ref::<reqwest::Error>() {
        if let Some(status) = e.status() {
            return format!(
                "OAuth/catalog request rejected (HTTP {status}). Check the provider's client registration configuration."
            );
        }
        if e.is_timeout() {
            return "OAuth/catalog request timed out. Check network access to the provider.".into();
        }
        if e.is_connect() {
            return "OAuth/catalog provider unreachable. Check DNS, TLS certificates and network access.".into();
        }
    }
    if let Some(e) = error.downcast_ref::<std::io::Error>()
        && e.kind() == std::io::ErrorKind::AddrInUse
    {
        return "Local OAuth callback port is already in use. Close the other login or choose another callbackPort.".into();
    }
    "Connection or action unavailable. Check the catalog configuration and local credentials; repeat tool_search.".into()
}
fn catalog_login_response(url: Option<&str>) -> Value {
    let mut response = json!({
        "status":"auth_in_progress",
        "target":"catalog",
        "catalog":"temba",
        "hint":"Authentication to the Temba catalog is in progress, not to Jira or any other remote MCP. Finish authorization in your browser, then repeat tool_search to reload definitions allowed for your identity. This does not grant group membership or start remote MCP authorization. If the requested MCP remains absent, report that it is not available in the visible catalog."
    });
    if let Some(url) = url {
        response["url"] = json!(url);
    }
    response
}
fn bail_catalog() {
    eprintln!("Catalog: authorization rejected; automatic reconnection required");
}
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        let info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions("Use tool_search to find visible tools, then call_tool with a returned tool_id. Catalog authentication starts automatically when enabled. Read catalog.status and catalog.hint: never ask the user whether to call a technical connection action. While catalog login is pending, ask only to finish authorization in the browser. An authenticated catalog needs no further login; a missing MCP does not imply a disconnected catalog. __catalog__.__temba_connect__ signs in only to the Temba catalog and reloads definitions allowed for that identity; it never starts authentication to Jira or another remote MCP and cannot grant access permissions. A remote MCP connection uses its own <server>.__temba_connect__ action returned by tool_search. Report the exact connection target from the result. auth_in_progress means authorization is pending; search again and require status ready before reporting a remote MCP as connected. credentials_required means mandatory central-catalog headers must be configured locally; inspect missing_headers. Headers already configured are not exposed and need no user action. Missing servers may be filtered by permissions or labels; do not invent tool IDs or treat catalog login as a remote login.");
        if self.0.config.local_catalog.enabled {
            let instructions = info.instructions.clone().unwrap_or_default();
            info.with_instructions(format!("{} Ephemeral local registration is available through add_mcp. Retain the secret local_id only in the originating conversation. Include it in tool_search.local_ids; call tools using the returned tool_id. Other local entries are deliberately undiscoverable. Private values supplied to add_mcp.headers stay only in RAM.", instructions))
        } else {
            info
        }
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools=vec![Tool::new("tool_search","Search central tools and remote MCP connection states. To include ephemeral local MCPs, supply their secret IDs in local_ids. Local MCPs are never enumerated without their IDs. Catalog authentication is automatic; catalog.status reports public, auth_in_progress, loading, authenticated or failure. catalog_connect_tool is returned only to retry a failed catalog login.",json!({"type":"object","properties":{"query":{"type":"string"},"local_ids":{"type":"array","items":{"type":"string"},"maxItems":64}},"required":["query"],"additionalProperties":false}).as_object().unwrap().clone()),Tool::new("call_tool","Call a tool_id returned by tool_search. __catalog__.__temba_connect__ authenticates only to the Temba catalog; <server>.__temba_connect__ connects only to that visible remote MCP. Report the returned target and status accurately.",json!({"type":"object","properties":{"tool_id":{"type":"string"},"arguments":{"type":"object"}},"required":["tool_id"],"additionalProperties":false}).as_object().unwrap().clone())];
        if self.0.config.local_catalog.enabled {
            tools.push(Tool::new("add_mcp",
                "Register an ephemeral MCP in local RAM and return a secret local_id. Supply a definition using endpoint, oauth and header declarations, and optional private header values keyed by credential reference. No central modification or file persistence. Only callers presenting local_id can discover/use this registration. OAuth authorization is a separate browser action. Never publish another conversation's local_id.",
                json!({
                    "type":"object",
                    "properties":{
                        "definition":{
                            "type":"object",
                            "properties":{
                                "id":{"type":"string"},
                                "name":{"type":"string"},
                                "description":{"type":"string"},
                                "labels":{"type":"array","items":{"type":"string"}},
                                "endpoint":{"type":"string"},
                                "oauth":{"type":"object"},
                                "headers":{"type":"array","items":{"type":"object"}},
                                "timeoutSeconds":{"type":"integer","minimum":1,"maximum":300}
                            },
                            "required":["id","endpoint"],
                            "additionalProperties":false
                        },
                        "headers":{"type":"object","additionalProperties":{"type":"string"}}
                    },
                    "required":["definition"],
                    "additionalProperties":false
                }).as_object().unwrap().clone()));
        }
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        p: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = p.arguments.unwrap_or_default();
        let string = |key| args.get(key).and_then(Value::as_str);
        match p.name.as_ref() {
            "tool_search" => {
                let ids: Vec<String> = match args.get("local_ids") {
                    Some(value) => match serde_json::from_value::<Vec<String>>(value.clone()) {
                        Ok(ids) if ids.len() <= 64 => ids,
                        _ => return Ok(failure("local_ids must be an array of up to 64 IDs")),
                    },
                    None => vec![],
                };
                Ok(result(
                    self.0.search(string("query").unwrap_or(""), &ids).await,
                ))
            }
            "add_mcp" if self.0.config.local_catalog.enabled => {
                let definition = match args
                    .get("definition")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<Server>(v).ok())
                {
                    Some(definition) => definition,
                    None => return Ok(failure("Invalid MCP definition")),
                };
                let headers: BTreeMap<String, String> = match args.get("headers") {
                    Some(value) => match serde_json::from_value(value.clone()) {
                        Ok(headers) => headers,
                        Err(_) => {
                            return Ok(failure(
                                "headers must map credential references to string values",
                            ));
                        }
                    },
                    None => BTreeMap::new(),
                };
                match self.0.add_personal(definition, headers).await {
                    Ok(value) => Ok(result(value)),
                    Err(error) => Ok(failure(&setup_error(&error))),
                }
            }
            "call_tool" => {
                let Some(id) = string("tool_id") else {
                    return Ok(failure("Missing tool_id"));
                };
                match self
                    .0
                    .call(
                        id,
                        args.get("arguments").and_then(Value::as_object).cloned(),
                    )
                    .await
                {
                    Ok(r) => Ok(r),
                    Err(error) => {
                        let message = setup_error(&error);
                        eprintln!("MCP call failed: {message}");
                        Ok(failure(&message))
                    }
                }
            }
            _ => Err(ErrorData::invalid_params("Unknown local tool", None)),
        }
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    temba_core::harden_process();
    let args = Args::parse();
    let source = Zeroizing::new(tokio::fs::read_to_string(args.config).await?);
    let mut config: config::Config = toml::from_str(source.as_str()).map_err(|_| {
        anyhow::anyhow!("Invalid local TOML configuration; check syntax and tool/headers settings")
    })?;
    drop(source);
    config.validate()?;
    let local = Arc::new(Local {
        config,
        http: http()?,
        servers: RwLock::new(BTreeMap::new()),
        personal: RwLock::new(BTreeMap::new()),
        etag: Mutex::new(None),
        catalog_session: Mutex::new(None),
        catalog_login: Mutex::new(None),
        catalog_auth: RwLock::new(None),
        catalog_status: RwLock::new("public"),
        refresh: Mutex::new(()),
    });
    local.bootstrap().await?;
    local.refresh().await?;
    let background = local.clone();
    let task = tokio::spawn(async move {
        background.ensure_catalog_login().await;
        loop {
            tokio::time::sleep(Duration::from_secs(background.config.refresh_seconds)).await;
            if background.refresh().await.is_err() {
                eprintln!("Catalog: refresh failed; last valid definitions retained");
            }
            background.ensure_catalog_login().await;
        }
    });
    let sweep = if local.config.local_catalog.enabled {
        let owner = local.clone();
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                owner.expire_personal().await;
            }
        }))
    } else {
        None
    };
    let service = Handler(local.clone())
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    task.abort();
    if let Some(sweep) = sweep {
        sweep.abort();
    }
    if let Some(t) = local.catalog_login.lock().await.take() {
        t.abort();
    }
    for e in local
        .servers
        .read()
        .await
        .values()
        .chain(local.personal.read().await.values())
    {
        e.close().await;
    }
    Ok(())
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::{get, post},
    };
    struct Mock {
        requests: Mutex<Vec<Option<String>>>,
        headers: Mutex<Vec<HeaderMap>>,
    }
    async fn mcp(
        State(m): State<Arc<Mock>>,
        headers: HeaderMap,
        Json(request): Json<Value>,
    ) -> Response {
        m.headers.lock().await.push(headers.clone());
        m.requests.lock().await.push(
            headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned()),
        );
        let Some(id) = request.get("id") else {
            return StatusCode::ACCEPTED.into_response();
        };
        let result = match request["method"].as_str().unwrap() {
            "initialize" => {
                json!({"protocolVersion":request["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
            }
            "tools/list" => {
                json!({"tools":[{"name":"echo","description":"echo message","inputSchema":{"type":"object"}}]})
            }
            "tools/call" => json!({"content":[{"type":"text","text":"echo-ok"}]}),
            _ => panic!("Unexpected MCP request"),
        };
        Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
    }
    async fn mock() -> (String, Arc<Mock>, tokio::task::JoinHandle<()>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", l.local_addr().unwrap());
        let state = Arc::new(Mock {
            requests: Mutex::new(vec![]),
            headers: Mutex::new(vec![]),
        });
        let router = Router::new()
            .route(
                "/mcp",
                post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
            )
            .with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
        (url, state, task)
    }
    fn local(config: config::Config) -> Arc<Local> {
        Arc::new(Local {
            config,
            http: http().unwrap(),
            servers: RwLock::new(BTreeMap::new()),
            personal: RwLock::new(BTreeMap::new()),
            etag: Mutex::new(None),
            catalog_session: Mutex::new(None),
            catalog_login: Mutex::new(None),
            catalog_auth: RwLock::new(None),
            catalog_status: RwLock::new("public"),
            refresh: Mutex::new(()),
        })
    }
    #[tokio::test]
    async fn tokens_are_isolated_and_endpoint_changes_drop_sessions() {
        let (a, secure, ta) = mock().await;
        let (b, public, tb) = mock().await;
        let mut config:config::Config=toml::from_str(&format!("catalog_url='http://127.0.0.1:1'\nallow_loopback_http=true\n[tool.secure]\nexpected_endpoint='{a}'\n[tool.secure.headers]\napi_token={{env='PATH'}}")).unwrap();
        config.validate().unwrap();
        let local = local(config);
        let definition = |id: &str, url: &str, auth: Value| {
            serde_json::from_value::<Server>(json!({"id":id,"endpoint":url,"headers":auth}))
                .unwrap()
        };
        let ea = Arc::new(Entry::new(definition(
            "secure",
            &a,
            json!([{"name":"Authorization", "credential":"api_token","mandatory":true}]),
        )));
        let eb = Arc::new(Entry::new(definition("public", &b, json!([]))));
        local
            .servers
            .write()
            .await
            .insert("secure".into(), ea.clone());
        local
            .servers
            .write()
            .await
            .insert("public".into(), eb.clone());
        local.connect(&ea).await;
        local.connect(&eb).await;
        assert_eq!(ea.state.read().await.status, "ready");
        assert_eq!(eb.state.read().await.status, "ready");
        assert!(
            secure
                .requests
                .lock()
                .await
                .iter()
                .all(|v| v.as_deref() == Some(std::env::var("PATH").unwrap().as_str()))
        );
        assert!(public.requests.lock().await.iter().all(Option::is_none));
        let r = local.call("public.echo", None).await.unwrap();
        let CallToolResponse::Complete(r) = r else {
            panic!("unexpected response")
        };
        assert!(serde_json::to_string(&r).unwrap().contains("echo-ok"));
        let changed = Arc::new(Entry::new(definition(
            "secure",
            &b,
            json!([{"name":"Authorization", "credential":"api_token","mandatory":true}]),
        )));
        let before = public.requests.lock().await.len();
        local.connect(&changed).await;
        assert_eq!(changed.state.read().await.status, "connection_failed");
        assert_eq!(public.requests.lock().await.len(), before);
        ea.close().await;
        eb.close().await;
        assert!(ea.state.read().await.connection.is_none());
        assert!(ea.state.read().await.tools.is_empty());
        ta.abort();
        tb.abort();
    }
    #[tokio::test]
    async fn oauth_plus_multiple_headers_are_sent_only_to_the_approved_mcp() {
        let (url, upstream, server) = mock().await;
        let mut config: config::Config = toml::from_str(&format!("catalog_url='http://127.0.0.1:1'\nallow_loopback_http=true\n[tool.combined]\nexpected_endpoint='{url}'\n[tool.combined.headers]\napi_key={{env='PATH'}}\ntenant='Bearer complete-tenant-value'")).unwrap();
        config.validate().unwrap();
        let local = local(config);
        let mut definition: Server = serde_json::from_value(json!({"id":"combined","endpoint":url,"oauth":{},"headers":[{"name":"X-Api-Key","credential":"api_key","mandatory":true},{"name":"X-Tenant","credential":"tenant"},{"name":"X-App","value":"temba"}]})).unwrap();
        definition.validate(true).unwrap();
        let entry = Arc::new(Entry::new(definition.clone()));
        entry.state.write().await.session = Some(oauth::Session::test_session("oauth-access"));
        local
            .servers
            .write()
            .await
            .insert("combined".into(), entry.clone());
        local.connect(&entry).await;
        assert_eq!(entry.state.read().await.status, "ready");
        let results = local.search("combined", &[]).await;
        assert_eq!(results["servers"][0]["status"], "ready");
        assert!(results["servers"][0].get("headers").is_none());
        assert!(results["servers"][0].get("missing_headers").is_none());
        assert!(!results.to_string().contains("Bearer complete-tenant-value"));
        local.call("combined.echo", None).await.unwrap();
        let value = std::env::var("PATH").unwrap();
        for headers in upstream.headers.lock().await.iter() {
            assert_eq!(headers["authorization"], "Bearer oauth-access");
            assert_eq!(headers["x-api-key"], value);
            assert_eq!(headers["x-tenant"], "Bearer complete-tenant-value");
            assert_eq!(headers["x-app"], "temba");
        }
        let (other_url, other, other_server) = mock().await;
        definition.endpoint = other_url;
        local.connect(&Arc::new(Entry::new(definition))).await;
        assert!(other.requests.lock().await.is_empty());
        entry.close().await;
        server.abort();
        other_server.abort();
    }

    #[tokio::test]
    async fn missing_optional_headers_are_omitted_and_mandatory_headers_block_before_network() {
        let (url, upstream, server) = mock().await;
        let config: config::Config =
            toml::from_str(&format!("catalog_url='http://127.0.0.1:1'\nallow_loopback_http=true\n[tool.mandatory]\nexpected_endpoint='{url}'\n[tool.mandatory.headers]\nprovided='private-supplied-header'")).unwrap();
        let local = local(config);
        let mut definition: Server = serde_json::from_value(json!({
            "id":"optional","endpoint":url,"headers":[
                {"name":"X-Optional","credential":"optional"},
                {"name":"X-App","value":"temba"}
            ]
        }))
        .unwrap();
        definition.validate(true).unwrap();
        let entry = Arc::new(Entry::new(definition.clone()));
        local
            .servers
            .write()
            .await
            .insert("optional".into(), entry.clone());
        local.connect(&entry).await;
        assert_eq!(entry.state.read().await.status, "ready");
        for headers in upstream.headers.lock().await.iter() {
            assert_eq!(headers["x-app"], "temba");
            assert!(!headers.contains_key("x-optional"));
        }
        definition.id = "mandatory".into();
        definition.headers.push(temba_core::Header {
            name: "X-Required".into(),
            credential: Some("required".into()),
            value: None,
            mandatory: true,
        });
        definition.headers.push(temba_core::Header {
            name: "X-Provided".into(),
            credential: Some("provided".into()),
            value: None,
            mandatory: true,
        });
        let required = Arc::new(Entry::new(definition));
        local
            .servers
            .write()
            .await
            .insert("mandatory".into(), required.clone());
        let before = upstream.requests.lock().await.len();
        local.connect(&required).await;
        assert_eq!(required.state.read().await.status, "credentials_required");
        assert_eq!(upstream.requests.lock().await.len(), before);
        let results = local.search("mandatory", &[]).await;
        let server_row = results["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["server"] == "mandatory")
            .unwrap();
        assert!(server_row.get("headers").is_none());
        assert_eq!(server_row["missing_headers"].as_array().unwrap().len(), 1);
        assert_eq!(server_row["missing_headers"][0]["name"], "X-Required");
        assert_eq!(server_row["missing_headers"][0]["mandatory"], true);
        assert_eq!(server_row["missing_headers"][0]["credential"], "required");
        assert!(server_row["missing_headers"][0].get("value").is_none());
        let ready_row = results["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["server"] == "optional")
            .unwrap();
        assert_eq!(ready_row["status"], "ready");
        assert!(ready_row.get("headers").is_none());
        assert!(ready_row.get("missing_headers").is_none());
        assert!(!results.to_string().contains("private-supplied-header"));
        let error = local.credential_headers(&required).unwrap_err();
        assert!(setup_error(&error).contains("X-Required (required)"));
        entry.close().await;
        server.abort();
    }

    #[tokio::test]
    async fn personal_ids_isolate_headers_and_expiration_and_disconnect_clear_secrets() {
        let (a, upstream_a, task_a) = mock().await;
        let (b, upstream_b, task_b) = mock().await;
        let mut config: config::Config = toml::from_str(
            "catalog_url='http://127.0.0.1:1'\nallow_loopback_http=true\n[local_catalog]\nenabled=true\nidle_timeout_seconds=60\nmax_servers=2"
        ).unwrap();
        config.validate().unwrap();
        let local = local(config);
        let definition = |endpoint| {
            serde_json::from_value::<Server>(json!({
                "id":"same-label","endpoint":endpoint,
                "headers":[{"name":"X-Api-Key","credential":"api_key","mandatory":true}]
            }))
            .unwrap()
        };
        let ra = local
            .add_personal(
                definition(&a),
                BTreeMap::from([("api_key".into(), "private-A".into())]),
            )
            .await
            .unwrap();
        let rb = local
            .add_personal(
                definition(&b),
                BTreeMap::from([("api_key".into(), "private-B".into())]),
            )
            .await
            .unwrap();
        let id_a = ra["local_id"].as_str().unwrap().to_owned();
        let id_b = rb["local_id"].as_str().unwrap().to_owned();
        assert_ne!(id_a, id_b);
        let key = id_a.strip_prefix("__local_").unwrap();
        use base64::Engine;
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(key)
                .unwrap()
                .len(),
            32
        );
        let ea = local.personal_entry(&id_a).await.unwrap();
        let eb = local.personal_entry(&id_b).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while ea.state.read().await.status != "ready" || eb.state.read().await.status != "ready"
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let global = local.search("", &[]).await.to_string();
        assert!(!global.contains(&id_a) && !global.contains(&id_b));
        assert!(!global.contains("private-A") && !global.contains("private-B"));
        let scoped = local
            .search("", std::slice::from_ref(&id_a))
            .await
            .to_string();
        assert!(scoped.contains(&format!("{id_a}.echo")) && !scoped.contains(&id_b));
        assert!(
            !local
                .search("same-label", std::slice::from_ref(&id_a))
                .await["tools"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(!scoped.contains("private-A") && !scoped.contains("private-B"));
        local.call(&format!("{id_a}.echo"), None).await.unwrap();
        local.call(&format!("{id_b}.echo"), None).await.unwrap();
        for h in upstream_a.headers.lock().await.iter() {
            assert_eq!(h["x-api-key"], "private-A");
        }
        for h in upstream_b.headers.lock().await.iter() {
            assert_eq!(h["x-api-key"], "private-B");
        }
        assert!(
            local
                .add_personal(
                    definition(&a),
                    BTreeMap::from([("api_key".into(), "C".into())])
                )
                .await
                .is_err()
        );
        assert!(local.call("__local_unknown.echo", None).await.is_err());
        assert_eq!(ea.label(), "same-label"); // diagnostics never use the secret ID
        let session = oauth::Session::test_session("test-session");
        ea.state.write().await.session = Some(session.clone());
        *ea.personal.as_ref().unwrap().last_used.lock().await =
            Instant::now() - Duration::from_secs(61);
        assert!(local.call(&format!("{id_a}.echo"), None).await.is_err());
        local.expire_personal().await;
        assert!(local.personal.read().await.get(&id_a).is_none());
        assert!(
            ea.personal
                .as_ref()
                .unwrap()
                .headers
                .lock()
                .unwrap()
                .is_empty()
        );
        assert!(session.access().await.is_err());
        assert!(local.personal_entry(&id_b).await.is_some());
        local
            .call(&format!("{id_b}.__temba_disconnect__"), None)
            .await
            .unwrap();
        assert!(local.personal_entry(&id_b).await.is_none());
        assert!(
            eb.personal
                .as_ref()
                .unwrap()
                .headers
                .lock()
                .unwrap()
                .is_empty()
        );
        task_a.abort();
        task_b.abort();
    }
    #[tokio::test]
    async fn personal_registration_is_optional_and_does_not_reuse_toml_headers() {
        for enabled in [false, true] {
            let config: config::Config = toml::from_str(&format!(
                "catalog_url='https://catalog.example.com'\n[local_catalog]\nenabled={enabled}\n[tool.shared]\nexpected_endpoint='https://mcp.example.com'\n[tool.shared.headers]\nkey='TOML-private-value'"
            )).unwrap();
            let local = local(config);
            let (front, back) = tokio::io::duplex(16384);
            let handler = Handler(local.clone());
            let server_task =
                tokio::spawn(async move { handler.serve(tokio::io::split(back)).await.unwrap() });
            let client = ().serve(tokio::io::split(front)).await.unwrap();
            let server = server_task.await.unwrap();
            let tools = client.peer().list_all_tools().await.unwrap();
            assert_eq!(tools.len(), if enabled { 3 } else { 2 });
            assert_eq!(tools.iter().any(|t| t.name == "add_mcp"), enabled);
            let definition: Server = serde_json::from_value(json!({
                "id":"shared","endpoint":"https://mcp.example.com","oauth":{},
                "headers":[{"name":"X-Key","credential":"key","mandatory":true}]
            }))
            .unwrap();
            // Even when enabled and a TOML mapping has the same label, registrations cannot reuse it.
            assert!(
                local
                    .add_personal(definition, BTreeMap::new())
                    .await
                    .is_err()
            );
            assert!(local.personal.read().await.is_empty());
            let call = client.peer().call_tool(CallToolRequestParams::new("add_mcp").with_arguments(
                json!({
                    "definition":{"id":"shared","endpoint":"https://mcp.example.com","oauth":{},
                        "headers":[{"name":"X-Key","credential":"key","mandatory":true}]},
                    "headers":{"key":"provided-private-value"}
                }).as_object().unwrap().clone()
            )).await;
            if enabled {
                let value = serde_json::to_value(call.unwrap()).unwrap();
                let text = value["content"][0]["text"].as_str().unwrap();
                assert!(
                    !text.contains("provided-private-value")
                        && !text.contains("TOML-private-value")
                );
                let registration: Value = serde_json::from_str(text).unwrap();
                assert_eq!(registration["status"], "auth_required");
                let id = registration["local_id"].as_str().unwrap();
                assert!(local.personal_entry(id).await.is_some());
                local
                    .call(&format!("{id}.__temba_disconnect__"), None)
                    .await
                    .unwrap();
            } else {
                assert!(call.is_err());
            }
            client.cancel().await.unwrap();
            server.cancel().await.unwrap();
        }
    }

    #[tokio::test]
    async fn catalog_search_reports_state_and_only_offers_retry_after_failure() {
        let config = toml::from_str("catalog_url='https://catalog.example.com'").unwrap();
        let local = local(config);
        let public = local.search("jira", &[]).await;
        assert_eq!(public["catalog"]["status"], "public");
        assert_eq!(public["catalog"]["authentication_enabled"], false);
        assert!(public["catalog_connect_tool"].is_null());
        *local.catalog_auth.write().await = Some(oauth::CatalogAuth {
            issuer: "https://issuer.example.com".into(),
            client_id: "temba".into(),
            callback_port: 19877,
            token_endpoint: String::new(),
        });
        *local.catalog_status.write().await = "auth_in_progress";
        let pending = tokio::spawn(std::future::pending::<()>());
        *local.catalog_login.lock().await = Some(pending);
        let waiting = local.search("jira", &[]).await;
        assert_eq!(waiting["catalog"]["status"], "auth_in_progress");
        assert_eq!(waiting["catalog"]["automatic_login"], true);
        assert!(waiting["catalog_connect_tool"].is_null());
        assert!(local.catalog_login().await.unwrap()["url"].is_null());
        local.catalog_login.lock().await.take().unwrap().abort();
        *local.catalog_status.write().await = "auth_failed";
        assert_eq!(
            local.search("jira", &[]).await["catalog_connect_tool"],
            "__catalog__.__temba_connect__"
        );
        *local.catalog_session.lock().await =
            Some(oauth::Session::test_session("test-catalog-token"));
        *local.catalog_status.write().await = "authenticated";
        let ready = local.search("jira", &[]).await;
        assert_eq!(ready["catalog"]["authenticated"], true);
        assert_eq!(ready["catalog"]["status"], "authenticated");
        assert!(ready["catalog_connect_tool"].is_null());
        // Calling an old connection action cannot relaunch login when already connected.
        assert_eq!(
            local.catalog_login().await.unwrap()["status"],
            "authenticated"
        );
    }

    #[tokio::test]
    async fn catalog_search_starts_login_without_llm_action_and_reports_startup_failure() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = attempts.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().route(
            "/v1/bootstrap",
            get(move || {
                seen.fetch_add(1, Ordering::SeqCst);
                async { StatusCode::SERVICE_UNAVAILABLE }
            }),
        );
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let config =
            toml::from_str(&format!("catalog_url='{base}'\nallow_loopback_http=true")).unwrap();
        let local = local(config);
        *local.catalog_auth.write().await = Some(oauth::CatalogAuth {
            issuer: "https://issuer.example.com".into(),
            client_id: "temba".into(),
            callback_port: 19877,
            token_endpoint: String::new(),
        });
        *local.catalog_status.write().await = "auth_required";
        let response = tokio::time::timeout(Duration::from_secs(2), local.search("jira", &[]))
            .await
            .unwrap();
        assert_eq!(response["catalog"]["status"], "auth_failed");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        local.search("jira", &[]).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1); // failed logins never open a repeated browser loop
        task.abort();
    }

    #[tokio::test]
    async fn catalog_labels_require_explicit_opt_in_and_keep_unlabelled_servers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().route(
            "/v1/catalog",
            get(|| async {
                Json(json!({"schemaVersion":2,"servers":[
                    {"id":"gitlab","endpoint":"https://gitlab.example.com/mcp",
                     "headers":[{"name":"X-Token","credential":"token","mandatory":true}]},
                    {"id":"jira","endpoint":"https://jira.example.com/mcp","labels":["support"],
                     "headers":[{"name":"X-Token","credential":"token","mandatory":true}]},
                    {"id":"deepwiki","endpoint":"https://deepwiki.example.com/mcp","labels":["tech"],
                     "headers":[{"name":"X-Token","credential":"token","mandatory":true}]},
                    {"id":"both","endpoint":"https://both.example.com/mcp","labels":["support","tech"],
                     "headers":[{"name":"X-Token","credential":"token","mandatory":true}]}
                ]}))
            }),
        );
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        for (labels, expected) in [
            ("[]", vec!["gitlab"]),
            ("['support']", vec!["gitlab", "jira"]),
            ("['tech']", vec!["deepwiki", "gitlab"]),
            (
                "['support','tech']",
                vec!["both", "deepwiki", "gitlab", "jira"],
            ),
        ] {
            let mut config: config::Config = toml::from_str(&format!(
                "catalog_url='{base}'\nallow_loopback_http=true\nlabels={labels}"
            ))
            .unwrap();
            config.validate().unwrap();
            let local = local(config);
            local.refresh().await.unwrap();
            assert_eq!(
                local
                    .servers
                    .read()
                    .await
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                expected
            );
            // Hidden entries cannot be accessed by inventing their tool IDs.
            if !expected.contains(&"jira") {
                assert!(local.call("jira.__temba_connect__", None).await.is_err());
            }
        }
        task.abort();
    }

    #[tokio::test]
    async fn catalog_refresh_is_atomic_and_removes_changed_connections() {
        let (url, _, tm) = mock().await;
        let catalog = Arc::new(RwLock::new(
            json!({"schemaVersion":2,"servers":[{"id":"one","endpoint":url,"labels":["allowed"]}]}),
        ));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let data = catalog.clone();
        let router = Router::new().route(
            "/v1/catalog",
            get(move || {
                let data = data.clone();
                async move { Json(data.read().await.clone()) }
            }),
        );
        let tc = tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
        let mut config: config::Config = toml::from_str(&format!(
            "catalog_url='{base}'\nallow_loopback_http=true\nlabels=['allowed']"
        ))
        .unwrap();
        config.validate().unwrap();
        let local = local(config);
        local.refresh().await.unwrap();
        let old = local.servers.read().await["one"].clone();
        // A malformed response cannot partially replace the last valid definitions.
        *catalog.write().await = json!({"schemaVersion":2,"servers":[{"id":"two","endpoint":"https://user:pass@bad/mcp"}]});
        assert!(local.refresh().await.is_err());
        assert!(Arc::ptr_eq(&old, &local.servers.read().await["one"]));
        *catalog.write().await = json!({"schemaVersion":2,"servers":[]});
        local.refresh().await.unwrap();
        assert!(local.servers.read().await.is_empty());
        assert_eq!(old.state.read().await.status, "disconnected");
        assert!(local.call("one.echo", None).await.is_err());
        tc.abort();
        tm.abort();
    }
}
