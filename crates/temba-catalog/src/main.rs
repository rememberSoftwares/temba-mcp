mod auth;
mod store;
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
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
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
    #[arg(long, env = "ADMIN_READ_ONLY", default_value_t = true, action = clap::ArgAction::Set)]
    read_only: bool,
    #[arg(long, env = "CATALOG_ADDITIONS_PATH")]
    additions_path: Option<PathBuf>,
}
struct App {
    catalog: Catalog,
    additions: RwLock<BTreeMap<String, Server>>,
    auth: auth::Verifier,
    admin: Zeroizing<String>,
    read_only: bool,
    additions_path: Option<PathBuf>,
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
    let identity = match h.get("authorization") {
        Some(_) => Some(
            app.auth
                .verify(bearer(&h).ok_or(StatusCode::UNAUTHORIZED)?)
                .await
                .map_err(|error| {
                    eprintln!(
                        "Catalog: access token rejected: {}",
                        auth::rejection_reason(&error)
                    );
                    StatusCode::UNAUTHORIZED
                })?,
        ),
        None => None,
    };
    let mut servers = app.catalog.servers.clone();
    servers.extend(app.additions.read().await.values().cloned());
    servers.retain(|s| {
        s.access
            .allows(identity.as_ref().map(|i| i.groups.as_slice()))
    });
    let mut catalog = Catalog {
        schema_version: 2,
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
    Json(json!({"schemaVersion":2,"catalogueAuth":app.auth.public_config()}))
}
async fn list(State(app): State<Shared>, h: HeaderMap) -> ApiResult {
    if !administrator(&app, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut servers = app.catalog.servers.clone();
    servers.extend(app.additions.read().await.values().cloned());
    Ok(Json(json!({"servers":servers,"readOnly":app.read_only,"persistence":if app.additions_path.is_some() {"volume"} else {"disabled"}})).into_response())
}
async fn add(State(app): State<Shared>, h: HeaderMap, Json(mut s): Json<Server>) -> ApiResult {
    if !administrator(&app, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if app.read_only {
        return Err(StatusCode::FORBIDDEN);
    }
    s.validate(app.allow_http)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let mut a = app.additions.write().await;
    if app.catalog.servers.iter().any(|v| v.name == s.name) || a.contains_key(&s.name) {
        return Ok((StatusCode::CONFLICT, Json(json!({
            "error":"duplicate_name",
            "message":format!("MCP name '{}' already exists in this catalog. Choose a unique name; neither the existing definition nor the new one was modified", s.name)
        }))).into_response());
    }
    if a.len() + app.catalog.servers.len() >= 256 {
        return Err(StatusCode::INSUFFICIENT_STORAGE);
    }
    let mut next = a.clone();
    next.insert(s.name.clone(), s.clone());
    store::persist(
        app.additions_path
            .as_deref()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?,
        &next,
    )
    .await
    .map_err(|_| {
        eprintln!("Catalog: persistence failed; definition not added");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    *a = next;
    eprintln!("Catalog: definition added: {}", s.name);
    Ok((StatusCode::CREATED, Json(s)).into_response())
}
async fn remove(State(app): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> ApiResult {
    if !administrator(&app, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if app.read_only {
        return Err(StatusCode::FORBIDDEN);
    }
    if app.catalog.servers.iter().any(|s| s.name == id) {
        return Err(StatusCode::CONFLICT);
    }
    let mut additions = app.additions.write().await;
    let mut next = additions.clone();
    if next.remove(&id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    store::persist(
        app.additions_path
            .as_deref()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?,
        &next,
    )
    .await
    .map_err(|_| {
        eprintln!("Catalog: persistence failed; definition not removed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    *additions = next;
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
        args.read_only || (!admin.is_empty() && args.additions_path.is_some()),
        "Writable admin API requires ADMIN_API_TOKEN and CATALOG_ADDITIONS_PATH"
    );
    let additions = match &args.additions_path {
        Some(path) => store::load(path, &catalog, args.allow_http)?,
        None => BTreeMap::new(),
    };
    eprintln!(
        "Catalog: {} persisted API definitions loaded; readOnly={}",
        additions.len(),
        args.read_only
    );
    let verifier = auth::Verifier::from_env()?;
    let auth_enabled = !verifier.public_config().is_null();
    eprintln!(
        "Catalog: {} definitions loaded; authentication={auth_enabled}",
        catalog.servers.len()
    );
    for server in catalog.servers.iter().chain(additions.values()) {
        if !auth_enabled && !server.access.public {
            eprintln!(
                "Catalog: WARNING MCP {} is hidden from all users: protected access requires catalog authentication",
                server.name
            );
        }
    }
    let app = Arc::new(App {
        catalog,
        additions: RwLock::new(additions),
        auth: verifier,
        admin,
        read_only: args.read_only,
        additions_path: args.additions_path,
        allow_http: args.allow_http,
    });
    let router = Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(|| async { StatusCode::OK }))
        .route("/v1/bootstrap", get(bootstrap))
        .route("/v1/catalog", get(catalog_handler))
        .route("/v1/admin/servers", get(list).post(add))
        .route("/v1/admin/servers/{name}", delete(remove))
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
    async fn group_filtered_catalog_is_isolated_between_users_and_anonymous_requests() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let mut catalog: Catalog = serde_json::from_value(json!({"schemaVersion":2,"servers":[
            {"name":"everyone","endpoint":"https://example.com/public","access":{"public":true},"headers":[{"name":"X-Api-Key","credential":"api_key","mandatory":true},{"name":"X-App","value":"temba"}]},
            {"name":"signed-in","endpoint":"https://example.com/auth","access":{"authenticated":true}},
            {"name":"only-x","endpoint":"https://example.com/x","access":{"groupsAnyOf":["X"]}},
            {"name":"only-y","endpoint":"https://example.com/y","access":{"groupsAnyOf":["/Y"]}},
            {"name":"x-or-y","endpoint":"https://example.com/xy","access":{"groupsAnyOf":["X","/Y"]}}
        ]}))
        .unwrap();
        catalog.validate(false).unwrap();
        let app = Arc::new(App {
            catalog,
            additions: RwLock::new(BTreeMap::new()),
            auth: auth::tests::verifier(),
            admin: Zeroizing::new(String::new()),
            read_only: true,
            additions_path: None,
            allow_http: false,
        });
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut previous_etag = None;
        for (groups, expected) in [
            (None, vec!["everyone"]),
            (Some(vec![]), vec!["everyone", "signed-in"]),
            (
                Some(vec!["/X"]),
                vec!["everyone", "signed-in", "only-x", "x-or-y"],
            ),
            (
                Some(vec!["/Y"]),
                vec!["everyone", "signed-in", "only-y", "x-or-y"],
            ),
            (
                Some(vec!["/X", "/Y"]),
                vec!["everyone", "signed-in", "only-x", "only-y", "x-or-y"],
            ),
            (
                Some(vec!["/X/subgroup", "/other/X", "X", "/x"]),
                vec!["everyone", "signed-in"],
            ),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("x-groups", HeaderValue::from_static("/X,/Y"));
            if let Some(groups) = groups {
                let token = auth::tests::signed(
                    json!({"iss":"https://auth.example/realm","aud":"temba-mcp","sub":"user","typ":"Bearer","exp":now+300,"groups":groups}),
                );
                headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
            }
            if let Some(etag) = previous_etag.take() {
                headers.insert("if-none-match", etag);
            }
            let response = catalog_handler(State(app.clone()), headers).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["vary"], "Authorization");
            previous_etag = Some(response.headers()["etag"].clone());
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            for server in raw["servers"].as_array().unwrap() {
                if let Some(groups) = server["access"]["groupsAnyOf"].as_array() {
                    assert!(
                        groups
                            .iter()
                            .all(|group| group.as_str().unwrap().starts_with('/'))
                    );
                }
            }
            let parsed: Catalog = serde_json::from_slice(&bytes).unwrap();
            let declarations = &parsed
                .servers
                .iter()
                .find(|s| s.name == "everyone")
                .unwrap()
                .headers;
            assert!(declarations[0].mandatory);
            assert_eq!(declarations[0].credential.as_deref(), Some("api_key"));
            assert_eq!(declarations[1].value.as_deref(), Some("temba"));
            assert!(!declarations[1].mandatory);
            assert_eq!(
                parsed
                    .servers
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer forged-token"),
        );
        assert_eq!(
            catalog_handler(State(app), headers).await.unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }
    #[tokio::test]
    async fn public_filter_etag_and_admin_collisions() {
        let mut catalog:Catalog=serde_json::from_value(json!({"schemaVersion":2,"servers":[{"name":"public","endpoint":"https://public.example/mcp"},{"name":"private","endpoint":"https://private.example/mcp","access":{"authenticated":true}}]})).unwrap();
        catalog.validate(false).unwrap();
        let app = Arc::new(App {
            catalog,
            additions: RwLock::new(BTreeMap::new()),
            auth: auth::Verifier::from_env().unwrap(),
            admin: Zeroizing::new("admin-test-only".into()),
            read_only: false,
            additions_path: Some(store::test_path()),
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
        assert_eq!(parsed.servers[0].name, "public");
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
                json!({"name":"temporary","endpoint":"https://temp.example/mcp"}),
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
        // An API addition cannot reuse a name owned by the Helm catalog.
        let mut collision = app.catalog.servers[0].clone();
        collision.endpoint = "https://other.example/mcp".into();
        assert_eq!(
            add(State(app.clone()), h.clone(), Json(collision))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert!(app.additions.read().await.is_empty());
        assert_eq!(
            add(State(app.clone()), h.clone(), Json(definition()))
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED
        );
        let before = app.additions.read().await.clone();
        let response = add(State(app.clone()), h.clone(), Json(definition()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "duplicate_name");
        assert!(body["message"].as_str().unwrap().contains("temporary"));
        assert_eq!(*app.additions.read().await, before);
        let restored =
            store::load(app.additions_path.as_ref().unwrap(), &app.catalog, false).unwrap();
        assert!(restored.contains_key("temporary"));

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
        assert!(
            store::load(app.additions_path.as_ref().unwrap(), &app.catalog, false)
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(app.additions_path.as_ref().unwrap().parent().unwrap()).unwrap();
    }
    #[tokio::test]
    async fn read_only_and_failed_writes_cannot_mutate_the_catalog() {
        let catalog = Catalog {
            schema_version: 2,
            revision: String::new(),
            servers: vec![],
        };
        let path = store::test_path();
        let definition: Server = serde_json::from_value(
            json!({"name":"persisted","endpoint":"https://example.com/mcp"}),
        )
        .unwrap();
        let headers = || {
            let mut h = HeaderMap::new();
            h.insert(
                "authorization",
                HeaderValue::from_static("Bearer admin-test"),
            );
            h
        };
        let readonly = Arc::new(App {
            catalog: catalog.clone(),
            additions: RwLock::new(BTreeMap::new()),
            auth: auth::Verifier::from_env().unwrap(),
            admin: Zeroizing::new("admin-test".into()),
            read_only: true,
            additions_path: Some(path.clone()),
            allow_http: false,
        });
        assert_eq!(
            add(State(readonly), headers(), Json(definition.clone()))
                .await
                .unwrap_err(),
            StatusCode::FORBIDDEN
        );
        let app = Arc::new(App {
            catalog,
            additions: RwLock::new(BTreeMap::new()),
            auth: auth::Verifier::from_env().unwrap(),
            admin: Zeroizing::new("admin-test".into()),
            read_only: false,
            additions_path: Some(path.join("missing/additions.json")),
            allow_http: false,
        });
        assert_eq!(
            add(State(app.clone()), headers(), Json(definition.clone()))
                .await
                .unwrap_err(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(app.additions.read().await.is_empty());
        app.additions
            .write()
            .await
            .insert(definition.name.clone(), definition);
        assert_eq!(
            remove(State(app.clone()), headers(), Path("persisted".into()))
                .await
                .unwrap_err(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(app.additions.read().await.contains_key("persisted"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
