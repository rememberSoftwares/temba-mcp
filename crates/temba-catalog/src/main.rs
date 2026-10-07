mod auth;
use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use clap::Parser;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use subtle::ConstantTimeEq;
use temba_core::{Catalog, Server};
use tokio::sync::RwLock;
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(
    version,
    about = "Central catalog: definitions only, no upstream connections"
)]
struct Args {
    #[arg(long, env = "CATALOG_PATH", default_value = "catalog.yaml")]
    config: String,
    #[arg(long, env = "PORT", default_value_t = 8080)]
    port: u16,
    #[arg(long, env = "ALLOW_INSECURE_HTTP", default_value_t = false)]
    allow_http: bool,
    #[arg(long, env = "ADMIN_MUTABLE", default_value_t = false)]
    mutable: bool,
}
struct App {
    catalog: Catalog,
    additions: RwLock<BTreeMap<String, Server>>,
    auth: auth::Verifier,
    admin: Zeroizing<String>,
    mutable: bool,
    allow_http: bool,
}
type Shared = Arc<App>;
type ApiResult = Result<Response, StatusCode>;
fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}
fn administrator(app: &App, h: &HeaderMap) -> bool {
    !app.admin.is_empty()
        && bearer(h).is_some_and(|s| bool::from(s.as_bytes().ct_eq(app.admin.as_bytes())))
}
async fn catalog_handler(State(app): State<Shared>, h: HeaderMap) -> ApiResult {
    let authenticated = match h.get("authorization") {
        Some(_) => {
            app.auth
                .verify(bearer(&h).ok_or(StatusCode::UNAUTHORIZED)?)
                .await
                .map_err(|_| StatusCode::UNAUTHORIZED)?;
            true
        }
        None => false,
    };
    let mut servers = app.catalog.servers.clone();
    servers.extend(app.additions.read().await.values().cloned());
    servers.retain(|s| s.catalog_access == "public" || authenticated);
    let mut catalog = Catalog {
        schema_version: 1,
        revision: String::new(),
        servers,
    };
    catalog
        .validate(app.allow_http)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let etag = format!("\"{}\"", catalog.revision);
    let mut r = if h.get("if-none-match").is_some_and(|v| v == etag.as_str()) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Json(catalog).into_response()
    };
    r.headers_mut()
        .insert("etag", HeaderValue::from_str(&etag).unwrap());
    r.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("private, no-cache"),
    );
    r.headers_mut()
        .insert("vary", HeaderValue::from_static("Authorization"));
    Ok(r)
}
async fn bootstrap(State(app): State<Shared>) -> Json<serde_json::Value> {
    Json(json!({"schemaVersion":1,"auth":app.auth.public_config()}))
}
async fn list(State(app): State<Shared>, h: HeaderMap) -> ApiResult {
    if !administrator(&app, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut servers = app.catalog.servers.clone();
    servers.extend(app.additions.read().await.values().cloned());
    Ok(Json(json!({"servers":servers,"mutable":app.mutable,"persistence":"Helm static; API additions are RAM only"})).into_response())
}
async fn add(State(app): State<Shared>, h: HeaderMap, Json(mut s): Json<Server>) -> ApiResult {
    if !administrator(&app, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !app.mutable {
        return Err(StatusCode::FORBIDDEN);
    }
    s.validate(app.allow_http)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let mut a = app.additions.write().await;
    if app.catalog.servers.iter().any(|v| v.id == s.id) || a.contains_key(&s.id) {
        return Err(StatusCode::CONFLICT);
    }
    if a.len() + app.catalog.servers.len() >= 256 {
        return Err(StatusCode::INSUFFICIENT_STORAGE);
    }
    a.insert(s.id.clone(), s.clone());
    eprintln!("Catalog: definition added: {}", s.id);
    Ok((StatusCode::CREATED, Json(s)).into_response())
}
async fn remove(State(app): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> ApiResult {
    if !administrator(&app, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !app.mutable {
        return Err(StatusCode::FORBIDDEN);
    }
    if app.catalog.servers.iter().any(|s| s.id == id) {
        return Err(StatusCode::CONFLICT);
    }
    if app.additions.write().await.remove(&id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    eprintln!("Catalog: definition removed: {id}");
    Ok(StatusCode::NO_CONTENT.into_response())
}
#[tokio::main]
async fn main() -> Result<()> {
    temba_core::harden_process();
    let args = Args::parse();
    let mut catalog: Catalog =
        serde_yaml::from_str(&tokio::fs::read_to_string(&args.config).await?)?;
    catalog.validate(args.allow_http)?;
    let admin = Zeroizing::new(std::env::var("ADMIN_API_TOKEN").unwrap_or_default());
    ensure!(
        !args.mutable || !admin.is_empty(),
        "ADMIN_MUTABLE requires ADMIN_API_TOKEN"
    );
    let app = Arc::new(App {
        catalog,
        additions: RwLock::new(BTreeMap::new()),
        auth: auth::Verifier::from_env()?,
        admin,
        mutable: args.mutable,
        allow_http: args.allow_http,
    });
    let router = Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(|| async { StatusCode::OK }))
        .route("/v1/bootstrap", get(bootstrap))
        .route("/v1/catalog", get(catalog_handler))
        .route("/v1/admin/servers", get(list).post(add))
        .route("/v1/admin/servers/{id}", delete(remove))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", args.port)).await?;
    eprintln!("Temba Catalog listening on port {}", args.port);
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=term.recv()=>{} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod api_tests {
    use super::*;
    #[tokio::test]
    async fn public_filter_etag_and_admin_collisions() {
        let mut catalog:Catalog=serde_json::from_value(json!({"schemaVersion":1,"servers":[{"id":"public","url":"https://public.example/mcp"},{"id":"private","url":"https://private.example/mcp","catalogAccess":"authenticated"}]})).unwrap();
        catalog.validate(false).unwrap();
        let app = Arc::new(App {
            catalog,
            additions: RwLock::new(BTreeMap::new()),
            auth: auth::Verifier::from_env().unwrap(),
            admin: Zeroizing::new("admin-test-only".into()),
            mutable: true,
            allow_http: false,
        });
        let response = catalog_handler(State(app.clone()), HeaderMap::new())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let etag = response.headers()["etag"].clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let parsed: Catalog = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.servers.len(), 1);
        assert_eq!(parsed.servers[0].id, "public");
        let mut h = HeaderMap::new();
        h.insert("if-none-match", etag);
        assert_eq!(
            catalog_handler(State(app.clone()), h)
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_MODIFIED
        );
        let definition = || {
            serde_json::from_value::<Server>(
                json!({"id":"temporary","url":"https://temp.example/mcp"}),
            )
            .unwrap()
        };
        assert_eq!(
            add(State(app.clone()), HeaderMap::new(), Json(definition()))
                .await
                .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_static("Bearer admin-test-only"),
        );
        assert_eq!(
            add(State(app.clone()), h.clone(), Json(definition()))
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED
        );
        assert_eq!(
            add(State(app.clone()), h.clone(), Json(definition()))
                .await
                .unwrap_err(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            remove(State(app.clone()), h.clone(), Path("public".into()))
                .await
                .unwrap_err(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            remove(State(app.clone()), h, Path("temporary".into()))
                .await
                .unwrap()
                .status(),
            StatusCode::NO_CONTENT
        );
    }
}
