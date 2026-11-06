use super::*;
use futures::future::join_all;
use tokio::sync::Notify;

struct Source {
    name: String,
    engine: Arc<Local>,
    initialized: AtomicBool,
    changed: Notify,
    diagnostic: RwLock<Option<String>>,
}
impl Source {
    async fn wait(&self) {
        let changed = self.changed.notified();
        if !self.initialized.load(Ordering::SeqCst) {
            let _ = tokio::time::timeout(AUTH_COMPLETION_WAIT, changed).await;
        }
    }
    fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let source = self.clone();
        tokio::spawn(async move {
            loop {
                let loaded = match source.engine.bootstrap().await {
                    Ok(()) => source.engine.refresh().await,
                    Err(error) => {
                        *source.engine.catalog_status.write().await = "catalog_unavailable";
                        Err(error)
                    }
                };
                let diagnostic = loaded
                    .as_ref()
                    .err()
                    .and_then(|e| e.downcast_ref::<temba_core::DuplicateMcpName>())
                    .map(ToString::to_string);
                *source.diagnostic.write().await = diagnostic.clone();
                if loaded.is_err() {
                    let mut status = source.engine.catalog_status.write().await;
                    if !matches!(
                        *status,
                        "auth_required" | "auth_failed" | "auth_in_progress"
                    ) {
                        *status = "catalog_unavailable";
                    }
                    eprintln!(
                        "Catalog {}: refresh failed ({}); other catalogs remain usable",
                        source.name, *status
                    );
                    if let Some(message) = diagnostic {
                        eprintln!("Catalog {}: {message}", source.name);
                    }
                }
                source.initialized.store(true, Ordering::SeqCst);
                source.changed.notify_waiters();
                let normal = source.engine.config.refresh_minutes * 60;
                let seconds = if loaded.is_ok() {
                    normal
                } else {
                    normal.min(60)
                };
                tokio::time::sleep(Duration::from_secs(seconds)).await;
            }
        })
    }
}

