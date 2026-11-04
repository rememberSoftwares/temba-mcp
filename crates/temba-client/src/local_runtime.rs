use super::config::Config;
use crate::{
    local_server::LocalServer,
    stdio_gateway::{Gateway, Handler},
};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager, tower::StreamableHttpServerConfig,
};
use std::sync::Arc;
use temba_core::Catalog;
use tokio_util::sync::CancellationToken;

fn router(catalog: Catalog) -> Router {
    Router::new()
        .route(
            "/v1/bootstrap",
            get(|| async { Json(serde_json::json!({"schemaVersion":2,"catalogueAuth":null})) }),
        )
        .route("/v1/catalog", get(list))
        .route("/healthz", get(|| async { StatusCode::OK }))
        .with_state(Arc::new(catalog))
}
async fn list(State(catalog): State<Arc<Catalog>>, headers: HeaderMap) -> Response {
    let etag = format!("\"{}\"", catalog.revision);
    let mut response = if headers
        .get("if-none-match")
        .is_some_and(|h| h == etag.as_str())
    {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Json(catalog.as_ref()).into_response()
    };
    response
        .headers_mut()
        .insert("etag", HeaderValue::from_str(&etag).unwrap());
    response.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("private, no-cache"),
    );
    response
}
fn application(
    local: &super::config::StaticCatalog,
    shutdown: &CancellationToken,
) -> Result<(Router, Vec<Arc<Gateway>>)> {
    let mut catalog = Catalog {
        schema_version: 2,
        revision: String::new(),
        servers: local
            .servers
            .iter()
            .map(|s| s.public_definition(local.address))
            .collect(),
    };
    catalog.validate(true)?;
    let mut app = router(catalog);
    let mut gateways = Vec::new();
    for server in &local.servers {
        if let LocalServer::Stdio(spec) = server {
            let route = format!("/mcp/{}", spec.id);
            let gateway = Gateway::new(spec.as_ref().clone(), shutdown.clone());
            let shared = gateway.clone();
            let service = StreamableHttpService::new(
                move || Ok(Handler(shared.clone())),
                Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default()
                    .enforce_origin_validation()
                    .with_cancellation_token(shutdown.child_token()),
            );
            app = app.nest_service(&route, service);
            gateways.push(gateway);
        }
    }
    Ok((app, gateways))
}
/// Private runtime owned by one CLI/MCP process. The loopback address is allocated
/// automatically and is never accepted from configuration.
pub struct Embedded {
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    gateways: Vec<Arc<Gateway>>,
}
impl Embedded {
    pub async fn start(config: &mut Config) -> Result<Option<Self>> {
        let Some(local) = config
            .local_catalog
            .as_mut()
            .filter(|c| c.enabled && !c.servers.is_empty())
        else {
            return Ok(None);
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        local.address = listener.local_addr()?;
        let shutdown = CancellationToken::new();
        let (app, gateways) = application(local, &shutdown)?;
        let stop = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { stop.cancelled().await })
                .await
        });
        Ok(Some(Self {
            shutdown,
            task,
            gateways,
        }))
    }
    pub async fn close(self) -> Result<()> {
        self.shutdown.cancel();
        futures::future::join_all(self.gateways.iter().map(|g| g.close())).await;
        // Sessions may still be held by a failed caller. Do not delay process exit.
        let mut task = self.task;
        if tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
        Ok(())
    }
}
pub async fn termination_signal() {
    #[cfg(unix)]
    if let Ok(mut terminate) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    } else {
        let _ = tokio::signal::ctrl_c().await;
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn serves_iac_snapshot_with_etag_and_no_management_api() {
        let mut config: Config = serde_yaml_ng::from_str(
            "local_catalog:\n  servers:\n  - id: service\n    endpoint: https://mcp.example/mcp\n",
        )
        .unwrap();
        config.validate().unwrap();
        let mut catalog = Catalog {
            schema_version: 2,
            revision: String::new(),
            servers: config
                .local_catalog
                .unwrap()
                .servers
                .iter()
                .map(|s| s.public_definition("127.0.0.1:8787".parse().unwrap()))
                .collect(),
        };
        catalog.validate(false).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task =
            tokio::spawn(async move { axum::serve(listener, router(catalog)).await.unwrap() });
        let http = super::super::http().unwrap();
        let bootstrap: serde_json::Value = http
            .get(format!("{base}/v1/bootstrap"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(bootstrap["catalogueAuth"].is_null());
        let response = http.get(format!("{base}/v1/catalog")).send().await.unwrap();
        let etag = response.headers()["etag"].clone();
        let data: Catalog = response.json().await.unwrap();
        assert_eq!(data.servers[0].id, "service");
        assert_eq!(
            http.get(format!("{base}/v1/catalog"))
                .header("if-none-match", etag)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            http.post(format!("{base}/v1/catalog"))
                .json(&serde_json::json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            http.get(format!("{base}/v1/admin/servers"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        task.abort();
    }
}

#[cfg(test)]
mod gateway_tests {
    use super::*;
    use rmcp::{
        ServiceExt,
        model::CallToolRequestParams,
        transport::streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
        },
    };
    use serde_json::{Value, json};
    #[tokio::test]
    async fn automatic_catalog_is_private_handles_mixed_entries_and_isolates_instances() {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stdio_mcp.py");
        let yaml = json!({"local_catalog":{"enabled":true,"servers":[
            {"id":"program","command":if cfg!(windows) {"python"} else {"python3"},"args":[fixture]},
            {"id":"remote","endpoint":"https://mcp.example.com/mcp"}
        ]}});
        let mut runtimes = Vec::new();
        let mut addresses = Vec::new();
        for _ in 0..2 {
            let mut config: Config = serde_json::from_value(yaml.clone()).unwrap();
            config.validate().unwrap();
            let runtime = Embedded::start(&mut config).await.unwrap().unwrap();
            let address = config.local_catalog.as_ref().unwrap().address;
            assert!(address.ip().is_loopback());
            let (_, source) = config.into_sources().pop().unwrap();
            let catalog = crate::catalog::load(&source).await.unwrap();
            assert_eq!(catalog.servers[1].endpoint, "https://mcp.example.com/mcp");
            assert_eq!(
                catalog.servers[0].endpoint,
                format!("http://{address}/mcp/program")
            );
            let service = ()
                .serve(StreamableHttpClientTransport::with_client(
                    crate::http().unwrap(),
                    StreamableHttpClientTransportConfig::with_uri(
                        catalog.servers[0].endpoint.clone(),
                    ),
                ))
                .await
                .unwrap();
            let result = service
                .peer()
                .call_tool(CallToolRequestParams::new("echo"))
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            service.cancel().await.unwrap();
            addresses.push(address);
            runtimes.push(runtime);
        }
        assert_ne!(addresses[0], addresses[1]);
        for runtime in runtimes {
            runtime.close().await.unwrap();
        }
        for address in addresses {
            assert!(tokio::net::TcpStream::connect(address).await.is_err());
        }
        for yaml in [
            "{}",
            "local_catalog: {enabled: false}",
            "local_catalog: {enabled: true}",
        ] {
            let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
            assert!(Embedded::start(&mut config).await.unwrap().is_none());
        }
    }
    #[tokio::test]
    async fn http_gateway_discovers_calls_and_isolates_processes_without_publishing_private_configuration()
     {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stdio_mcp.py");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut config: Config = serde_json::from_value(json!({"local_catalog":{"enabled":true,"servers":[
            {"id":"first","command":if cfg!(windows) {"python"} else {"python3"},"args":[fixture],"env":{"FIXTURE_TOKEN":"private-first"}},
            {"id":"second","command":if cfg!(windows) {"python"} else {"python3"},"args":[fixture],"env":{"FIXTURE_TOKEN":"private-second"}},
            {"id":"network","endpoint":"https://mcp.invalid/mcp"}
        ]}})).unwrap();
        config.validate().unwrap();
        let shutdown = CancellationToken::new();
        config.local_catalog.as_mut().unwrap().address = address;
        let (app, gateways) =
            application(config.local_catalog.as_ref().unwrap(), &shutdown).unwrap();
        let stop = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { stop.cancelled().await })
                .await
                .unwrap()
        });
        let http = crate::http().unwrap();
        let snapshot = http
            .get(format!("http://{address}/v1/catalog"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(snapshot.contains(&format!("http://{address}/mcp/first")));
        for private in [
            "command",
            "private-first",
            "private-second",
            "stdio_mcp.py",
            "FIXTURE_TOKEN",
            "envFromEnv",
        ] {
            assert!(!snapshot.contains(private));
        }
        assert_eq!(
            serde_json::from_str::<Value>(&snapshot).unwrap()["servers"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        for header in ["origin", "host"] {
            let response = http
                .post(format!("http://{address}/mcp/first"))
                .header(header, "https://untrusted.invalid")
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_client_error());
        }
        let mut services = Vec::new();
        let mut pids = Vec::new();
        for id in ["first", "first", "second"] {
            let config =
                StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp/{id}"));
            let service = ()
                .serve(StreamableHttpClientTransport::with_client(
                    crate::http().unwrap(),
                    config,
                ))
                .await
                .unwrap();
            let tools = service.peer().list_all_tools().await.unwrap();
            assert_eq!(tools[0].name, "echo");
            let result =
                service
                    .peer()
                    .call_tool(CallToolRequestParams::new("echo").with_arguments(
                        json!({"value":"through-http"}).as_object().unwrap().clone(),
                    ))
                    .await
                    .unwrap();
            let data: Value =
                serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(data["literal"], format!("private-{id}"));
            assert_eq!(data["arguments"]["value"], "through-http");
            pids.push(data["pid"].clone());
            services.push(service);
        }
        assert_eq!(pids[0], pids[1]);
        assert_ne!(pids[0], pids[2]);
        for service in services {
            service.cancel().await.unwrap();
        }
        shutdown.cancel();
        for gateway in gateways {
            gateway.close().await;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
