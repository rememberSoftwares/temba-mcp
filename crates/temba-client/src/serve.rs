use super::config::Config;
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use std::sync::Arc;
use temba_core::Catalog;

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
pub async fn run(mut config: Config) -> Result<()> {
    let local = config.local_catalog.take().ok_or_else(|| {
        anyhow::anyhow!(
            "Configure local_catalog with enabled: true and servers before running serve"
        )
    })?;
    ensure!(
        local.enabled,
        "Local catalog is disabled; set local_catalog.enabled: true"
    );
    let super::config::StaticCatalog {
        listen,
        servers,
        tools,
        ..
    } = local;
    drop(tools);
    ensure!(
        servers.iter().all(|s| s.access.public),
        "The static server supports public definitions only"
    );
    let mut catalog = Catalog {
        schema_version: 2,
        revision: String::new(),
        servers,
    };
    catalog.validate(config.allow_insecure_http)?;
    drop(config);
    let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| {
        format!("Cannot start local catalog on {listen}; check that the address is available and the port is not already in use")
    })?;
    eprintln!(
        "Temba static catalog listening on {}; {} definitions; configuration is read only at startup",
        listener.local_addr()?,
        catalog.servers.len()
    );
    axum::serve(listener, router(catalog))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
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
            servers: config.local_catalog.unwrap().servers,
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
