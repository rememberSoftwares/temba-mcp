use crate::config;
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
};
use temba_core::Server;
use zeroize::Zeroizing;

#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub enum LocalServer {
    Http(Box<Server>),
    Stdio(Box<StdioServer>),
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StdioServer {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default, deserialize_with = "config::unique_map")]
    pub env: BTreeMap<String, EnvValue>,
    #[serde(default, rename = "envFromEnv")]
    pub env_from_env: Vec<EnvReference>,
    #[serde(default = "timeout", rename = "timeoutSeconds")]
    pub timeout_seconds: u64,
}
#[derive(Clone, Deserialize)]
pub struct EnvValue(#[serde(deserialize_with = "config::secret")] Zeroizing<String>);
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvReference {
    name: String,
    #[serde(rename = "valueFrom")]
    value_from: EnvSource,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvSource {
    #[serde(rename = "envVarRef")]
    env_var_ref: EnvVar,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvVar {
    name: String,
}
fn timeout() -> u64 {
    60
}
fn env_name(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
impl LocalServer {
    pub fn public_definition(&self, listen: SocketAddr) -> Server {
        match self {
            Self::Http(server) => server.as_ref().clone(),
            Self::Stdio(server) => Server {
                name: server.name.clone(),
                description: server.description.clone(),
                endpoint: format!("http://{}/mcp/{}", listen, server.name),
                labels: Vec::new(),
                oauth: None,
                headers: Vec::new(),
                access: Default::default(),
                timeout_seconds: server.timeout_seconds,
            },
        }
    }
    pub fn validate(&mut self, listen: SocketAddr, allow_http: bool) -> Result<()> {
        match self {
            Self::Http(server) => {
                server.validate(allow_http)?;
                ensure!(
                    server.access.public,
                    "Local definitions must use public access"
                );
            }
            Self::Stdio(server) => {
                ensure!(
                    listen.ip().is_loopback(),
                    "Local stdio gateway must use a loopback address"
                );
                server.validate()?;
                self.public_definition(listen).validate(true)?;
            }
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct MissingEnvironment(pub String);
impl std::fmt::Display for MissingEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "envFromEnv: environment variable '{}' is missing or is not valid Unicode. Export it before starting the AI host, then restart its Temba process",
            self.0.escape_debug()
        )
    }
}
impl std::error::Error for MissingEnvironment {}
impl StdioServer {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.command.trim().is_empty()
                && self.command.len() <= 4096
                && !self.command.contains('\0'),
            "Invalid local MCP command"
        );
        ensure!(
            self.args.len() <= 256
                && self
                    .args
                    .iter()
                    .all(|v| v.len() <= 16384 && !v.contains('\0')),
            "Invalid local MCP arguments"
        );
        ensure!(
            self.cwd.as_ref().is_none_or(|p| p.is_absolute()),
            "Local MCP cwd must be an absolute path"
        );
        ensure!(
            self.env.len() + self.env_from_env.len() <= 64,
            "Too many local MCP environment variables"
        );
        let mut names = HashSet::new();
        for (name, value) in &self.env {
            ensure!(
                env_name(name) && names.insert(name.to_ascii_uppercase()),
                "Invalid or duplicate environment variable name"
            );
            ensure!(
                value.0.len() <= 16384 && !value.0.contains('\0'),
                "Invalid environment value"
            );
        }
        for reference in &self.env_from_env {
            ensure!(
                env_name(&reference.name)
                    && env_name(&reference.value_from.env_var_ref.name)
                    && names.insert(reference.name.to_ascii_uppercase()),
                "Invalid or duplicate environment reference"
            );
        }
        Ok(())
    }
    pub fn process(&self) -> Result<tokio::process::Command> {
        let mut command = rmcp::transport::which_command(&self.command)?;
        command.args(&self.args).kill_on_drop(true).env_clear();
        for name in [
            "PATH",
            "HOME",
            "USERPROFILE",
            "SYSTEMROOT",
            "WINDIR",
            "TMP",
            "TEMP",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "APPDATA",
            "LOCALAPPDATA",
            "PATHEXT",
            "COMSPEC",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        for (name, value) in &self.env {
            command.env(name, value.0.as_str());
        }
        for reference in &self.env_from_env {
            let value = Zeroizing::new(
                std::env::var(&reference.value_from.env_var_ref.name).map_err(|_| {
                    anyhow::Error::new(MissingEnvironment(
                        reference.value_from.env_var_ref.name.clone(),
                    ))
                })?,
            );
            command.env(&reference.name, value.as_str());
        }
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_catalog_keeps_commands_private_and_only_trusts_its_exact_proxy_urls() {
        let mut config: config::Config = serde_yaml_ng::from_str(
            r#"
local_catalog:
  enabled: true
  servers:
    - name: network
      endpoint: https://mcp.example.com/mcp
    - name: disk
      command: private-command
      args: [private-argument]
      env: {TOKEN: private-token}
      envFromEnv:
        - name: API_KEY
          valueFrom: {envVarRef: {name: PRIVATE_ENV}}
"#,
        )
        .unwrap();
        config.validate().unwrap();
        let local = config.local_catalog.as_ref().unwrap();
        let definitions: Vec<_> = local
            .servers
            .iter()
            .map(|s| s.public_definition(local.address))
            .collect();
        let published = serde_json::to_string(&definitions).unwrap();
        for secret in [
            "private-command",
            "private-argument",
            "private-token",
            "PRIVATE_ENV",
            "envFromEnv",
            "command",
        ] {
            assert!(!published.contains(secret));
        }
        assert!(published.contains("http://127.0.0.1:0/mcp/disk"));
        let sources = config.into_sources();
        let source = &sources[0].1;
        let mut catalog = temba_core::Catalog {
            schema_version: 2,
            revision: String::new(),
            servers: definitions,
        };
        source.validate_catalog(&mut catalog).unwrap();
        catalog.servers[0].endpoint = "http://127.0.0.1:9999/unapproved".into();
        assert!(source.validate_catalog(&mut catalog).is_err());
        assert!(
            serde_json::from_value::<Server>(
                serde_json::json!({"name":"disk","command":"private-command"})
            )
            .is_err()
        );
    }
    #[test]
    fn invalid_commands_mixed_transports_and_network_execution_are_rejected() {
        for definition in [
            "{name: disk, command: program, endpoint: 'https://mcp.example.com'}",
            "{name: disk, command: ''}",
            "{name: disk, command: program, cwd: relative/path}",
            "{name: disk, command: program, timeoutSeconds: 0}",
            "{name: disk, command: program, env: {TOKEN: first, token: second}}",
            "{name: disk, command: program, env: {'BAD-NAME': value}}",
            "{name: disk, command: program, env: {TOKEN: first}, envFromEnv: [{name: TOKEN, valueFrom: {envVarRef: {name: ENV}}}]}",
        ] {
            let input = format!("local_catalog:\n  enabled: true\n  servers: [{definition}]");
            let parsed = serde_yaml_ng::from_str::<config::Config>(&input);
            assert!(
                parsed.is_err() || parsed.unwrap().validate().is_err(),
                "accepted invalid stdio definition"
            );
        }
        let mut config: config::Config = serde_yaml_ng::from_str("local_catalog: {servers: [{name: same, command: program}, {name: same, endpoint: 'https://mcp.example.com'}]}").unwrap();
        assert!(config.validate().is_err());
    }
}
