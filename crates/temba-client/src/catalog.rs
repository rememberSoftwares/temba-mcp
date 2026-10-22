use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{fmt::Write, sync::Arc, time::Duration};
use temba_core::Catalog;

use super::{config::Config, oauth};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    schema_version: u32,
    catalogue_auth: Option<oauth::CatalogAuth>,
}

pub async fn bootstrap(http: &reqwest::Client, base: &str) -> Result<Option<oauth::CatalogAuth>> {
    let response = http
        .get(format!("{base}/v1/bootstrap"))
        .timeout(Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?;
    let bootstrap: Bootstrap = temba_core::http_json(response).await?;
    ensure!(
        bootstrap.schema_version == 2,
        "Unsupported bootstrap version"
    );
    if let Some(auth) = &bootstrap.catalogue_auth {
        auth.validate()?;
    }
    Ok(bootstrap.catalogue_auth)
}

async fn fetch(
    http: &reqwest::Client,
    config: &Config,
    session: Option<&oauth::Session>,
) -> Result<Catalog> {
    let mut request = http
        .get(format!("{}/v1/catalog", config.catalog_url))
        .timeout(Duration::from_secs(20));
    if let Some(session) = session {
        request = request.bearer_auth(session.access().await?.as_str());
    }
    let response = request.send().await?.error_for_status()?;
    let mut catalog: Catalog = temba_core::http_json(response).await?;
    catalog.validate(config.allow_insecure_http)?;
    Ok(catalog)
}

pub async fn run(config: &Config, json: bool) -> Result<()> {
    let http = super::http()?;
    let session: Option<Arc<oauth::Session>> = match bootstrap(&http, &config.catalog_url).await? {
        None => None,
        Some(auth) => {
            let pending = auth.pending(&http).await?;
            eprintln!(
                "Sign in to the Temba catalog in your browser.\nIf it does not open, visit: {}",
                pending.url
            );
            let url = pending.url.clone();
            tokio::task::spawn_blocking(move || {
                if open::that(url).is_err() {
                    eprintln!("Could not open the browser automatically. Use the link above.");
                }
            });
            let session = tokio::select! {
                result = pending.finish() => result.map_err(|_| anyhow::anyhow!("Catalog sign-in failed or timed out. Run the command again to retry."))?,
                _ = tokio::signal::ctrl_c() => anyhow::bail!("Catalog sign-in cancelled"),
            };
            Some(session)
        }
    };
    let result = fetch(&http, config, session.as_deref()).await;
    if let Some(session) = session {
        session.clear().await;
    }
    let catalog = result?;
    if json {
        println!("{}", serde_json::to_string_pretty(&catalog)?);
    } else {
        print!("{}", render(&catalog));
    }
    Ok(())
}

// Catalog text is untrusted input. Keep control sequences out of terminal output.
fn terminal(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            output.extend(c.escape_unicode());
        } else {
            output.push(c);
        }
    }
    output
}

