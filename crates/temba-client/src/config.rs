use anyhow::{Result, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
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
    pub credentials: BTreeMap<String, Credential>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub token_env: String,
    pub expected_endpoint: String,
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
        for c in self.credentials.values() {
            temba_core::endpoint(&c.expected_endpoint, self.allow_loopback_http)?;
            ensure!(
                !c.token_env.is_empty(),
                "Missing token environment variable name"
            );
        }
        Ok(())
    }
}
