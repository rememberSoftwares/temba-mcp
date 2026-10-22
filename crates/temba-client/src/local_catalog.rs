//! Ephemeral, non-enumerable registrations. Possession of an ID authorizes access.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::sync::Mutex as StdMutex;

pub(super) struct Personal {
    pub label: String,
    pub headers: StdMutex<BTreeMap<String, Zeroizing<String>>>,
}
#[derive(Debug)]
pub(super) struct RegistrationError(pub &'static str);
impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for RegistrationError {}

#[derive(Debug)]
pub(super) struct LocalOAuthOriginNotAllowed(pub String);
impl std::fmt::Display for LocalOAuthOriginNotAllowed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "OAuth origin {} is not approved for this ephemeral local MCP. Disconnect this registration and call add_mcp again with definition.oauth.allowedOrigins including this provider's trusted origin, then connect the new local_id. No central catalog change or Temba catalog authentication is required. Do not disable OAuth origin validation.",
            self.0
        )
    }
}
impl std::error::Error for LocalOAuthOriginNotAllowed {}

impl Local {
    pub(super) async fn add_personal(
        self: &Arc<Self>,
        mut definition: Server,
        headers: BTreeMap<String, String>,
    ) -> Result<Value> {
        if !self.config.local_catalog.enabled {
            return Err(RegistrationError("Local catalog is disabled.").into());
        }
        definition
            .validate(self.config.allow_insecure_http)
            .map_err(|_| {
                RegistrationError(
                    "Invalid local MCP definition; check endpoint, OAuth and header declarations.",
                )
            })?;
        if !definition.access.public {
            return Err(RegistrationError("Local MCP access is controlled by its secret ID; catalog group access rules do not apply.").into());
        }
        if headers.len() > 32
            || headers.iter().any(|(reference, value)| {
                value.len() > 16384
                    || !definition
                        .headers
                        .iter()
                        .any(|h| h.credential.as_ref() == Some(reference))
            })
        {
            return Err(RegistrationError("Private headers must match the definition's credential references and size limits.").into());
        }
        let label = definition.id.clone();
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|_| RegistrationError("Random ID generation failed."))?;
        let id = format!("__local_{}", URL_SAFE_NO_PAD.encode(bytes));
        definition.id = id.clone();
        let mut entry = Entry::new(definition);
        entry.personal = Some(Personal {
            label,
            headers: StdMutex::new(
                headers
                    .into_iter()
                    .map(|(k, v)| (k, Zeroizing::new(v)))
                    .collect(),
            ),
        });
        self.credential_headers(&entry).map_err(|error| {
            if error.is::<MissingHeaders>() {
                RegistrationError("Mandatory private headers are missing. Supply their full values in add_mcp.headers using the credential references.")
            } else {
                RegistrationError("Invalid local header values.")
            }
        })?;
        let entry = Arc::new(entry);
        {
            let mut entries = self.personal.write().await;
            if entries.len() >= self.config.local_catalog.max_servers {
                return Err(RegistrationError(
                    "Local catalog limit reached. Disconnect an existing local MCP to free a slot.",
                )
                .into());
            }
            // Never overwrite a registration, even in the event of a random collision.
            if entries.contains_key(&id) {
                return Err(RegistrationError("Random ID collision; retry registration.").into());
            }
            entries.insert(id.clone(), entry.clone());
        }
        let status = entry.state.read().await.status;
        if entry.definition.oauth.is_none() {
            let owner = self.clone();
            tokio::spawn(async move {
                owner.connect(&entry).await;
            });
        }
        Ok(json!({
            "local_id":id,
            "status":status,
            "connect_tool_id":format!("{id}.__temba_connect__"),
            "hint":"Keep local_id in this conversation. Pass local_ids:[local_id] to tool_search to discover its tools, then use their returned tool_id with call_tool. Local entries are not listed without their ID. For auth_required, use connect_tool_id to start this MCP's OAuth flow. For connecting, search again; do not report ready until confirmed. Catalog authentication is not required for this local registration. OAuth providers are discovered automatically over public destinations unless definition.oauth.allowedOrigins contains an explicit restriction. This registration stays available until explicitly disconnected or the Temba process stops; inactivity does not remove it. Nothing is saved to the central catalog or a local file."
        }))
    }

    pub(super) async fn personal_entry(&self, id: &str) -> Option<Arc<Entry>> {
        let entry = self.personal.read().await.get(id).cloned()?;
        if entry.retired.load(Ordering::SeqCst) {
            return None;
        }
        Some(entry)
    }
}
