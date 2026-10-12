use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use zeroize::Zeroizing;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub catalog_url: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "refresh")]
    pub refresh_seconds: u64,
    #[serde(default = "limit")]
    pub search_limit: usize,
    #[serde(default)]
    pub allow_loopback_http: bool,
    #[serde(default)]
    pub tool: BTreeMap<String, ToolConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    pub expected_endpoint: String,
    #[serde(default)]
    pub headers: BTreeMap<String, LocalHeader>,
}
#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum LocalHeader {
    Literal(#[serde(deserialize_with = "secret")] Zeroizing<String>),
    Env { env: String },
}
fn secret<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Zeroizing<String>, D::Error> {
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
fn refresh() -> u64 {
    60
}
fn limit() -> usize {
    5
}
impl Config {
    pub fn validate(&mut self) -> Result<()> {
        let u = temba_core::endpoint(&self.catalog_url, self.allow_loopback_http)?;
        ensure!(u.query().is_none(), "Catalog URL cannot have a query");
        ensure!(
            (10..=3600).contains(&self.refresh_seconds) && (1..=50).contains(&self.search_limit),
            "Invalid refresh/search limits"
        );
        self.catalog_url = self.catalog_url.trim_end_matches('/').to_owned();
        self.labels = self
            .labels
            .iter()
            .map(|s| s.trim().to_lowercase())
            .collect();
        for tool in self.tool.values() {
            temba_core::endpoint(&tool.expected_endpoint, self.allow_loopback_http)?;
            ensure!(tool.headers.len() <= 32, "Too many local header mappings");
            for header in tool.headers.values() {
                match header {
                    LocalHeader::Env { env } => ensure!(
                        !env.is_empty()
                            && env.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                        "Invalid header environment variable name"
                    ),
                    LocalHeader::Literal(value) => {
                        ensure!(value.len() <= 16384, "Local header value too long")
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strings_are_literal_and_environment_references_are_explicit() {
        let mut config: Config = toml::from_str(
            r#"
catalog_url = "https://catalog.example.com"
[tool.grafana]
expected_endpoint = "https://grafana.example.com/mcp"
[tool.grafana.headers]
api_token = "Bearer complete-value"
named_string = "PATH"
from_env = { env = "PATH" }
"#,
        )
        .unwrap();
        config.validate().unwrap();
        let headers = &config.tool["grafana"].headers;
        assert_eq!(
            headers["api_token"].resolve().unwrap().unwrap().as_str(),
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
    fn old_config_and_ambiguous_environment_objects_are_rejected() {
        for input in [
            "catalog_url='https://catalog.example.com'\n[credentials.grafana]\nexpected_endpoint='https://grafana.example.com/mcp'\n",
            "catalog_url='https://catalog.example.com'\n[tool.grafana]\nexpected_endpoint='https://grafana.example.com/mcp'\n[tool.grafana.headers]\nkey={env='PATH',value='secret'}",
        ] {
            assert!(toml::from_str::<Config>(input).is_err());
        }
    }
}
