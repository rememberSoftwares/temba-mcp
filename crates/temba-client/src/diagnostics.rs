//! User-facing configuration errors. Never forward parser input values or source excerpts.
use anyhow::{Context, Result};
use serde::de::IntoDeserializer;
use serde_yaml_ng::Value;

pub fn config(source: &str, filename: &str) -> Result<crate::config::Config> {
    let parsed = serde_path_to_error::deserialize(serde_yaml_ng::Deserializer::from_str(source));
    let mut config: crate::config::Config = parsed.map_err(|error| {
        let location = error.inner().location().map(|p| format!(" near line {}, column {}", p.line(), p.column())).unwrap_or_default();
        let mut path = error.path().to_string();
        let mut message = error.inner().to_string();
        // Untagged transport selection otherwise hides the useful field error.
        if let Some(index) = path.strip_prefix("local_catalog.servers[")
            .and_then(|s| s.split(']').next()).and_then(|s| s.parse::<usize>().ok())
            && let Ok(value) = serde_yaml_ng::from_str::<Value>(source)
            && let Some(node) = value.get("local_catalog").and_then(|v| v.get("servers")).and_then(|v| v.get(index))
        {
            if node.get("command").is_some() && node.get("endpoint").is_some() {
                message = "Choose exactly one transport: endpoint for HTTP, or command with args for stdio".into();
            } else if node.get("command").is_some() {
                if let Err(error) = serde_path_to_error::deserialize::<_, crate::local_server::StdioServer>(node.clone().into_deserializer()) {
                    let inner = error.path().to_string();
                    path = format!("local_catalog.servers[{index}]");
                    if inner != "." { path.push('.'); path.push_str(&inner); }
                    message = error.inner().to_string();
                }
            } else if let Err(error) = serde_path_to_error::deserialize::<_, temba_core::Server>(node.clone().into_deserializer()) {
                let inner = error.path().to_string();
                path = format!("local_catalog.servers[{index}]");
                if inner != "." { path.push('.'); path.push_str(&inner); }
                message = error.inner().to_string();
            }
        }
        anyhow::anyhow!("Configuration '{}'{} at {}: {}", filename.escape_debug(), location,
            if path == "." { "<root>".into() } else { path.escape_debug().to_string() }, explain(&message))
    })?;
    config.validate().with_context(|| {
        format!(
            "Configuration '{}': validation failed",
            filename.escape_debug()
        )
    })?;
    Ok(config)
}

fn quoted_field<'a>(message: &'a str, prefix: &str) -> Option<&'a str> {
    let (before, rest) = message.split_once(prefix)?;
    if !before.is_empty()
        && !before.strip_suffix(": ").is_some_and(|p| {
            p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'[' | b']' | b'-'))
        })
    {
        return None;
    }
    let field = rest.split('`').next()?;
    (field.len() <= 64
        && field
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
    .then_some(field)
}
fn explain(message: &str) -> String {
    if let Some(field) = quoted_field(message, "unknown field `") {
        let hint = match field {
            "id" => {
                "Replace id with name. Catalog names must be unique; MCP names must be unique within each catalog."
            }
            "access" => {
                "Remove access from command/args entries; local stdio MCPs do not use catalog access rules."
            }
            "listen" => {
                "Remove listen; the local catalog starts automatically and chooses its own internal port."
            }
            "labels" => {
                "Put labels under each remote catalogs[] entry; local MCPs have no label filter."
            }
            _ => "Check this field's spelling and nesting in temba.example.yaml.",
        };
        return format!("Unknown field '{field}'. {hint}");
    }
    if let Some(field) = quoted_field(message, "missing field `") {
        return format!(
            "Missing required field '{field}'. Add it to this entry; see temba.example.yaml."
        );
    }
    if let Some(field) = quoted_field(message, "duplicate field `") {
        return format!("Field '{field}' is defined twice. Keep a single value.");
    }
    // Only expected types come from the schema. The invalid value may be a secret.
    if (message.contains("invalid type:") || message.contains("invalid value:"))
        && let Some((_, expected)) = message.rsplit_once(", expected ")
    {
        let expected = expected.split(" at line ").next().unwrap_or("");
        return format!(
            "Wrong value type or format; expected {}. Check YAML quoting and indentation.",
            expected.escape_debug()
        );
    }
    for safe in [
        "Invalid HTTP header name",
        "Duplicate HTTP header (names are case-insensitive)",
        "Duplicate mapping key",
        "Choose exactly one transport: endpoint for HTTP, or command with args for stdio",
    ] {
        if message.contains(safe) {
            return safe.into();
        }
    }
    "Invalid YAML syntax or configuration structure. Check indentation, colons, quotes and list markers against temba.example.yaml; configuration values are omitted from this error.".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn error(source: &str) -> String {
        format!(
            "{:#}",
            config(source, "example.yaml")
                .err()
                .expect("must reject invalid config")
        )
    }
    #[test]
    fn transport_fields_and_removed_options_have_actionable_paths() {
        let message = error(
            "local_catalog:\n  enabled: true\n  servers:\n    - name: program\n      command: uvx\n      access: {public: true}\n",
        );
        assert!(message.contains("local_catalog.servers[0]"), "{message}");
        assert!(message.contains("Unknown field 'access'"), "{message}");
        assert!(message.contains("Remove access"));
        assert!(message.contains("line"));
        assert!(error("local_catalog: {listen: '127.0.0.1:8787'}").contains("Remove listen"));
        assert!(error("local_catalog: {servers: [{name: program, command: uvx, endpoint: 'https://mcp.example'}]}").contains("Choose exactly one transport"));
    }
    #[test]
    fn validation_identifies_entries_and_concrete_limits() {
        let message =
            error("local_catalog: {servers: [{name: program, command: uvx, cwd: relative/path}]}");
        assert!(message.contains("local_catalog.servers[0]"));
        assert!(message.contains("absolute path"));
        assert!(error("refresh_minutes: 0").contains("between 1 and 1440"));
        assert!(error("catalogs: [{name: a, url: 'https://example.com'}, {name: a, url: 'https://example.org'}]").contains("catalogs[1].name: duplicate"));
        assert!(error("local_catalog: {tools: {service: {expected_endpoint: 'https://example.com', headers: {Authorization: TOP_SECRET_TOKEN, authorization: OTHER_SECRET_TOKEN}}}}").contains("Duplicate HTTP header"));
    }
    #[test]
    fn nested_types_missing_fields_and_syntax_do_not_echo_secrets() {
        for source in [
            "refresh_minutes: TOP_SECRET_TOKEN",
            "refresh_minutes: 'unknown field `TOP_SECRET_TOKEN`'",
            "refresh_minutes: 'TOP_SECRET_TOKEN, expected OTHER_SECRET_TOKEN'",
            "catalogs: [{url: 'https://example.com?token=TOP_SECRET_TOKEN'}]",
            "local_catalog: {servers: [{name: program, command: uvx, env: {TOKEN: [TOP_SECRET_TOKEN]}}]}",
            "catalogs: [\n token: TOP_SECRET_TOKEN\n bad: [",
        ] {
            let message = error(source);
            assert!(message.contains("example.yaml"));
            assert!(!message.contains("TOP_SECRET_TOKEN"), "{message}");
            assert!(!message.contains("OTHER_SECRET_TOKEN"), "{message}");
        }
        assert!(error("refresh_minutes: not_a_number").contains("refresh_minutes"));
        assert!(
            error("catalogs: [{url: 'https://example.com'}]")
                .contains("Missing required field 'name'")
        );
    }
}
