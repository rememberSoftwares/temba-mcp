mod config;
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
    time::Duration,
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
    etag: Mutex<Option<String>>,
    catalog_session: Mutex<Option<Arc<oauth::Session>>>,
    catalog_login: Mutex<Option<tokio::task::JoinHandle<()>>>,
    catalog_auth: RwLock<Option<oauth::CatalogAuth>>,
    refresh: Mutex<()>,
}
#[derive(Debug)]
struct MissingHeaders(Vec<String>);
impl std::fmt::Display for MissingHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Mandatory local headers missing: {}. Configure their environment variables in the local TOML and restart the client; do not provide secrets in chat.",
            self.0.join(", ")
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
                    self.drop_authenticated().await;
                    return Err(anyhow::anyhow!("Catalog reconnection required"));
                }
            }
        }
        let response = req.send().await?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(());
        }
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(session) = self.catalog_session.lock().await.take() {
                session.clear().await;
            }
            *self.etag.lock().await = None;
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
        catalog.servers.retain(|s| {
            self.config.labels.is_empty() || self.config.labels.iter().all(|l| s.labels.contains(l))
        });
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
                if let Err(error) = self.credential_headers(&e.definition) {
                    e.state.write().await.status = if error.is::<MissingHeaders>() {
                        "credentials_required"
                    } else {
                        "connection_failed"
                    };
                    eprintln!("MCP {}: {}", e.definition.id, setup_error(&error));
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
    fn credential_headers(&self, definition: &Server) -> Result<reqwest::header::HeaderMap> {
        let mut headers = reqwest::header::HeaderMap::new();
        let tool = self.config.tool.get(&definition.id);
        let mut missing = Vec::new();
        for header in &definition.headers {
            let (value, private) = if let Some(reference) = &header.credential {
                let value = match tool.and_then(|t| t.headers.get(reference)) {
                    Some(source) => source.resolve()?,
                    None => None,
                };
                let Some(value) = value else {
                    if header.mandatory {
                        missing.push(format!("{} ({reference})", header.name));
                    }
                    continue;
                };
                ensure!(
                    tool.is_some_and(|t| t.expected_endpoint == definition.endpoint),
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
        self.log_progress(&format!("MCP {}: connecting", e.definition.id))
            .await;
        let credential_headers = match self.credential_headers(&e.definition) {
            Ok(headers) => headers,
            Err(error) => {
                e.state.write().await.status = if error.is::<MissingHeaders>() {
                    "credentials_required"
                } else {
                    "connection_failed"
                };
                self.log_progress(&format!("MCP {}: {}", e.definition.id, setup_error(&error)))
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
                    e.definition.id
                );
            }
        }
        let status = e.state.read().await.status;
        self.log_progress(&format!("MCP {}: {status}", e.definition.id))
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
        let headers = match self.credential_headers(&e.definition) {
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
        self.log_progress(&format!("MCP {}: auth_in_progress", e.definition.id))
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
                        .log_progress(&format!(
                            "MCP {}: authorization failed or timed out",
                            entry.definition.id
                        ))
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
    async fn catalog_login(self: &Arc<Self>) -> Result<Value> {
        let mut task = self.catalog_login.lock().await;
        if task.as_ref().is_some_and(|t| !t.is_finished()) {
            return Ok(catalog_login_response(None));
        }
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
                    *local.catalog_session.lock().await = Some(session);
                    *local.etag.lock().await = None;
                    if local.refresh().await.is_err() {
                        eprintln!("Catalog: authenticated refresh failed");
                    } else {
                        eprintln!("Catalog: protected definitions loaded");
                    }
                }
                Err(_) => eprintln!("Catalog: authorization failed or timed out"),
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
    async fn search(&self, query: &str) -> Value {
        let entries = self.servers.read().await.clone();
        let mut rows = vec![];
        let mut docs = vec![];
        let mut states = vec![];
        for e in entries.values() {
            let s = e.state.read().await;
            states.push(
                json!({"server":e.definition.id,"status":s.status,"headers":e.definition.headers}),
            );
            if s.status == "ready" {
                for t in &s.tools {
                    docs.push(format!(
                        "{} {} {} {}",
                        e.definition.id,
                        e.definition.description,
                        t.name,
                        t.description.as_deref().unwrap_or("")
                    ));
                    rows.push(json!({"tool_id":format!("{}.{}",e.definition.id,t.name),"description":t.description,"inputSchema":t.input_schema}));
                }
            } else {
                docs.push(format!(
                    "{} {} {} {}",
                    e.definition.id,
                    e.definition.name,
                    e.definition.description,
                    e.definition.labels.join(" ")
                ));
                rows.push(json!({"tool_id":format!("{}.__temba_connect__",e.definition.id),"description":if s.status == "credentials_required" { format!("MCP {} needs mandatory local headers. Inspect its headers metadata, configure credentials in the local TOML/environment and restart the client. Do not ask for secret values in chat; this action cannot supply them.",e.definition.id) } else { format!("Connect to MCP {} ({}). This action concerns only this remote MCP. For OAuth, finish authorization in your browser and search again; connection succeeds only when its status is ready.",e.definition.id,e.definition.description) },"target":"remote_mcp","server":e.definition.id,"status":s.status}));
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
        json!({
            "tools":tools,
            "servers":states,
            "catalog_connect_tool":if catalog_auth_enabled{Some("__catalog__.__temba_connect__")}else{None},
            "catalog": {
                "authenticated": self.catalog_session.lock().await.is_some(),
                "connect_description": if catalog_auth_enabled { Some("Sign in to the Temba catalog to reload definitions visible to your identity. This does not authenticate to any remote MCP or grant group membership.") } else { None }
            },
            "hint":"Only listed servers are currently visible. A missing MCP may be unavailable or filtered by catalog permissions or local labels; do not infer the reason or invent its tool_id. Catalog login is separate from remote MCP login."
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
                    let entries = self.servers.read().await.clone();
                    for e in entries.values() {
                        e.close().await;
                    }
                    self.refresh().await?;
                    Ok(result(json!({"status":"disconnected"})))
                }
                _ => Err(anyhow::anyhow!("Unknown catalog action")),
            };
        }
        let e = self
            .servers
            .read()
            .await
            .get(server)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Unknown server or excluded by labels"))?;
        if name == "__temba_connect__" {
            return Ok(result(self.login(e).await?));
        }
        if name == "__temba_disconnect__" {
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
    eprintln!("Catalog: authorization rejected; reconnect using __catalog__.__temba_connect__");
}
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions("Use tool_search to find visible tools, then call_tool with a returned tool_id. __catalog__.__temba_connect__ signs in only to the Temba catalog and reloads definitions allowed for that identity; it never starts authentication to Jira or another remote MCP and cannot grant access permissions. A remote MCP connection uses its own <server>.__temba_connect__ action returned by tool_search. Report the exact connection target from the result. auth_in_progress means authorization is pending; search again and require status ready before reporting a remote MCP as connected. credentials_required means mandatory headers must be configured locally; inspect the headers metadata and never ask users to paste secrets in chat. Missing servers may be filtered by permissions or labels; do not invent tool IDs or treat catalog login as a remote login.")
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools=vec![Tool::new("tool_search","Search visible tools and remote MCP connection states. An absent MCP may be filtered by permissions or labels. catalog_connect_tool signs in only to Temba; it does not connect to a remote MCP.",json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}).as_object().unwrap().clone()),Tool::new("call_tool","Call a tool_id returned by tool_search. __catalog__.__temba_connect__ authenticates only to the Temba catalog; <server>.__temba_connect__ connects only to that visible remote MCP. Report the returned target and status accurately.",json!({"type":"object","properties":{"tool_id":{"type":"string"},"arguments":{"type":"object"}},"required":["tool_id"],"additionalProperties":false}).as_object().unwrap().clone())];
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
            "tool_search" => Ok(result(self.0.search(string("query").unwrap_or("")).await)),
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
                        eprintln!("MCP {id}: {message}");
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
        etag: Mutex::new(None),
        catalog_session: Mutex::new(None),
        catalog_login: Mutex::new(None),
        catalog_auth: RwLock::new(None),
        refresh: Mutex::new(()),
    });
    local.bootstrap().await?;
    local.refresh().await?;
    let background = local.clone();
    let task = tokio::spawn(async move {
        if background.catalog_auth.read().await.is_some() {
            match background.catalog_login().await {
                Ok(_) => eprintln!("Catalog: finish authentication in your browser"),
                Err(error) => eprintln!("Catalog: {}", setup_error(&error)),
            }
        }
        loop {
            tokio::time::sleep(Duration::from_secs(background.config.refresh_seconds)).await;
            if background.refresh().await.is_err() {
                eprintln!("Catalog: refresh failed; last valid definitions retained");
            }
        }
    });
    let service = Handler(local.clone())
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    task.abort();
    if let Some(t) = local.catalog_login.lock().await.take() {
        t.abort();
    }
    for e in local.servers.read().await.values() {
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
            etag: Mutex::new(None),
            catalog_session: Mutex::new(None),
            catalog_login: Mutex::new(None),
            catalog_auth: RwLock::new(None),
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
        let mut definition: Server = serde_json::from_value(json!({"id":"combined","endpoint":url,"oauth":{},"headers":[{"name":"X-Api-Key","credential":"api_key"},{"name":"X-Tenant","credential":"tenant"},{"name":"X-App","value":"temba"}]})).unwrap();
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
            toml::from_str("catalog_url='http://127.0.0.1:1'\nallow_loopback_http=true").unwrap();
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
        let results = local.search("mandatory").await;
        let server_row = results["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["server"] == "mandatory")
            .unwrap();
        assert_eq!(server_row["headers"][0]["mandatory"], false);
        assert_eq!(server_row["headers"][2]["mandatory"], true);
        assert_eq!(server_row["headers"][2]["credential"], "required");
        let error = local.credential_headers(&required.definition).unwrap_err();
        assert!(setup_error(&error).contains("X-Required (required)"));
        entry.close().await;
        server.abort();
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