struct Federation {
    sources: BTreeMap<String, Arc<Source>>,
    dynamic: Arc<Local>,
}
impl Federation {
    fn new(config: config::Config) -> Result<Arc<Self>> {
        let gate = Arc::new(Mutex::new(()));
        let dynamic = Local::new(config.dynamic_source(), gate.clone())?;
        let sources = config
            .into_sources()
            .into_iter()
            .map(|(id, config)| {
                let engine = Local::new(config, gate.clone())?;
                Ok((
                    id.clone(),
                    Arc::new(Source {
                        name: id,
                        engine,
                        initialized: AtomicBool::new(false),
                        changed: Notify::new(),
                        diagnostic: RwLock::new(None),
                    }),
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        ensure!(
            !sources.is_empty() || dynamic.config.dynamic_catalog.enabled,
            "No MCP sources configured. Run 'temba add catalog', add MCP entries to local_catalog.servers, or enable dynamic registration with 'temba set dynamic_catalog true'"
        );
        Ok(Arc::new(Self { sources, dynamic }))
    }
    async fn search(&self, query: &str, local_ids: &[String]) -> Value {
        if !local_ids.is_empty() {
            let mut result = self.dynamic.search(query, local_ids).await;
            result.as_object_mut().unwrap().remove("catalog");
            result
                .as_object_mut()
                .unwrap()
                .remove("catalog_connect_tool");
            result["catalogs"] = json!([]);
            result["partial"] = json!(false);
            result["scope"] = json!("dynamic");
            result["hint"] = json!(
                "Only supplied local_ids were inspected; no catalog login is needed. Empty or omitted query lists these MCPs without schemas. Use keywords with the same local_ids for detailed tools; use exact returned tool_id values."
            );
            return result;
        }
        let responses = join_all(self.sources.values().map(|source| async move {
            source.wait().await;
            (
                source.name.clone(),
                source.engine.search(query, &[]).await,
                source.diagnostic.read().await.clone(),
            )
        }))
        .await;
        let mut tools = Vec::new();
        let mut servers = Vec::new();
        let mut catalogs = Vec::new();
        let mut partial = false;
        let inventory = query.trim().is_empty();
        let mut total_matches = 0u64;
        let mut loaded_tools = 0u64;
        let mut truncated = false;
        for (id, response, diagnostic) in responses {
            let mut status = response["catalog"].clone();
            status["name"] = json!(id);
            if inventory {
                status["loaded_tools"] = response["loaded_tools"].clone();
                loaded_tools += response["loaded_tools"].as_u64().unwrap_or(0);
            } else {
                for key in ["total_matches", "returned_tools", "truncated", "limit"] {
                    status[key] = response[key].clone();
                }
                total_matches += response["total_matches"].as_u64().unwrap_or(0);
                truncated |= response["truncated"].as_bool().unwrap_or(false);
            }
            if let Some(message) = diagnostic {
                status["error"] = json!(message);
            }
            status["connect_tool"] = json!(
                response["catalog_connect_tool"]
                    .as_str()
                    .map(|value| format!("{id}.{value}"))
            );
            partial |= !matches!(status["status"].as_str(), Some("public" | "authenticated"));
            catalogs.push(status);
            for mut tool in response["tools"].as_array().cloned().unwrap_or_default() {
                let raw = tool["tool_id"].as_str().unwrap();
                tool["tool_id"] = json!(format!("{id}.{raw}"));
                tool["catalog"] = json!(id);
                if let Some(server) = tool["server"].as_str() {
                    tool["server"] = json!(format!("{id}.{server}"));
                }
                tools.push(tool);
            }
            for mut server in response["servers"].as_array().cloned().unwrap_or_default() {
                let raw = server["server"].as_str().unwrap();
                server["server"] = json!(format!("{id}.{raw}"));
                server["catalog"] = json!(id);
                servers.push(server);
            }
        }
        let returned_tools = tools.len();
        let mut response = json!({"mode":if inventory {"inventory"} else {"search"},"tools":tools,"servers":servers,"catalogs":catalogs,"partial":partial,"truncated":truncated,
            "hint":if inventory {"Visible MCP server inventory only; no tool descriptions or schemas. tool_count is unknown until a server is ready. Use keyword queries for tools and connection actions. partial reports catalog loading/login/unavailability."} else {"Use exact returned tool_id values. truncated means matching tools exceeded a per-catalog limit; refine the query. total_matches counts currently searchable tools and connection actions. partial reports catalog loading/login/unavailability. Catalog login cannot grant group membership."}});
        if inventory {
            response["loaded_tools"] = json!(loaded_tools);
        } else {
            response["total_matches"] = json!(total_matches);
            response["returned_tools"] = json!(returned_tools);
        }
        response
    }
}

#[derive(Clone)]
struct FederatedHandler(Arc<Federation>);
impl std::ops::Deref for FederatedHandler {
    type Target = Federation;
    fn deref(&self) -> &Federation {
        &self.0
    }
}
impl ServerHandler for FederatedHandler {
    fn get_info(&self) -> ServerConfig {
        Handler(self.dynamic.clone()).get_info().with_instructions("Use tool_search with an empty or omitted query to list MCP servers without tool schemas. Use keyword queries for tool details, then call_tool with an exact returned tool_id. Remote IDs include catalog, MCP and tool; never construct or shorten them. Each catalog has independent permissions, login and credentials. Inspect catalogs and partial for incomplete results; use a catalog's connect_tool only to retry its failed login. A catalog login is not a remote MCP login and cannot grant group access. Continue the original request after a connection returns ready. Missing mandatory headers must be configured locally; never request secrets in chat. Dynamic additions return secret local_ids: retain them only in the originating conversation and pass them to tool_search.local_ids; this scoped search does not trigger catalog login. Dynamic entries remain available until disconnected or the process exits.")
    }
    async fn list_tools(
        &self,
        p: Option<PaginatedRequestParams>,
        c: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut result = Handler(self.dynamic.clone()).list_tools(p, c).await?;
        for tool in &mut result.tools {
            if tool.name == "call_tool" {
                tool.description = Some("Call an exact tool_id returned by tool_search or add_mcp. Remote IDs are catalog.MCP.tool; each catalog and remote MCP has its own connection flow. Do not invent, shorten or exchange IDs across catalogs.".into());
            }
        }
        Ok(result)
    }
    async fn call_tool(
        &self,
        p: CallToolRequestParams,
        c: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = p.arguments.as_ref();
        if p.name == "tool_search" {
            let ids: Vec<String> = match args.and_then(|a| a.get("local_ids")) {
                Some(value) => match serde_json::from_value::<Vec<String>>(value.clone()) {
                    Ok(ids) if ids.len() <= 64 => ids,
                    _ => return Ok(failure("local_ids must contain at most 64 IDs")),
                },
                None => vec![],
            };
            let query = match args.and_then(|a| a.get("query")) {
                None => "",
                Some(Value::String(query)) => query,
                _ => {
                    return Ok(failure(
                        "query must be a string; use an empty string or omit it to list MCP servers",
                    ));
                }
            };
            return Ok(result(self.search(query, &ids).await));
        }
        if p.name == "call_tool" {
            let Some(id) = args.and_then(|a| a.get("tool_id")).and_then(Value::as_str) else {
                return Ok(failure("Missing tool_id"));
            };
            if !id.starts_with("__local_") {
                let Some((namespace, tool)) = id.split_once('.') else {
                    return Ok(failure(
                        "Use the exact catalog-qualified tool_id returned by tool_search",
                    ));
                };
                let Some(source) = self.sources.get(namespace) else {
                    return Ok(failure("Unknown catalog"));
                };
                source.wait().await;
                let arguments = args
                    .and_then(|a| a.get("arguments"))
                    .and_then(Value::as_object)
                    .cloned();
                return Ok(match source.engine.call(tool, arguments).await {
                    Ok(response) => {
                        if tool.split_once('.').is_some_and(|(_, name)| {
                            matches!(name, "__temba_connect__" | "__temba_disconnect__")
                        }) {
                            qualify_control(response, namespace)
                        } else {
                            response
                        }
                    }
                    Err(error) => failure(&setup_error(&error)),
                });
            }
        }
        Handler(self.dynamic.clone()).call_tool(p, c).await
    }
}

fn qualify_control(mut response: CallToolResponse, namespace: &str) -> CallToolResponse {
    if let CallToolResponse::Complete(result) = &mut response {
        for content in &mut result.content {
            if let ContentBlock::Text(text) = content
                && let Ok(mut data) = serde_json::from_str::<Value>(&text.text)
                && data.is_object()
            {
                data["catalog"] = json!(namespace);
                if let Some(server) = data["server"].as_str() {
                    data["server"] = json!(format!("{namespace}.{server}"));
                }
                text.text = data.to_string();
            }
        }
    }
    response
}

pub async fn run(config: config::Config) -> Result<()> {
    let federation = Federation::new(config)?;
    let tasks = federation
        .sources
        .values()
        .map(Source::start)
        .collect::<Vec<_>>();
    let result = async {
        let service = FederatedHandler(federation.clone())
            .serve(rmcp::transport::stdio())
            .await?;
        service.waiting().await?;
        Ok(())
    }
    .await;
    for task in tasks {
        task.abort();
    }
    for source in federation.sources.values() {
        source.engine.close().await;
    }
    federation.dynamic.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, post},
    };

    async fn fixture(
        label: &'static str,
        duplicate: bool,
    ) -> (String, String, Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let endpoint = format!("{base}/mcp");
        let reject = Arc::new(AtomicBool::new(false));
        let rejected = reject.clone();
        let public = json!({"name":"same","endpoint":endpoint,"headers":[{"name":"X-Key","credential":"key","mandatory":true}]});
        let private = json!({"name":"private","endpoint":endpoint,"access":{"authenticated":true},"oauth":{}});
        let router = Router::new()
            .route("/v1/bootstrap",get(||async{Json(json!({"schemaVersion":2,"catalogueAuth":null}))}))
            .route("/protected/v1/bootstrap",get(||async{Json(json!({"schemaVersion":2,"catalogueAuth":{"issuer":"https://identity.invalid/realm","clientId":"temba","callbackPort":19877}}))}))
            .route("/v1/catalog",get(move |headers:HeaderMap| {
                let public=public.clone();let private=private.clone();let reject=rejected.clone();
                async move {
                    assert!(!headers.contains_key("x-key"));
                    let mut servers=vec![public];
                    if let Some(token)=headers.get("authorization") {
                        assert_eq!(token.to_str().unwrap(),format!("Bearer catalog-{label}-token"));
                        if reject.load(Ordering::SeqCst) { return StatusCode::UNAUTHORIZED.into_response(); }
                        servers.push(private);
                    }
                    Json(json!({"schemaVersion":2,"servers":servers})).into_response()
                }
            }))
            .route("/mcp",post(move |headers:HeaderMap,Json(request):Json<Value>|async move {
                assert_eq!(headers["x-key"].to_str().unwrap(),format!("private-{label}"));
                let Some(id)=request.get("id") else{return StatusCode::ACCEPTED.into_response()};
                let result=match request["method"].as_str().unwrap() {
                    "initialize"=>json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"1"}}),
                    "tools/list"=> {
                        let tools=(0..7).map(|i|json!({"name":if duplicate {"same".to_owned()}else{format!("operation_{i}.subtask")},"description":"operation","inputSchema":{"type":"object"}})).collect::<Vec<_>>();
                        json!({"tools":tools})
                    }
                    "tools/call"=>json!({"content":[{"type":"text","text":format!("response-{label}") }]}),
                    _=>panic!("unexpected method"),
                };
                Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
            }).get(||async{StatusCode::METHOD_NOT_ALLOWED}));
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, endpoint, reject, task)
    }
    fn config(a: &str, ae: &str, b: &str, be: &str) -> config::Config {
        let mut config:config::Config=serde_yaml_ng::from_str(&format!("allow_insecure_http: true\nsearch_limit: 5\ncatalogs:\n- name: first\n  url: {a}\n  tools:\n    same:\n      expected_endpoint: {ae}\n      headers:\n        X-Key: private-first\n- name: second\n  url: {b}\n  tools:\n    same:\n      expected_endpoint: {be}\n      headers:\n        X-Key: private-second\n- name: offline\n  url: http://127.0.0.1:1\ndynamic_catalog:\n  enabled: true\n")).unwrap();
        config.validate().unwrap();
        config
    }
    #[tokio::test]
    async fn remote_duplicate_names_are_rejected_and_reported_to_the_caller() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/v1/bootstrap",
                get(|| async { Json(json!({"schemaVersion":2,"catalogueAuth":null})) }),
            )
            .route(
                "/v1/catalog",
                get(|| async {
                    Json(json!({"schemaVersion":2,"servers":[
                        {"name":"repeated","endpoint":"https://first.example/mcp"},
                        {"name":"repeated","endpoint":"https://second.example/mcp"}
                    ]}))
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config: config::Config = serde_yaml_ng::from_str(&format!(
            "allow_insecure_http: true\ncatalogs: [{{name: team, url: 'http://{address}'}}]"
        ))
        .unwrap();
        config.validate().unwrap();
        let federation = Federation::new(config).unwrap();
        let task = federation.sources["team"].start();
        tokio::time::timeout(Duration::from_secs(3), federation.sources["team"].wait())
            .await
            .unwrap();
        let response = federation.search("anything", &[]).await;
        assert_eq!(response["partial"], true);
        assert!(
            response["catalogs"][0]["error"]
                .as_str()
                .unwrap()
                .contains("duplicate MCP name 'repeated'")
        );
        assert!(response["tools"].as_array().unwrap().is_empty());
        task.abort();
        federation.sources["team"].engine.close().await;
        server.abort();
    }
    #[tokio::test]
    async fn remote_label_filters_are_isolated_and_local_has_no_filter() {
        let definitions = json!({"schemaVersion":2,"servers":[
            {"name":"plain","endpoint":"https://mcp.invalid/plain","oauth":{}},
            {"name":"support","endpoint":"https://mcp.invalid/support","labels":["support"],"oauth":{}},
            {"name":"tech","endpoint":"https://mcp.invalid/tech","labels":["tech"],"oauth":{}},
            {"name":"both","endpoint":"https://mcp.invalid/both","labels":["support","tech"],"oauth":{}}
        ]});
        let router = Router::new().route(
            "/v1/catalog",
            get(move || {
                let definitions = definitions.clone();
                async move { Json(definitions) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut config: config::Config = serde_yaml_ng::from_str(&format!(
            r#"
allow_insecure_http: true
catalogs:
  - name: first
    url: http://{address}
    labels: [support]
  - name: second
    url: http://{address}
    labels: [tech]
  - name: defaults
    url: http://{address}
local_catalog:
  enabled: true
  servers:
    - name: placeholder
      endpoint: https://mcp.invalid/mcp
      access: {{public: true}}
"#
        ))
        .unwrap();
        config.validate().unwrap();
        config.local_catalog.as_mut().unwrap().address = address;
        let federation = Federation::new(config).unwrap();
        for (id, expected) in [
            ("first", vec!["plain", "support"]),
            ("second", vec!["plain", "tech"]),
            ("defaults", vec!["plain"]),
            ("local", vec!["both", "plain", "support", "tech"]),
        ] {
            let engine = &federation.sources[id].engine;
            engine.refresh().await.unwrap();
            assert_eq!(
                engine
                    .servers
                    .read()
                    .await
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                expected
            );
            if id != "local" {
                assert!(engine.call("both.__temba_connect__", None).await.is_err());
            }
            engine.close().await;
        }
        server.abort();
    }
    #[tokio::test]
    async fn combines_five_per_catalog_without_id_credential_session_or_failure_collisions() {
        let (a, ae, reject_a, ta) = fixture("first", false).await;
        let (b, be, _, tb) = fixture("second", false).await;
        let federation = Federation::new(config(&a, &ae, &b, &be)).unwrap();
        for source in federation.sources.values() {
            if source.name == "offline" {
                *source.engine.catalog_status.write().await = "catalog_unavailable";
            } else {
                source.engine.bootstrap().await.unwrap();
                source.engine.refresh().await.unwrap();
            }
            source.initialized.store(true, Ordering::SeqCst);
        }
        let result = federation.search("operation", &[]).await;
        assert_eq!(result["tools"].as_array().unwrap().len(), 10);
        assert_eq!(result["total_matches"], 14);
        assert_eq!(result["returned_tools"], 10);
        assert_eq!(result["truncated"], true);
        let first = result["catalogs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "first")
            .unwrap();
        assert_eq!(first["total_matches"], 7);
        assert_eq!(first["truncated"], true);
        let inventory = federation.search("", &[]).await;
        assert_eq!(inventory["mode"], "inventory");
        assert_eq!(inventory["loaded_tools"], 14);
        assert_eq!(inventory["servers"].as_array().unwrap().len(), 2);
        assert!(inventory["tools"].as_array().unwrap().is_empty());
        assert_eq!(inventory["truncated"], false);
        for id in ["first", "second"] {
            assert_eq!(
                result["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|t| t["catalog"] == id)
                    .count(),
                5
            );
        }
        assert_eq!(result["partial"], true);
        assert!(!result.to_string().contains("private-first"));
        let (front, back) = tokio::io::duplex(65536);
        let f = federation.clone();
        let server = tokio::spawn(async move {
            FederatedHandler(f)
                .serve(tokio::io::split(back))
                .await
                .unwrap()
        });
        let client = ().serve(tokio::io::split(front)).await.unwrap();
        let server = server.await.unwrap();
        let advertised = client.peer().list_all_tools().await.unwrap();
        let search = advertised.iter().find(|t| t.name == "tool_search").unwrap();
        assert_eq!(search.description, search::tool().description);
        assert!(
            search
                .description
                .as_ref()
                .unwrap()
                .starts_with("List MCPs with query=\"\"")
        );
        assert!(search.input_schema.get("required").is_none());
        assert!(
            search.input_schema["properties"]["query"]["description"]
                .as_str()
                .unwrap()
                .contains("without tool descriptions")
        );
        let omitted = client
            .peer()
            .call_tool(CallToolRequestParams::new("tool_search"))
            .await
            .unwrap();
        let omitted: Value =
            serde_json::from_str(&omitted.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(omitted["mode"], "inventory");
        assert!(omitted["tools"].as_array().unwrap().is_empty());
        for namespace in ["first", "second"] {
            let response = client
                .peer()
                .call_tool(
                    CallToolRequestParams::new("call_tool").with_arguments(
                        json!({"tool_id":format!("{namespace}.same.operation_0.subtask")})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .unwrap();
            assert!(
                serde_json::to_string(&response)
                    .unwrap()
                    .contains(&format!("response-{namespace}"))
            );
            let source = &federation.sources[namespace];
            *source.engine.catalog_session.lock().await = Some(oauth::Session::test_session(
                &format!("catalog-{namespace}-token"),
            ));
            source.engine.refresh().await.unwrap();
            assert!(source.engine.servers.read().await.contains_key("private"));
        }
        reject_a.store(true, Ordering::SeqCst);
        assert!(federation.sources["first"].engine.refresh().await.is_err());
        assert!(
            !federation.sources["first"]
                .engine
                .servers
                .read()
                .await
                .contains_key("private")
        );
        assert!(
            federation.sources["second"]
                .engine
                .servers
                .read()
                .await
                .contains_key("private")
        );
        assert_eq!(
            federation.sources["second"]
                .engine
                .catalog_session
                .lock()
                .await
                .as_ref()
                .unwrap()
                .access()
                .await
                .unwrap()
                .as_str(),
            "catalog-second-token"
        );
        client.cancel().await.unwrap();
        server.cancel().await.unwrap();
        for source in federation.sources.values() {
            source.engine.close().await;
        }
        ta.abort();
        tb.abort();
    }
    #[tokio::test]
    async fn dynamic_search_is_private_and_does_not_report_fake_catalog_authentication() {
        let mut config: config::Config =
            serde_yaml_ng::from_str("dynamic_catalog:\n  enabled: true\n").unwrap();
        config.validate().unwrap();
        let federation = Federation::new(config).unwrap();
        let definition = serde_json::from_value(
            json!({"name":"temporary", "endpoint":"https://mcp.invalid/mcp", "oauth":{}}),
        )
        .unwrap();
        let added = federation
            .dynamic
            .add_personal(definition, BTreeMap::new())
            .await
            .unwrap();
        let id = added["local_id"].as_str().unwrap().to_owned();
        assert!(
            federation.search("", &[]).await["tools"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let result = federation.search("", std::slice::from_ref(&id)).await;
        assert_eq!(result["scope"], "dynamic");
        assert!(result.get("catalog").is_none());
        assert!(result["catalogs"].as_array().unwrap().is_empty());
        assert_eq!(result["mode"], "inventory");
        assert!(result["tools"].as_array().unwrap().is_empty());
        assert_eq!(result["servers"].as_array().unwrap().len(), 1);
        federation.dynamic.close().await;
    }

    #[tokio::test]
    async fn duplicate_upstream_tool_names_are_rejected() {
        let (base, endpoint, _, task) = fixture("first", true).await;
        let f = Federation::new(config(&base, &endpoint, &base, &endpoint)).unwrap();
        let source = &f.sources["first"];
        source.engine.bootstrap().await.unwrap();
        source.engine.refresh().await.unwrap();
        let _ = wait_for_connection(
            || async {
                source.engine.servers.read().await["same"]
                    .state
                    .read()
                    .await
                    .status
            },
            Duration::from_secs(3),
        )
        .await;
        assert_eq!(
            source.engine.servers.read().await["same"]
                .state
                .read()
                .await
                .status,
            "connection_failed"
        );
        source.engine.close().await;
        task.abort();
    }
    #[tokio::test]
    async fn background_authentication_rejection_remains_auth_required_not_unavailable() {
        let auth = oauth::CatalogAuth {
            issuer: "https://identity.invalid".into(),
            client_id: "temba".into(),
            callback_port: 19877,
            token_endpoint: String::new(),
        };
        let router = Router::new()
            .route("/v1/bootstrap", get(|| async { Json(json!({"schemaVersion":2,"catalogueAuth":{"issuer":"https://identity.invalid","clientId":"temba","callbackPort":19877}})) }))
            .route("/v1/catalog", get(|| async { StatusCode::UNAUTHORIZED }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let config =
            serde_yaml_ng::from_str(&format!("catalog_url: {base}\nallow_insecure_http: true\n"))
                .unwrap();
        let engine = Local::new(config, Arc::new(Mutex::new(()))).unwrap();
        *engine.catalog_auth.write().await = Some(auth);
        *engine.catalog_session.lock().await = Some(oauth::Session::test_session("rejected-token"));
        *engine.catalog_status.write().await = "authenticated";
        let source = Arc::new(Source {
            name: "test".into(),
            engine,
            initialized: AtomicBool::new(false),
            changed: Notify::new(),
            diagnostic: RwLock::new(None),
        });
        let task = source.start();
        source.wait().await;
        assert_eq!(*source.engine.catalog_status.read().await, "auth_required");
        assert!(source.engine.catalog_session.lock().await.is_none());
        assert!(source.engine.catalog_login.lock().await.is_none());
        task.abort();
        source.engine.close().await;
        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn queued_login_does_not_open_a_delayed_browser_after_request_has_returned() {
        let config: config::SourceConfig =
            serde_yaml_ng::from_str("catalog_url: https://catalog.invalid\n").unwrap();
        let gate = Arc::new(Mutex::new(()));
        let guard = gate.clone().lock_owned().await;
        let engine = Local::new(config, gate).unwrap();
        *engine.catalog_auth.write().await = Some(oauth::CatalogAuth {
            issuer: "https://identity.invalid".into(),
            client_id: "temba".into(),
            callback_port: 19877,
            token_endpoint: String::new(),
        });
        engine.start_catalog_login().await.unwrap();
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        assert_eq!(*engine.catalog_status.read().await, "auth_required");
        assert!(engine.catalog_login_url.read().await.is_none());
        assert!(
            engine
                .catalog_login
                .lock()
                .await
                .as_ref()
                .unwrap()
                .is_finished()
        );
        drop(guard);
        engine.close().await;
    }

    #[tokio::test]
    async fn catalog_logins_queue_without_binding_the_same_callback_port() {
        let (base, endpoint, _, task) = fixture("first", false).await;
        let f = Federation::new(config(&base, &endpoint, &base, &endpoint)).unwrap();
        let gate = f.sources["first"].engine.catalog_login_gate.clone();
        let guard = gate.clone().lock_owned().await;
        for namespace in ["first", "second"] {
            let mut config: config::SourceConfig = serde_yaml_ng::from_str(&format!(
                "catalog_url: {base}/protected\nallow_insecure_http: true\n"
            ))
            .unwrap();
            config.validate().unwrap();
            let engine = Local::new(config, gate.clone()).unwrap();
            let response = engine.start_catalog_login().await.unwrap();
            assert_eq!(response["status"], "auth_in_progress");
            assert!(response["url"].is_null());
            assert!(
                !engine
                    .catalog_login
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .is_finished(),
                "{namespace}"
            );
            engine.close().await;
        }
        drop(guard);
        task.abort();
    }
}