fn render(catalog: &Catalog) -> String {
    let mut output = format!("Temba catalog — {} MCP server(s)\n", catalog.servers.len());
    if catalog.servers.is_empty() {
        output.push_str("No MCP servers are visible to this user.\n");
    }
    for server in &catalog.servers {
        let _ = writeln!(
            output,
            "\n{}\n  Endpoint: {}",
            server.id,
            terminal(&server.endpoint)
        );
        if !server.description.is_empty() {
            let _ = writeln!(output, "  Description: {}", terminal(&server.description));
        }
        let access = if server.access.public {
            "public".to_owned()
        } else if server.access.authenticated {
            "authenticated users".to_owned()
        } else {
            format!("any group: {}", server.access.groups_any_of.join(", "))
        };
        let _ = writeln!(output, "  Access: {}", terminal(&access));
        let _ = writeln!(
            output,
            "  Labels: {}",
            if server.labels.is_empty() {
                "none".to_owned()
            } else {
                terminal(&server.labels.join(", "))
            }
        );
        let _ = writeln!(output, "  Timeout: {}s", server.timeout_seconds);
        if let Some(oauth) = &server.oauth {
            let _ = writeln!(output, "  OAuth: enabled");
            let _ = writeln!(
                output,
                "    Client ID: {}",
                terminal(
                    oauth
                        .client_id
                        .as_deref()
                        .unwrap_or("automatic registration")
                )
            );
            let _ = writeln!(
                output,
                "    Scopes: {}",
                if oauth.scopes.is_empty() {
                    "none requested".to_owned()
                } else {
                    terminal(&oauth.scopes.join(", "))
                }
            );
            let _ = writeln!(
                output,
                "    Callback port: {}",
                if oauth.callback_port == 0 {
                    "automatic".to_owned()
                } else {
                    oauth.callback_port.to_string()
                }
            );
            let _ = writeln!(
                output,
                "    Allowed origins: {}",
                if oauth.allowed_origins.is_empty() {
                    "automatic public discovery".to_owned()
                } else {
                    terminal(&oauth.allowed_origins.join(", "))
                }
            );
        }
        if !server.headers.is_empty() {
            output.push_str("  Headers:\n");
            for header in &server.headers {
                if let Some(reference) = &header.credential {
                    let _ = writeln!(
                        output,
                        "    {} <- tool.{}.headers.{} ({})",
                        header.name,
                        server.id,
                        reference,
                        if header.mandatory {
                            "required"
                        } else {
                            "optional"
                        }
                    );
                } else if let Some(value) = &header.value {
                    let _ = writeln!(
                        output,
                        "    {} = {} (public)",
                        header.name,
                        terminal(&serde_json::to_string(value).unwrap())
                    );
                }
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn example() -> Catalog {
        let mut catalog: Catalog = serde_json::from_value(json!({"servers":[{
            "id":"service", "endpoint":"https://mcp.example/mcp", "labels":["support"],
            "description":"Untrusted\u{1b}[2J\ntext", "oauth":{},
            "headers":[{"name":"X-Key","credential":"api_token","mandatory":true},{"name":"X-Tenant","credential":"tenant"},{"name":"X-Client","value":"temba"}]
        }]})).unwrap();
        catalog.validate(false).unwrap();
        catalog
    }

    #[test]
    fn terminal_output_explains_configuration_and_escapes_control_sequences() {
        let output = render(&example());
        for text in [
            "service",
            "https://mcp.example/mcp",
            "Labels: support",
            "X-Key <- tool.service.headers.api_token (required)",
            "X-Tenant <- tool.service.headers.tenant (optional)",
            "X-Client = \"temba\" (public)",
            "OAuth: enabled",
            "automatic public discovery",
        ] {
            assert!(output.contains(text), "missing {text}");
        }
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains("[2J\ntext"));
        assert!(!terminal("\u{202e}abc").contains('\u{202e}'));
        let empty = Catalog {
            schema_version: 2,
            revision: String::new(),
            servers: vec![],
        };
        assert!(render(&empty).contains("No MCP servers"));
    }

    #[tokio::test]
    async fn user_catalog_is_complete_and_does_not_contact_remote_mcps_or_send_local_credentials() {
        let hits = Arc::new(AtomicUsize::new(0));
        let remote_hits = hits.clone();
        let router = Router::new()
            .route(
                "/v1/bootstrap",
                get(|| async { Json(json!({"schemaVersion":2,"catalogueAuth":null})) }),
            )
            .route(
                "/v1/catalog",
                get(|headers: HeaderMap| async move {
                    assert!(!headers.contains_key("x-key"));
                    let authorization = headers.get("authorization").map(|h| h.to_str().unwrap());
                    assert!(
                        authorization.is_none() || authorization == Some("Bearer cli-test-access")
                    );
                    if authorization.is_some() {
                        Json(example()).into_response()
                    } else {
                        StatusCode::UNAUTHORIZED.into_response()
                    }
                }),
            )
            .route(
                "/mcp",
                get(move || {
                    remote_hits.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::OK }
                }),
            );
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut config: Config = toml::from_str(&format!("catalog_url='{base}'\nallow_insecure_http=true\nlabels=[]\n[tool.service]\nexpected_endpoint='https://mcp.example/mcp'\n[tool.service.headers]\napi_token='never-send-or-print-this'")).unwrap();
        config.validate().unwrap();
        let http = super::super::http().unwrap();
        assert!(bootstrap(&http, &base).await.unwrap().is_none());
        assert!(fetch(&http, &config, None).await.is_err());
        let session = oauth::Session::test_session("cli-test-access");
        let catalog = fetch(&http, &config, Some(&session)).await.unwrap();
        assert_eq!(catalog.servers.len(), 1); // Labels are deliberately not filtered by the CLI.
        assert_eq!(catalog.servers[0].labels, vec!["support"]);
        assert!(
            !serde_json::to_string(&catalog)
                .unwrap()
                .contains("never-send-or-print-this")
        );
        assert!(!render(&catalog).contains("never-send-or-print-this"));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        server.abort();
    }
}
