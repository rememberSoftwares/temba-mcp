use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use url::Url;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    #[default]
    None,
    Token {
        #[serde(default = "authorization_header", rename = "headerName")]
        header_name: String,
        #[serde(default = "bearer_prefix")]
        prefix: String,
    },
    Oauth {
        #[serde(default, rename = "clientId")]
        client_id: Option<String>,
        #[serde(default)]
        scopes: Vec<String>,
        #[serde(default, rename = "callbackPort")]
        callback_port: u16,
        #[serde(default, rename = "allowedOrigins")]
        allowed_origins: Vec<String>,
    },
}
fn authorization_header() -> String {
    "Authorization".into()
}
fn bearer_prefix() -> String {
    "Bearer ".into()
}
fn public() -> String {
    "public".into()
}
fn timeout() -> u64 {
    60
}
fn schema() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Server {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub url: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default = "public")]
    pub catalog_access: String,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Catalog {
    #[serde(default = "schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub servers: Vec<Server>,
}
impl Catalog {
    pub fn validate(&mut self, allow_http: bool) -> Result<()> {
        ensure!(self.schema_version == 1, "Unsupported catalog schema");
        ensure!(self.servers.len() <= 256, "Catalog exceeds 256 servers");
        let mut ids = HashSet::new();
        for server in &mut self.servers {
            server.validate(allow_http)?;
            ensure!(ids.insert(server.id.clone()), "Duplicate server ID");
        }
        self.revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&self.servers)?));
        Ok(())
    }
}
impl Server {
    pub fn validate(&mut self, allow_http: bool) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 64
                && !self.id.starts_with("__")
                && self
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "Invalid server ID"
        );
        endpoint(&self.url, allow_http)?;
        ensure!(
            matches!(self.catalog_access.as_str(), "public" | "authenticated"),
            "Invalid catalog access"
        );
        ensure!(
            (1..=300).contains(&self.timeout_seconds),
            "Timeout must be between 1 and 300 seconds"
        );
        ensure!(
            self.description.len() <= 8192 && self.name.len() <= 256,
            "Metadata too large"
        );
        for label in &mut self.labels {
            *label = label.trim().to_lowercase();
        }
        if let Auth::Oauth {
            allowed_origins, ..
        } = &self.auth
        {
            for origin in allowed_origins {
                endpoint(origin, false)?;
            }
        }
        if let Auth::Token {
            header_name,
            prefix,
        } = &self.auth
        {
            ensure!(
                header_name == "Authorization" || header_name.starts_with("X-"),
                "Unsupported credential header"
            );
            ensure!(
                !prefix.contains(['\r', '\n']) && prefix.len() <= 64,
                "Invalid credential prefix"
            );
        }
        Ok(())
    }
    pub fn visible(&self, labels: &[String]) -> bool {
        self.labels.is_empty() || self.labels.iter().any(|v| labels.contains(v))
    }
}
pub fn endpoint(value: &str, allow_loopback_http: bool) -> Result<Url> {
    let u = Url::parse(value)?;
    ensure!(
        u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none(),
        "URL must have a host and no credentials or fragment"
    );
    let local = matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    ensure!(
        u.scheme() == "https" || (allow_loopback_http && local && u.scheme() == "http"),
        "HTTPS required; HTTP development is loopback only"
    );
    for (key, _) in u.query_pairs() {
        if [
            "token",
            "access_token",
            "key",
            "api_key",
            "secret",
            "password",
            "authorization",
        ]
        .contains(&key.to_lowercase().as_str())
        {
            bail!("Credentials in URLs are forbidden");
        }
    }
    Ok(u)
}
pub fn https_override(value: &str, issuer: &str) -> Result<()> {
    let u = endpoint(value, false)?;
    let i = endpoint(issuer, false)?;
    ensure!(
        u.origin() == i.origin() && u.query().is_none(),
        "Override must be HTTPS on the issuer origin without query"
    );
    Ok(())
}
pub fn harden_process() {
    // Some provider edges reset TLS ClientHellos containing hybrid ML-KEM groups.
    // Retain TLS 1.3 and certificate validation with interoperable ECDHE groups.
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    provider.kx_groups = vec![
        rustls::crypto::aws_lc_rs::kx_group::X25519,
        rustls::crypto::aws_lc_rs::kx_group::SECP256R1,
        rustls::crypto::aws_lc_rs::kx_group::SECP384R1,
    ];
    let _ = provider.install_default();

    #[cfg(unix)]
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &limit);
    }
}

/// Limit untrusted HTTP JSON and zeroize our owned response buffer on drop.
pub async fn http_bytes(mut response: reqwest::Response) -> Result<zeroize::Zeroizing<Vec<u8>>> {
    const MAX: usize = 1024 * 1024;
    ensure!(
        response.content_length().is_none_or(|n| n <= MAX as u64),
        "HTTP response too large"
    );
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await? {
        ensure!(bytes.len() + chunk.len() <= MAX, "HTTP response too large");
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
pub async fn http_json<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    Ok(serde_json::from_slice(&http_bytes(response).await?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_rejects_secrets_and_duplicates() {
        assert!(serde_json::from_str::<Server>(r#"{"id":"jira","url":"https://jira.test/mcp","headers":{"Authorization":"secret"}}"#).is_err());
        let s: Server =
            serde_json::from_str(r#"{"id":"jira","url":"https://jira.test/mcp"}"#).unwrap();
        let mut c = Catalog {
            schema_version: 1,
            revision: String::new(),
            servers: vec![s.clone(), s],
        };
        assert!(c.validate(false).is_err());
    }
    #[test]
    fn credentials_and_insecure_destinations_rejected() {
        for u in [
            "https://user:pass@host/mcp",
            "http://host/mcp",
            "https://host/mcp?access_token=abc",
        ] {
            assert!(endpoint(u, true).is_err());
        }
        assert!(endpoint("http://127.0.0.1:8080/mcp", true).is_ok());
        assert!(endpoint("https://host/mcp?tools=all", false).is_ok());
        assert!(https_override("https://other.test/token", "https://issuer.test/realm").is_err());
    }
}
