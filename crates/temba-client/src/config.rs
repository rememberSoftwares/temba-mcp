use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use zeroize::Zeroizing;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub catalogs: Vec<CatalogSource>,
    #[serde(default = "refresh")]
    pub refresh_minutes: u64,
    #[serde(default = "limit")]
    pub search_limit: usize,
    #[serde(default)]
    pub allow_insecure_http: bool,
    #[serde(default)]
    pub dynamic_catalog: DynamicCatalog,
    #[serde(default)]
    pub local_catalog: Option<StaticCatalog>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSource {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default, deserialize_with = "unique_map")]
    pub tools: BTreeMap<String, ToolConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticCatalog {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, deserialize_with = "unique_map")]
    pub tools: BTreeMap<String, ToolConfig>,
    // Assigned by the local runtime before catalog I/O. Not a YAML option.
    #[serde(skip, default = "runtime_address")]
    pub address: std::net::SocketAddr,
    #[serde(default)]
    pub servers: Vec<crate::local_server::LocalServer>,
}
fn runtime_address() -> std::net::SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}
pub(crate) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with("__")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
impl Config {
    pub fn validate(&mut self) -> Result<()> {
        ensure!(
            self.catalogs.len() <= 32,
            "At most 32 catalogs are supported"
        );
        ensure!(
            (1..=1440).contains(&self.refresh_minutes),
            "refresh_minutes must be between 1 and 1440"
        );
        ensure!(
            (1..=50).contains(&self.search_limit),
            "search_limit must be between 1 and 50 per catalog"
        );
        ensure!(
            (1..=64).contains(&self.dynamic_catalog.max_servers),
            "dynamic_catalog.max_servers must be between 1 and 64"
        );
        let mut names = BTreeMap::new();
        for (index, source) in self.catalogs.iter_mut().enumerate() {
            ensure!(
                valid_name(&source.name),
                "catalogs[{index}].name: use 1–64 letters, digits, hyphens or underscores, without a leading __"
            );
            ensure!(
                source.name != "local",
                "catalogs[{index}].name: local is reserved for local_catalog; choose another name"
            );
            if let Some(first) = names.insert(source.name.clone(), index) {
                anyhow::bail!(
                    "catalogs[{index}].name: duplicate catalog name '{}'; already used by catalogs[{first}].name. Each catalog name must be unique",
                    source.name
                );
            }
            let url = temba_core::endpoint(&source.url, self.allow_insecure_http).with_context(|| format!("catalogs[{index}].url: invalid catalog URL; use HTTPS, or explicitly enable allow_insecure_http for HTTP"))?;
            ensure!(url.query().is_none(), "Catalog URL cannot have a query");
            source.url = source.url.trim_end_matches('/').to_owned();
            source.labels = source
                .labels
                .iter()
                .map(|l| l.trim().to_lowercase())
                .collect();
        }
        for (index, source) in self.catalogs.iter().enumerate() {
            for (server, tool) in &source.tools {
                ensure!(valid_name(server), "Invalid MCP name in catalog tools");
                validate_tool(tool, self.allow_insecure_http).with_context(|| {
                    format!("catalogs[{index}].tools.{}", server.escape_debug())
                })?;
            }
        }
        if let Some(local) = &mut self.local_catalog {
            for (server, tool) in &local.tools {
                ensure!(
                    valid_name(server),
                    "Invalid MCP name in local_catalog.tools"
                );
                validate_tool(tool, self.allow_insecure_http)
                    .with_context(|| format!("local_catalog.tools.{}", server.escape_debug()))?;
            }
            for (index, server) in local.servers.iter_mut().enumerate() {
                server
                    .validate(local.address, self.allow_insecure_http)
                    .with_context(|| format!("local_catalog.servers[{index}]"))?;
            }
            let mut catalog = temba_core::Catalog {
                schema_version: 2,
                revision: String::new(),
                servers: local
                    .servers
                    .iter()
                    .map(|s| s.public_definition(local.address))
                    .collect(),
            };
            catalog.validate(true).context("local_catalog.servers: invalid MCP definitions (names must be unique across HTTP and stdio entries)")?;
            for server in &local.servers {
                if let crate::local_server::LocalServer::Stdio(server) = server {
                    ensure!(
                        !local.tools.contains_key(&server.name),
                        "Configure stdio credentials in env/envFromEnv, not local_catalog.tools"
                    );
                }
            }
        }
        Ok(())
    }
    pub fn into_sources(mut self) -> Vec<(String, SourceConfig)> {
        let mut local_stdio_endpoints = std::collections::HashSet::new();
        if let Some(local) = self
            .local_catalog
            .take()
            .filter(|c| c.enabled && !c.servers.is_empty())
        {
            let address = local.address;
            local_stdio_endpoints = local
                .servers
                .iter()
                .filter(|s| matches!(s, crate::local_server::LocalServer::Stdio(_)))
                .map(|s| s.public_definition(local.address).endpoint)
                .collect();
            self.catalogs.push(CatalogSource {
                name: "local".into(),
                url: format!("http://{address}"),
                labels: Vec::new(),
                tools: local.tools,
            });
        }
        self.catalogs
            .into_iter()
            .map(|source| {
                let config = SourceConfig {
                    namespace: source.name.clone(),
                    catalog_url: source.url,
                    labels: (source.name != "local").then_some(source.labels),
                    refresh_minutes: self.refresh_minutes,
                    search_limit: self.search_limit,
                    allow_insecure_http: self.allow_insecure_http,
                    local_stdio_endpoints: if source.name == "local" {
                        local_stdio_endpoints.clone()
                    } else {
                        Default::default()
                    },
                    tool: source.tools,
                    dynamic_catalog: DynamicCatalog::default(),
                };
                (source.name, config)
            })
            .collect()
    }
    pub fn dynamic_source(&self) -> SourceConfig {
        SourceConfig {
            namespace: "dynamic".into(),
            catalog_url: "https://unused.invalid".into(),
            labels: None,
            refresh_minutes: self.refresh_minutes,
            search_limit: self.search_limit,
            allow_insecure_http: self.allow_insecure_http,
            local_stdio_endpoints: Default::default(),
            tool: BTreeMap::new(),
            dynamic_catalog: DynamicCatalog {
                enabled: self.dynamic_catalog.enabled,
                max_servers: self.dynamic_catalog.max_servers,
            },
        }
    }
}
fn validate_tool(tool: &ToolConfig, allow_http: bool) -> Result<()> {
    temba_core::endpoint(&tool.expected_endpoint, allow_http)?;
    ensure!(tool.headers.len() <= 32, "Too many local header mappings");
    for header in tool.headers.values() {
        match header {
            LocalHeader::Env { env } => ensure!(
                !env.is_empty() && env.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "Invalid header environment variable name"
            ),
            LocalHeader::Literal(value) => {
                ensure!(value.len() <= 16384, "Local header value too long")
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    #[serde(default)]
    pub namespace: String,
    #[serde(skip)]
    pub local_stdio_endpoints: std::collections::HashSet<String>,
    pub catalog_url: String,
    #[serde(default = "remote_labels")]
    pub labels: Option<Vec<String>>,
    #[serde(default = "refresh")]
    pub refresh_minutes: u64,
    #[serde(default = "limit")]
    pub search_limit: usize,
    #[serde(default)]
    pub allow_insecure_http: bool,
    #[serde(default)]
    pub tool: BTreeMap<String, ToolConfig>,
    #[serde(default)]
    pub dynamic_catalog: DynamicCatalog,
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DynamicCatalog {
    pub enabled: bool,
    pub max_servers: usize,
}
impl Default for DynamicCatalog {
    fn default() -> Self {
        Self {
            enabled: false,
            max_servers: 16,
        }
    }
}
pub(crate) fn unique_map<'de, D, V>(d: D) -> std::result::Result<BTreeMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct Visitor<V>(std::marker::PhantomData<V>);
    impl<'de, V: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<V> {
        type Value = BTreeMap<String, V>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a mapping with unique keys")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            use serde::de::Error;
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, V>()? {
                if values.insert(key, value).is_some() {
                    return Err(A::Error::custom("Duplicate mapping key"));
                }
            }
            Ok(values)
        }
    }
    d.deserialize_map(Visitor(std::marker::PhantomData))
}
pub struct ToolConfig {
    pub expected_endpoint: String,
    pub headers: BTreeMap<String, LocalHeader>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolConfig {
    expected_endpoint: String,
    #[serde(default, deserialize_with = "unique_map")]
    headers: BTreeMap<String, LiteralHeader>,
    #[serde(default, rename = "headersFromEnv")]
    headers_from_env: Vec<HeaderFromEnv>,
}
#[derive(Deserialize)]
struct LiteralHeader(#[serde(deserialize_with = "secret")] Zeroizing<String>);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderFromEnv {
    name: String,
    #[serde(rename = "valueFrom")]
    value_from: HeaderValueFrom,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderValueFrom {
    #[serde(rename = "envVarRef")]
    env_var_ref: EnvVarRef,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvVarRef {
    name: String,
}
impl<'de> Deserialize<'de> for ToolConfig {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error;
        let raw = RawToolConfig::deserialize(d)?;
        let mut headers = BTreeMap::new();
        let mut insert = |name: String, value: LocalHeader| {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| D::Error::custom("Invalid HTTP header name"))?;
            if headers.insert(name.as_str().to_owned(), value).is_some() {
                return Err(D::Error::custom(
                    "Duplicate HTTP header (names are case-insensitive)",
                ));
            }
            Ok(())
        };
        for (name, value) in raw.headers {
            insert(name, LocalHeader::Literal(value.0))?;
        }
        for header in raw.headers_from_env {
            insert(
                header.name,
                LocalHeader::Env {
                    env: header.value_from.env_var_ref.name,
                },
            )?;
        }
        Ok(Self {
            expected_endpoint: raw.expected_endpoint,
            headers,
        })
    }
}
pub enum LocalHeader {
    Literal(Zeroizing<String>),
    Env { env: String },
}
pub(crate) fn secret<'de, D: Deserializer<'de>>(
    d: D,
) -> std::result::Result<Zeroizing<String>, D::Error> {
    String::deserialize(d).map(Zeroizing::new)
}
impl LocalHeader {
    pub fn resolve(&self) -> Result<Option<Zeroizing<String>>> {
        match self {
            Self::Literal(value) => Ok((!value.is_empty()).then(|| value.clone())),
            Self::Env { env } => match std::env::var(env) {
                Ok(value) if !value.is_empty() => Ok(Some(Zeroizing::new(value))),
                Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => {
                    anyhow::bail!("Invalid local header encoding")
                }
            },
        }
    }
}
fn remote_labels() -> Option<Vec<String>> {
    Some(Vec::new())
}
fn refresh() -> u64 {
    60
}
fn limit() -> usize {
    5
}
impl SourceConfig {
    pub fn validate_catalog(&self, catalog: &mut temba_core::Catalog) -> Result<()> {
        for server in &mut catalog.servers {
            let local_stdio = self.local_stdio_endpoints.contains(&server.endpoint);
            if local_stdio {
                ensure!(
                    server.oauth.is_none() && server.headers.is_empty(),
                    "Invalid local stdio gateway definition"
                );
            }
            server.validate(self.allow_insecure_http || local_stdio)?;
        }
        catalog.validate(true)
    }

    #[cfg(test)]
    pub fn validate(&mut self) -> Result<()> {
        let u = temba_core::endpoint(&self.catalog_url, self.allow_insecure_http)?;
        ensure!(u.query().is_none(), "Catalog URL cannot have a query");
        ensure!(
            (1..=1440).contains(&self.refresh_minutes),
            "refresh_minutes must be between 1 and 1440 minutes"
        );
        ensure!(
            (1..=50).contains(&self.search_limit),
            "search_limit must be between 1 and 50"
        );
        self.catalog_url = self.catalog_url.trim_end_matches('/').to_owned();
        if let Some(labels) = &mut self.labels {
            *labels = labels.iter().map(|s| s.trim().to_lowercase()).collect();
        }
        ensure!(
            (1..=64).contains(&self.dynamic_catalog.max_servers),
            "Invalid local catalog limits"
        );
        for tool in self.tool.values() {
            validate_tool(tool, self.allow_insecure_http)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refresh_defaults_to_one_hour_and_accepts_one_minute_to_one_day() {
        let mut config: SourceConfig =
            serde_yaml_ng::from_str("catalog_url: https://catalog.example.com\n").unwrap();
        assert_eq!(config.refresh_minutes, 60);
        assert!(!config.allow_insecure_http);
        for minutes in [1, 60, 1440] {
            config.refresh_minutes = minutes;
            config.validate().unwrap();
        }
        for minutes in [0, 1441, u64::MAX] {
            config.refresh_minutes = minutes;
            assert!(config.validate().is_err());
        }
    }
    #[test]
    fn insecure_http_is_opt_in_for_local_and_remote_destinations() {
        for host in [
            "localhost",
            "127.0.0.1",
            "[::1]",
            "192.168.1.10",
            "example.com",
        ] {
            let mut config: SourceConfig = serde_yaml_ng::from_str(&format!(
                "catalog_url: http://{host}:18080\nallow_insecure_http: true\n"
            ))
            .unwrap();
            config.validate().unwrap();
            config.allow_insecure_http = false;
            assert!(config.validate().is_err());
        }
    }
    #[test]
    fn strings_are_literal_and_environment_references_are_explicit() {
        let mut config: SourceConfig = serde_yaml_ng::from_str(
            "catalog_url: https://catalog.example.com\ntool:\n  grafana:\n    expected_endpoint: https://grafana.example.com/mcp\n    headers:\n      Authorization: Bearer complete-value\n      named_string: PATH\n    headersFromEnv:\n    - name: from_env\n      valueFrom:\n        envVarRef:\n          name: PATH\n",
        )
        .unwrap();
        config.validate().unwrap();
        let headers = &config.tool["grafana"].headers;
        assert_eq!(
            headers["authorization"]
                .resolve()
                .unwrap()
                .unwrap()
                .as_str(),
            "Bearer complete-value"
        );
        assert_eq!(
            headers["named_string"].resolve().unwrap().unwrap().as_str(),
            "PATH"
        );
        assert_eq!(
            headers["from_env"].resolve().unwrap().unwrap().as_str(),
            std::env::var("PATH").unwrap()
        );
    }
    #[test]
    fn yaml_headers_are_nested_and_environment_values_remain_private() {
        let mut config: Config = serde_yaml_ng::from_str(
            r#"
catalogs:
  - name: team
    url: https://catalog.example.com
    tools:
      service:
        expected_endpoint: https://mcp.example.com/mcp
        headers:
          X-Region: "eu-west"
          X-Literal: "PATH"
        headersFromEnv:
          - name: Authorization
            valueFrom:
              envVarRef:
                name: PATH
          - name: X-Optional
            valueFrom:
              envVarRef:
                name: TEMBA_TEST_NONEXISTENT_8432580
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert!(config.dynamic_source().tool.is_empty());
        let sources = config.into_sources();
        let headers = &sources[0].1.tool["service"].headers;
        assert_eq!(
            headers["x-region"].resolve().unwrap().unwrap().as_str(),
            "eu-west"
        );
        assert_eq!(
            headers["x-literal"].resolve().unwrap().unwrap().as_str(),
            "PATH"
        );
        assert_eq!(
            headers["authorization"]
                .resolve()
                .unwrap()
                .unwrap()
                .as_str(),
            std::env::var("PATH").unwrap()
        );
        assert!(headers["x-optional"].resolve().unwrap().is_none());
    }
    #[test]
    fn yaml_rejects_duplicate_headers_invalid_env_and_legacy_shapes() {
        let start = "catalogs:\n  - name: team\n    url: https://catalog.example.com\n    tools:\n      service:\n        expected_endpoint: https://mcp.example.com/mcp\n";
        for tail in [
            "        headers: {Authorization: value, authorization: other}\n",
            "        headers:\n          X-Key: first\n          X-Key: second\n",
            "        headers: {Authorization: value}\n        headersFromEnv:\n          - name: authorization\n            valueFrom: {envVarRef: {name: PATH}}\n",
            "        headersFromEnv:\n          - name: X-Key\n            valueFrom: {envVarRef: {name: PATH}}\n          - name: x-key\n            valueFrom: {envVarRef: {name: PATH}}\n",
            "        headersFromEnv:\n          - name: X-Key\n            valueFrom: {envVarRef: {name: 'BAD-NAME'}}\n",
            "        headersFromEnv:\n          - name: 'Invalid Header'\n            valueFrom: {envVarRef: {name: PATH}}\n",
            "        headers: {X-Key: {env: PATH}}\n",
            "        headersFromEnv:\n          - name: X-Key\n            valueFrom: {secretKeyRef: {name: secret, key: token}}\n",
        ] {
            let parsed = serde_yaml_ng::from_str::<Config>(&format!("{start}{tail}"));
            assert!(
                parsed.is_err() || parsed.unwrap().validate().is_err(),
                "accepted invalid headers"
            );
        }
        for input in [
            "[[catalogs]]\nid = 'team'\nurl = 'https://catalog.example.com'",
            "catalogs: []\ntool: {}",
            "catalogs:\n  - name: team\n    url: https://catalog.example.com\n    tools:\n      same: {expected_endpoint: 'https://mcp.example.com'}\n      same: {expected_endpoint: 'https://other.example.com'}",
        ] {
            assert!(serde_yaml_ng::from_str::<Config>(input).is_err());
        }
    }
    #[test]
    fn old_config_and_ambiguous_environment_objects_are_rejected() {
        for input in [
            "catalog_url: https://catalog.example.com\ncredentials:\n  grafana:\n    expected_endpoint: https://grafana.example.com/mcp\n",
            "catalog_url: https://catalog.example.com\ntool:\n  grafana:\n    expected_endpoint: https://grafana.example.com/mcp\n    headers:\n      X-Key:\n        env: PATH\n        value: secret\n",
        ] {
            assert!(serde_yaml_ng::from_str::<SourceConfig>(input).is_err());
        }
    }
}

#[cfg(test)]
mod federation_tests {
    use super::*;
    #[test]
    fn label_selection_belongs_only_to_each_remote_catalog() {
        let mut config: Config = serde_yaml_ng::from_str(
            r#"
catalogs:
  - name: first
    url: https://first.example.com
    labels: [" Support "]
  - name: second
    url: https://second.example.com
    labels: ["DOCUMENTATION"]
  - name: defaults
    url: https://default.example.com
local_catalog:
  enabled: true
  servers:
    - name: service
      endpoint: https://mcp.example.com/mcp
      labels: [private-category]
      access: {public: true}
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert!(config.dynamic_source().labels.is_none());
        let sources = config.into_sources();
        assert_eq!(
            sources[0].1.labels.as_deref(),
            Some(["support".to_owned()].as_slice())
        );
        assert_eq!(
            sources[1].1.labels.as_deref(),
            Some(["documentation".to_owned()].as_slice())
        );
        assert_eq!(sources[2].1.labels.as_deref(), Some([].as_slice()));
        assert!(sources[3].1.labels.is_none());
        for invalid in [
            "labels: []",
            "local_catalog: {labels: []}",
            "dynamic_catalog: {labels: []}",
        ] {
            assert!(serde_yaml_ng::from_str::<Config>(invalid).is_err());
        }
    }
    #[test]
    fn local_source_is_automatic_and_carries_only_its_own_credentials() {
        let mut config: Config = serde_yaml_ng::from_str(
            r#"
catalogs:
  - name: team
    url: https://catalog.example.com
local_catalog:
  enabled: true
  servers:
    - name: service
      endpoint: https://mcp.example.com/mcp
      access: {public: true}
  tools:
    service:
      expected_endpoint: https://mcp.example.com/mcp
      headers: {X-Key: private-local-value}
"#,
        )
        .unwrap();
        config.validate().unwrap();
        let sources = config.into_sources();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[1].0, "local");
        assert_eq!(sources[1].1.catalog_url, "http://127.0.0.1:0");
        assert!(!sources[1].1.allow_insecure_http);
        assert!(sources[0].1.tool.is_empty());
        assert_eq!(
            sources[1].1.tool["service"].headers["x-key"]
                .resolve()
                .unwrap()
                .unwrap()
                .as_str(),
            "private-local-value"
        );
    }
    #[test]
    fn local_activation_is_explicit_and_empty_catalogs_are_not_included() {
        for (options, expected) in [
            ("", None),
            ("enabled: false", None),
            ("enabled: true", Some("http://127.0.0.1:0")),
        ] {
            let mut config: Config = serde_yaml_ng::from_str(&format!("local_catalog:\n  {options}\n  servers:\n    - name: service\n      endpoint: https://mcp.example.com/mcp\n      access: {{public: true}}\n")).unwrap();
            config.validate().unwrap();
            let sources = config.into_sources();
            assert_eq!(sources.first().map(|s| s.1.catalog_url.as_str()), expected);
        }
        let mut empty: Config = serde_yaml_ng::from_str("local_catalog: {enabled: true}").unwrap();
        empty.validate().unwrap();
        assert!(empty.into_sources().is_empty());
        let mut reserved: Config = serde_yaml_ng::from_str(
            "catalogs: [{name: local, url: 'https://catalog.example.com'}]",
        )
        .unwrap();
        assert!(reserved.validate().is_err());
    }
    #[test]
    fn validates_namespaces_and_static_definitions_without_legacy_ambiguity() {
        for input in [
            "catalog_url: https://old.example\n",
            "local_catalog: {listen: '127.0.0.1:8787'}",
            "local_catalog: {address: '0.0.0.0:8787'}",
            "catalogs:\n- name: same\n  url: https://a.example\n- name: same\n  url: https://b.example\n",
            "catalogs:\n- name: has.dot\n  url: https://a.example\n",
            "catalogs:\n- name: __local\n  url: https://a.example\n",
            "catalogs:\n- name: a\n  url: https://a.example\ntool:\n  unknown:\n    mcp:\n      expected_endpoint: https://mcp.example\n",
            "local_catalog:\n  servers:\n  - name: duplicate\n    endpoint: https://a.example\n  - name: duplicate\n    endpoint: https://b.example\n",
            "local_catalog:\n  servers:\n  - name: protected\n    endpoint: https://a.example\n    access:\n      authenticated: true\n",
        ] {
            let parsed = serde_yaml_ng::from_str::<Config>(input);
            assert!(
                parsed.is_err() || parsed.unwrap().validate().is_err(),
                "accepted {input}"
            );
        }
        let mut config: Config = serde_yaml_ng::from_str("search_limit: 5\ncatalogs:\n- name: team\n  url: https://catalog.example/\n  tools:\n    service:\n      expected_endpoint: https://mcp.example\n      headers:\n        X-Key: literal-value\ndynamic_catalog:\n  enabled: true\nlocal_catalog:\n  servers:\n  - name: service\n    endpoint: https://mcp.example\n").unwrap();
        config.validate().unwrap();
        assert!(config.dynamic_source().tool.is_empty());
        let sources = config.into_sources();
        assert_eq!(sources[0].0, "team");
        assert_eq!(sources[0].1.catalog_url, "https://catalog.example");
        assert!(sources[0].1.tool.contains_key("service"));
        assert!(!sources[0].1.dynamic_catalog.enabled);
    }
}
