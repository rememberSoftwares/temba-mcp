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
use temba_core::{Auth, Catalog, Server};
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
        let status = match definition.auth {
            Auth::Oauth { .. } => "auth_required",
            _ => "connecting",
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
        ensure!(r["schemaVersion"] == 1, "Unsupported bootstrap version");
        let auth: Option<oauth::CatalogAuth> = serde_json::from_value(r["auth"].clone())?;
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
                if !matches!(e.definition.auth, Auth::Oauth { .. }) {
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
        let total = connect.len();
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
            let mut done = 0;
            while let Some(e) = pending.next().await {
                done += 1;
                eprintln!(
                    "MCP {}: {} ({done}/{total})",
                    e.definition.id,
                    e.state.read().await.status
                );
            }
        });
        Ok(())
    }
    async fn drop_authenticated(&self) {
        *self.etag.lock().await = None;
        let mut servers = self.servers.write().await;
        let removed: Vec<_> = servers
            .values()
            .filter(|e| e.definition.catalog_access == "authenticated")
            .cloned()
            .collect();
        servers.retain(|_, e| e.definition.catalog_access == "public");
        drop(servers);
        for e in removed {
            e.retired.store(true, Ordering::SeqCst);
            e.close().await;
        }
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
        let attempt = async {
            let mut builder = reqwest::Client::builder()
                .user_agent("Temba/1.0")
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10));
            if let Auth::Token {
                header_name,
                prefix,
            } = &e.definition.auth
            {
                let credential = self
                    .config
                    .credentials
                    .get(&e.definition.id)
                    .ok_or_else(|| anyhow::anyhow!("Local credential mapping missing"))?;
                ensure!(
                    credential.expected_endpoint == e.definition.url,
                    "Static credential destination mismatch"
                );
                let token = Zeroizing::new(std::env::var(&credential.token_env)?);
                ensure!(!token.is_empty(), "Empty static token");
                let mut headers = reqwest::header::HeaderMap::new();
                let mut v =
                    reqwest::header::HeaderValue::from_str(&format!("{prefix}{}", token.as_str()))?;
                v.set_sensitive(true);
                headers.insert(
                    reqwest::header::HeaderName::from_bytes(header_name.as_bytes())?,
                    v,
                );
                builder = builder.default_headers(headers);
            }
            let client = transport::Http {
                client: builder.build()?,
                session,
                endpoint: Arc::from(e.definition.url.as_str()),
            };
            let config = StreamableHttpClientTransportConfig::with_uri(e.definition.url.clone())
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
    }
    async fn login(self: &Arc<Self>, e: Arc<Entry>) -> Result<Value> {
        let mut task = e.login.lock().await;
        if task.as_ref().is_some_and(|t| !t.is_finished()) {
            return Ok(json!({"status":"auth_in_progress","server":e.definition.id}));
        }
        let Auth::Oauth {
            client_id,
            scopes,
            callback_port,
            allowed_origins,
        } = &e.definition.auth
        else {
            drop(task);
            self.connect(&e).await;
            return Ok(json!({"status":e.state.read().await.status}));
        };
        let mut origins = allowed_origins.clone();
        origins.push(e.definition.url.clone());
        let provider = oauth::discover(
            &self.http,
            &e.definition.url,
            &origins,
            self.config.allow_loopback_http,
        )
        .await?;
        let pending = oauth::Pending::new(
            self.http.clone(),
            provider,
            client_id.clone(),
            scopes.clone(),
            Some(e.definition.url.clone()),
            *callback_port,
        )
        .await?;
        let url = pending.url.clone();
        e.state.write().await.status = "auth_in_progress";
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
                    eprintln!(
                        "MCP {}: {}",
                        entry.definition.id,
                        entry.state.read().await.status
                    );
                }
                Err(_) => {
                    entry.state.write().await.status = "auth_required";
                    eprintln!(
                        "MCP {}: authorization failed or timed out",
                        entry.definition.id
                    );
                }
            }
        }));
        let browser_url = url.clone();
        tokio::task::spawn_blocking(move || {
            let _ = open::that(browser_url);
        });
        Ok(
            json!({"status":"auth_in_progress","server":e.definition.id,"url":url,"hint":"Finish authorization in your browser, then repeat tool_search. Tokens remain in this local process."}),
        )
    }
    async fn catalog_login(self: &Arc<Self>) -> Result<Value> {
        let mut task = self.catalog_login.lock().await;
        if task.as_ref().is_some_and(|t| !t.is_finished()) {
            return Ok(json!({"status":"auth_in_progress"}));
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
                    *local.catalog_session.lock().await = Some(session);
                    *local.etag.lock().await = None;
                    if local.refresh().await.is_err() {
                        eprintln!("Catalog: authenticated refresh failed");
                    }
                }
                Err(_) => eprintln!("Catalog: authorization failed or timed out"),
            }
        }));
        let browser_url = url.clone();
        tokio::task::spawn_blocking(move || {
            let _ = open::that(browser_url);
        });
        Ok(json!({"status":"auth_in_progress","url":url}))
    }
    async fn search(&self, query: &str) -> Value {
        let entries = self.servers.read().await.clone();
        let mut rows = vec![];
        let mut docs = vec![];
        let mut states = vec![];
        for e in entries.values() {
            let s = e.state.read().await;
            states.push(json!({"server":e.definition.id,"status":s.status}));
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
                rows.push(json!({"tool_id":format!("{}.__temba_connect__",e.definition.id),"description":e.definition.description,"status":s.status}));
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
        json!({"tools":tools,"servers":states,"catalog_connect_tool":if self.catalog_auth.read().await.is_some(){Some("__catalog__.__temba_connect__")}else{None}})
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
fn bail_catalog() {
    eprintln!("Catalog: authorization rejected; reconnect using __catalog__.__temba_connect__");
}
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions("Use tool_search to find tools, then call_tool with its tool_id. Connection actions open a browser; wait for authorization and search again.")
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools=vec![Tool::new("tool_search","Search tools and inspect remote connection states",json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}).as_object().unwrap().clone()),Tool::new("call_tool","Call a remote tool or connect/disconnect action",json!({"type":"object","properties":{"tool_id":{"type":"string"},"arguments":{"type":"object"}},"required":["tool_id"],"additionalProperties":false}).as_object().unwrap().clone())];
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
    let mut config: config::Config =
        toml::from_str(&tokio::fs::read_to_string(args.config).await?)?;
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
    }
    async fn mcp(
        State(m): State<Arc<Mock>>,
        headers: HeaderMap,
        Json(request): Json<Value>,
    ) -> Response {
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
        let mut config:config::Config=toml::from_str(&format!("catalog_url='http://127.0.0.1:1'\nallow_loopback_http=true\n[credentials.secure]\ntoken_env='PATH'\nexpected_endpoint='{a}'")).unwrap();
        config.validate().unwrap();
        let local = local(config);
        let definition = |id: &str, url: &str, auth: Value| {
            serde_json::from_value::<Server>(json!({"id":id,"url":url,"auth":auth})).unwrap()
        };
        let ea = Arc::new(Entry::new(definition(
            "secure",
            &a,
            json!({"type":"token"}),
        )));
        let eb = Arc::new(Entry::new(definition("public", &b, json!({"type":"none"}))));
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
            secure.requests.lock().await.iter().all(
                |v| v.as_deref() == Some(&format!("Bearer {}", std::env::var("PATH").unwrap()))
            )
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
            json!({"type":"token"}),
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
    async fn catalog_refresh_is_atomic_and_removes_changed_connections() {
        let (url, _, tm) = mock().await;
        let catalog = Arc::new(RwLock::new(
            json!({"schemaVersion":1,"servers":[{"id":"one","url":url,"labels":["allowed"]}]}),
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
        *catalog.write().await =
            json!({"schemaVersion":1,"servers":[{"id":"two","url":"https://user:pass@bad/mcp"}]});
        assert!(local.refresh().await.is_err());
        assert!(Arc::ptr_eq(&old, &local.servers.read().await["one"]));
        *catalog.write().await = json!({"schemaVersion":1,"servers":[]});
        local.refresh().await.unwrap();
        assert!(local.servers.read().await.is_empty());
        assert_eq!(old.state.read().await.status, "disconnected");
        assert!(local.call("one.echo", None).await.is_err());
        tc.abort();
        tm.abort();
    }
}
