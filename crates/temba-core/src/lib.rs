use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use url::Url;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OAuth {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub callback_port: u16,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default)]
    pub mandatory: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Access {
    #[serde(default, skip_serializing_if = "is_false")]
    pub public: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub authenticated: bool,
    #[serde(
        default,
        deserialize_with = "deserialize_group_paths",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub groups_any_of: Vec<String>,
}
// Normalize catalog rules only; verified token claims must retain their full paths.
fn deserialize_group_paths<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .map(|group| {
            if group.is_empty() || group.trim() != group || group.contains(['\r', '\n']) {
                return Err(serde::de::Error::custom("Access groups must be nonempty paths without surrounding whitespace or line breaks"));
            }
            Ok(if group.starts_with('/') {
                group
            } else {
                format!("/{group}")
            })
        })
        .collect()
}
fn is_false(value: &bool) -> bool {
    !value
}
impl Default for Access {
    fn default() -> Self {
        Self {
            public: true,
            authenticated: false,
            groups_any_of: vec![],
        }
    }
}
impl Access {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            usize::from(self.public)
                + usize::from(self.authenticated)
                + usize::from(!self.groups_any_of.is_empty())
                == 1,
            "Access requires exactly one of public, authenticated or nonempty groupsAnyOf"
        );
        ensure!(self.groups_any_of.len() <= 64, "Too many access groups");
        for group in &self.groups_any_of {
            ensure!(
                group.starts_with('/')
                    && group.len() > 1
                    && group.len() <= 1024
                    && group.trim() == group
                    && !group.contains(['\r', '\n']),
                "Access groups must be exact full Keycloak paths"
            );
        }
        Ok(())
    }
    pub fn allows(&self, groups: Option<&[String]>) -> bool {
        self.public
            || groups.is_some_and(|groups| {
                self.authenticated || self.groups_any_of.iter().any(|g| groups.contains(g))
            })
    }
}
fn timeout() -> u64 {
    60
}
fn schema() -> u32 {
    2
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Server {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub endpoint: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub oauth: Option<OAuth>,
    #[serde(default)]
    pub headers: Vec<Header>,
    #[serde(default)]
    pub access: Access,
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
#[derive(Debug)]
pub struct DuplicateMcpName {
    pub name: String,
    pub first: usize,
    pub duplicate: usize,
}
impl std::fmt::Display for DuplicateMcpName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "servers[{}].name: duplicate MCP name '{}'; already used by servers[{}].name. Each MCP name must be unique within a catalog",
            self.duplicate, self.name, self.first
        )
    }
}
impl std::error::Error for DuplicateMcpName {}
impl Catalog {
    pub fn validate(&mut self, allow_http: bool) -> Result<()> {
        ensure!(self.schema_version == 2, "Unsupported catalog schema");
        ensure!(self.servers.len() <= 256, "Catalog exceeds 256 servers");
        let mut names = std::collections::BTreeMap::new();
        for (index, server) in self.servers.iter_mut().enumerate() {
            server
                .validate(allow_http)
                .with_context(|| format!("servers[{index}]"))?;
            if let Some(first) = names.insert(server.name.clone(), index) {
                return Err(DuplicateMcpName {
                    name: server.name.clone(),
                    first,
                    duplicate: index,
                }
                .into());
            }
        }
        self.revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&self.servers)?));
        Ok(())
    }
}
impl Server {
    pub fn validate(&mut self, allow_http: bool) -> Result<()> {
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 64
                && !self.name.starts_with("__")
                && self
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "Invalid MCP name: use 1-64 letters, digits, hyphens or underscores, without a leading __"
        );
        endpoint(&self.endpoint, allow_http)?;
        self.access.validate()?;
        ensure!(
            (1..=300).contains(&self.timeout_seconds),
            "Timeout must be between 1 and 300 seconds"
        );
        ensure!(self.description.len() <= 8192, "Metadata too large");
        for label in &mut self.labels {
            *label = label.trim().to_lowercase();
        }
        if let Some(oauth) = &self.oauth {
            for origin in &oauth.allowed_origins {
                endpoint(origin, allow_http)?;
            }
        }
        ensure!(self.headers.len() <= 32, "Too many headers");
        let mut names = HashSet::new();
        for header in &self.headers {
            let name = header.name.to_ascii_lowercase();
            ensure!(
                (name == "authorization" || name.starts_with("x-"))
                    && header.name.len() <= 128
                    && header
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "Headers must be Authorization or valid X-* names"
            );
            ensure!(names.insert(name.clone()), "Duplicate header name");
            ensure!(
                self.oauth.is_none() || name != "authorization",
                "OAuth owns Authorization; use X-* headers alongside OAuth"
            );
            match (&header.credential, &header.value) {
                (Some(credential), None) => {
                    ensure!(
                        !credential.is_empty()
                            && credential.len() <= 128
                            && credential
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                        "Invalid local credential reference"
                    );
                }
                (None, Some(value)) => {
                    ensure!(
                        name != "authorization",
                        "Public Authorization values are not allowed; use a local credential"
                    );
                    ensure!(
                        value.len() <= 16384 && !value.chars().any(|c| c.is_control() && c != '\t'),
                        "Invalid public header value"
                    );
                }
                _ => bail!("A header requires exactly one of credential or public value"),
            }
        }
        Ok(())
    }
}
pub fn endpoint(value: &str, allow_insecure_http: bool) -> Result<Url> {
    let u = Url::parse(value)?;
    ensure!(
        u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none(),
        "URL must have a host and no credentials or fragment"
    );
    ensure!(
        u.scheme() == "https" || (allow_insecure_http && u.scheme() == "http"),
        "HTTPS required unless insecure HTTP is explicitly enabled"
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
    fn access_rules_fail_closed_and_match_exact_group_paths() {
        for invalid in [
            r#"{}"#,
            r#"{"groupsAnyOf":[]}"#,
            r#"{"public":true,"groupsAnyOf":["/X"]}"#,
            r#"{"public":true,"authenticated":true}"#,
        ] {
            let rule: Access = serde_json::from_str(invalid).unwrap();
            assert!(rule.validate().is_err(), "accepted {invalid}");
        }
        let rule: Access = serde_json::from_str(r#"{"groupsAnyOf":["/X","/Y"]}"#).unwrap();
        rule.validate().unwrap();
        assert!(!rule.allows(None));
        assert!(!rule.allows(Some(&[])));
        assert!(rule.allows(Some(&["/X".into()])));
        assert!(rule.allows(Some(&["/Y".into()])));
        assert!(!rule.allows(Some(&["/X/subgroup".into(), "/x".into()])));
    }
    #[test]
    fn group_shorthand_is_canonical_and_preserves_hierarchy_and_case() {
        let short: Access =
            serde_json::from_str(r#"{"groupsAnyOf":["dev","equipes/dev"]}"#).unwrap();
        let full: Access =
            serde_json::from_str(r#"{"groupsAnyOf":["/dev","/equipes/dev"]}"#).unwrap();
        short.validate().unwrap();
        assert_eq!(short, full);
        assert_eq!(
            serde_json::to_value(&short).unwrap(),
            serde_json::json!({"groupsAnyOf":["/dev","/equipes/dev"]})
        );
        assert!(short.allows(Some(&["/dev".into()])));
        assert!(short.allows(Some(&["/equipes/dev".into()])));
        assert!(!short.allows(Some(&[
            "dev".into(),
            "/Dev".into(),
            "/other/dev".into(),
            "/dev/child".into()
        ])));
        let nested: Access = serde_json::from_str(r#"{"groupsAnyOf":["equipes/dev"]}"#).unwrap();
        assert!(!nested.allows(Some(&["/dev".into()])));
        for invalid in [
            "",
            " ",
            " dev",
            "dev ",
            " /dev",
            "dev\n",
            "de\rv",
            "/",
            &"x".repeat(1024),
        ] {
            let parsed =
                serde_json::from_value::<Access>(serde_json::json!({"groupsAnyOf":[invalid]}));
            assert!(
                parsed.is_err() || parsed.unwrap().validate().is_err(),
                "accepted {invalid:?}"
            );
        }
    }
    #[test]
    fn equivalent_group_syntax_produces_the_same_catalog_revision() {
        let mut short: Catalog = serde_json::from_value(serde_json::json!({"servers":[{
            "name":"service", "endpoint":"https://example.com/mcp", "access":{"groupsAnyOf":["dev"]}
        }]}))
        .unwrap();
        let mut full: Catalog = serde_json::from_value(serde_json::json!({"servers":[{
            "name":"service", "endpoint":"https://example.com/mcp", "access":{"groupsAnyOf":["/dev"]}
        }]}))
        .unwrap();
        short.validate(false).unwrap();
        full.validate(false).unwrap();
        assert_eq!(short.revision, full.revision);
    }
    #[test]
    fn catalog_rejects_secrets_and_duplicates() {
        assert!(serde_json::from_str::<Server>(r#"{"name":"jira","endpoint":"https://jira.test/mcp","headers":{"Authorization":"secret"}}"#).is_err());
        let s: Server =
            serde_json::from_str(r#"{"name":"jira","endpoint":"https://jira.test/mcp"}"#).unwrap();
        let mut c = Catalog {
            schema_version: 2,
            revision: String::new(),
            servers: vec![s.clone(), s],
        };
        let error = c.validate(false).unwrap_err();
        let duplicate = error.downcast_ref::<DuplicateMcpName>().unwrap();
        assert_eq!(duplicate.name, "jira");
        assert_eq!((duplicate.first, duplicate.duplicate), (0, 1));
        assert!(error.to_string().contains("servers[1].name"));
    }
    #[test]
    fn server_uses_only_name_and_rejects_the_removed_id() {
        let server: Server = serde_json::from_value(serde_json::json!({
            "name":"service", "endpoint":"https://example.com/mcp",
            "headers":[{"name":"X-Tenant","value":"team"}]
        }))
        .unwrap();
        let value = serde_json::to_value(server).unwrap();
        assert_eq!(value["name"], "service");
        assert!(value.get("id").is_none());
        assert_eq!(value["headers"][0]["name"], "X-Tenant");
        assert!(
            serde_json::from_value::<Server>(serde_json::json!({
                "id":"service", "name":"service", "endpoint":"https://example.com/mcp"
            }))
            .is_err()
        );
    }
    #[test]
    fn oauth_and_headers_are_independent_but_authorization_cannot_conflict() {
        let definition = |headers| {
            serde_json::from_value::<Server>(serde_json::json!({
                "name":"jira","endpoint":"https://example.com/mcp","oauth":{},
                "headers":headers
            }))
            .unwrap()
        };
        let mut combined = definition(serde_json::json!([
            {"name":"X-Api-Key","credential":"api_key"},
            {"name":"X-Tenant","credential":"tenant"}
        ]));
        combined.validate(false).unwrap();
        for headers in [
            serde_json::json!([{"name":"Authorization","credential":"key"}]),
            serde_json::json!([{"name":"X-Tenant","credential":"key"},{"name":"x-tenant","credential":"other"}]),
            serde_json::json!([{"name":"Host","credential":"key"}]),
        ] {
            assert!(definition(headers).validate(false).is_err());
        }
        assert!(serde_json::from_value::<Server>(serde_json::json!({"name":"jira","endpoint":"https://example.com/mcp","headers":[{"name":"X-Key","credential":"key","value":"public"}]})).unwrap().validate(false).is_err());
    }
    #[test]
    fn public_headers_have_exactly_one_source_and_no_secret_authorization_value() {
        let definition = |headers| {
            serde_json::from_value::<Server>(serde_json::json!({"name":"public","endpoint":"https://example.com/mcp","headers":headers})).unwrap()
        };
        let mut public = definition(serde_json::json!([{"name":"X-App","value":"temba"}]));
        public.validate(false).unwrap();
        assert!(!public.headers[0].mandatory);
        for headers in [
            serde_json::json!([{"name":"X-Key"}]),
            serde_json::json!([{"name":"Authorization","value":"Bearer secret"}]),
            serde_json::json!([{"name":"X-App","value":"bad\r\nvalue"}]),
        ] {
            assert!(definition(headers).validate(false).is_err());
        }
        assert!(serde_json::from_value::<Server>(serde_json::json!({"name":"old","endpoint":"https://example.com/mcp","headers":[{"name":"X-Key","credential":"key","prefix":"old"}]})).is_err());
    }
    #[test]
    fn credentials_and_insecure_destinations_rejected() {
        for u in [
            "https://user:pass@host/mcp",
            "https://host/mcp?access_token=abc",
        ] {
            assert!(endpoint(u, true).is_err());
        }
        for url in [
            "http://127.0.0.1:8080/mcp",
            "http://192.168.1.20:30080/mcp",
            "http://mcp.internal/mcp",
        ] {
            assert!(endpoint(url, true).is_ok());
            assert!(endpoint(url, false).is_err());
        }
        assert!(endpoint("ftp://host/mcp", true).is_err());
        let mut server: Server = serde_json::from_value(serde_json::json!({
            "name":"service", "endpoint":"https://mcp.example/mcp",
            "oauth":{"allowedOrigins":["http://identity.internal"]}
        }))
        .unwrap();
        assert!(server.validate(false).is_err());
        server.validate(true).unwrap();
        assert!(endpoint("https://host/mcp?tools=all", false).is_ok());
        assert!(https_override("https://other.test/token", "https://issuer.test/realm").is_err());
    }
}
