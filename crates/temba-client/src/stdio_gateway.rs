use crate::local_server::StdioServer;
use rmcp::{
    ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt,
    model::*,
    service::{RequestContext, RunningService},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub struct Gateway {
    spec: StdioServer,
    upstream: Mutex<Option<Upstream>>,
    shutdown: CancellationToken,
}
struct ChildGuard(Option<Box<dyn process_wrap::tokio::ChildWrapper>>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.start_kill();
        }
    }
}
struct Upstream {
    service: RunningService<RoleClient, ()>,
    child: ChildGuard,
}
impl Upstream {
    async fn close(self) {
        let Self { service, mut child } = self;
        let _ = tokio::time::timeout(Duration::from_secs(2), service.cancel()).await;
        if let Some(mut process) = child.0.take() {
            let _ = process.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), process.wait()).await;
        }
    }
}
#[derive(Clone)]
pub struct Handler(pub Arc<Gateway>);
impl Gateway {
    pub fn new(spec: StdioServer, shutdown: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            spec,
            upstream: Mutex::new(None),
            shutdown,
        })
    }
    fn error(&self, message: &str) -> ErrorData {
        ErrorData::internal_error(format!("Local MCP {}: {message}", self.spec.name), None)
    }
    fn duration(&self) -> Duration {
        Duration::from_secs(self.spec.timeout_seconds)
    }
    async fn peer(&self) -> Result<rmcp::Peer<RoleClient>, ErrorData> {
        let connect = async {
            let mut upstream = self.upstream.lock().await;
            if upstream
                .as_ref()
                .is_some_and(|s| s.service.is_closed() || s.service.peer().is_transport_closed())
                && let Some(previous) = upstream.take()
            {
                previous.close().await;
            }
            if upstream.is_none() {
                let command = self
                    .spec
                    .process()
                    .map_err(|error| {
                        if let Some(missing) = error.downcast_ref::<crate::local_server::MissingEnvironment>() {
                            self.error(&missing.to_string())
                        } else {
                            self.error("executable unavailable; install the configured command or use an absolute executable path, and check the PATH inherited from your AI host")
                        }
                    })?;
                let mut command = process_wrap::tokio::CommandWrap::from(command);
                command.wrap(process_wrap::tokio::KillOnDrop);
                #[cfg(unix)]
                command.wrap(process_wrap::tokio::ProcessGroup::leader());
                #[cfg(windows)]
                command.wrap(process_wrap::tokio::JobObject);
                command
                    .command_mut()
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::inherit());
                let mut child = ChildGuard(Some(command.spawn().map_err(|error| {
                    let hint = match error.kind() {
                        std::io::ErrorKind::NotFound => "executable or cwd directory not found; check installation and use an existing absolute cwd",
                        std::io::ErrorKind::PermissionDenied => "permission denied; check executable permissions and access to cwd",
                        _ => "could not start the configured command; check executable installation and cwd",
                    };
                    self.error(hint)
                })?));
                let process = child.0.as_mut().unwrap().inner_mut();
                let stdin = process
                    .stdin()
                    .take()
                    .ok_or_else(|| self.error("stdin pipe unavailable"))?;
                let stdout = process
                    .stdout()
                    .take()
                    .ok_or_else(|| self.error("stdout pipe unavailable"))?;
                let service = ()
                    .serve((stdout, stdin))
                    .await
                    .map_err(|_| self.error("stdio initialization failed; check local stderr"))?;
                eprintln!("MCP {}: stdio process ready", self.spec.name);
                *upstream = Some(Upstream { service, child });
            }
            Ok(upstream.as_ref().unwrap().service.peer().clone())
        };
        let result = tokio::select! {
            _ = self.shutdown.cancelled() => Err(self.error("server is shutting down")),
            result = tokio::time::timeout(self.duration(), connect) => result.unwrap_or_else(|_| Err(self.error("process startup timed out"))),
        };
        if let Err(error) = &result {
            eprintln!("MCP {}: {}", self.spec.name, error.message);
        }
        result
    }
    pub async fn close(&self) {
        if let Some(service) = self.upstream.lock().await.take() {
            service.close().await;
            eprintln!("MCP {}: stdio process stopped", self.spec.name);
        }
    }
}
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        context.peer.set_peer_info(request.clone());
        self.0.peer().await?;
        self.negotiate_initialize(&request)
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let peer = self.0.peer().await?;
        let result = tokio::select! {
            _ = self.0.shutdown.cancelled() => return Err(self.0.error("server is shutting down")),
            result = tokio::time::timeout(self.0.duration(), peer.list_all_tools()) => result,
        };
        let tools = match result {
            Ok(result) => result.map_err(|_| {
                self.0
                    .error("tools/list failed; reconnect to restart a crashed process")
            })?,
            Err(_) => {
                self.0.close().await;
                return Err(self.0.error("tools/list timed out"));
            }
        };
        if tools.len() > 10000 {
            return Err(self.0.error("too many tools"));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let peer = self.0.peer().await?;
        let result = tokio::select! {
            _ = self.0.shutdown.cancelled() => return Err(self.0.error("server is shutting down")),
            result = tokio::time::timeout(self.0.duration(), peer.call_tool(request)) => result,
        };
        match result {
            Ok(result) => result
                .map(CallToolResponse::Complete)
                .map_err(|_| self.0.error("tool call failed; it was not retried")),
            Err(_) => {
                self.0.close().await;
                Err(self.0.error(
                    "tool call timed out; the process was stopped and the call was not retried",
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn definition(timeout: u64) -> StdioServer {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stdio_mcp.py");
        serde_json::from_value(
            json!({"name":"fixture", "command":if cfg!(windows) {"python"} else {"python3"},
            "args":[fixture, "; this-is-an-argument-not-a-shell-command"], "timeoutSeconds":timeout,
            "cwd":env!("CARGO_MANIFEST_DIR"), "env":{"FIXTURE_TOKEN":"private-token"},
            "envFromEnv":[{"name":"FIXTURE_REF","valueFrom":{"envVarRef":{"name":"PATH"}}}]}),
        )
        .unwrap()
    }
    async fn echo(gateway: &Gateway, arguments: Value) -> Result<Value, rmcp::ServiceError> {
        let peer = gateway.peer().await.unwrap();
        let result = peer
            .call_tool(
                CallToolRequestParams::new("echo")
                    .with_arguments(arguments.as_object().unwrap().clone()),
            )
            .await?;
        let text = result.content[0].as_text().unwrap();
        Ok(serde_json::from_str(&text.text).unwrap())
    }
    #[tokio::test]
    async fn missing_environment_names_the_reference_without_exposing_values() {
        let name = format!("TEMBA_MISSING_{}", uuid::Uuid::new_v4().simple());
        let mut spec = definition(3);
        spec.env_from_env = serde_json::from_value(
            json!([{"name":"API_TOKEN","valueFrom":{"envVarRef":{"name":name}}}]),
        )
        .unwrap();
        let gateway = Gateway::new(spec, CancellationToken::new());
        let error = gateway
            .peer()
            .await
            .expect_err("missing variable must fail");
        assert!(error.message.contains(&name));
        assert!(error.message.contains("restart"));
        assert!(!error.message.contains("private-token"));
        gateway.close().await;
    }
    #[tokio::test]
    async fn real_process_preserves_arguments_env_and_state_then_recovers_after_crash() {
        let gateway = Gateway::new(definition(3), CancellationToken::new());
        let peer = gateway.peer().await.unwrap();
        assert_eq!(peer.list_all_tools().await.unwrap()[0].name, "echo");
        let first = echo(&gateway, json!({"value":"hello"})).await.unwrap();
        let second = echo(&gateway, json!({"value":"again"})).await.unwrap();
        assert_eq!(first["pid"], second["pid"]);
        assert_eq!(first["literal"], "private-token");
        assert_eq!(first["reference"], std::env::var("PATH").unwrap());
        assert_eq!(first["cwd"], env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            first["startup_args"][0],
            "; this-is-an-argument-not-a-shell-command"
        );
        assert!(
            !first["env_names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str().unwrap().starts_with("CARGO_"))
        );
        assert!(echo(&gateway, json!({"crash":true})).await.is_err());
        let recovered = echo(&gateway, json!({"value":"recovered"})).await.unwrap();
        assert_ne!(first["pid"], recovered["pid"]);
        gateway.close().await;
    }
    #[tokio::test]
    async fn call_timeout_stops_the_process_without_replaying_the_request() {
        let gateway = Gateway::new(definition(1), CancellationToken::new());
        let first = echo(&gateway, json!({"value":"before-timeout"}))
            .await
            .unwrap();
        let (front, back) = tokio::io::duplex(65536);
        let handler = Handler(gateway.clone());
        let server =
            tokio::spawn(async move { handler.serve(tokio::io::split(back)).await.unwrap() });
        let client = ().serve(tokio::io::split(front)).await.unwrap();
        let server = server.await.unwrap();
        let request = CallToolRequestParams::new("echo")
            .with_arguments(json!({"hang":true}).as_object().unwrap().clone());
        let result = tokio::time::timeout(Duration::from_secs(6), client.peer().call_tool(request))
            .await
            .unwrap();
        assert!(result.is_err());
        let after = echo(&gateway, json!({"value":"after-timeout"}))
            .await
            .unwrap();
        assert_ne!(first["pid"], after["pid"]);
        client.cancel().await.unwrap();
        server.waiting().await.unwrap();
        gateway.close().await;
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shutdown_stops_the_process_and_its_descendants() {
        let gateway = Gateway::new(definition(3), CancellationToken::new());
        let result = echo(&gateway, json!({"child":true})).await.unwrap();
        gateway.close().await;
        for pid in [
            result["pid"].as_u64().unwrap(),
            result["child_pid"].as_u64().unwrap(),
        ] {
            let mut alive = false;
            for _ in 0..30 {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
                alive = stat.is_ok_and(|s| !s.split_once(") ").unwrap().1.starts_with('Z'));
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(!alive, "a child process survived shutdown");
        }
    }
    #[tokio::test]
    async fn handshake_timeout_is_bounded_and_missing_commands_are_sanitized() {
        let mut spec = definition(1);
        spec.args.truncate(1);
        spec.args.push("--no-handshake".into());
        let gateway = Gateway::new(spec, CancellationToken::new());
        let result = tokio::time::timeout(Duration::from_secs(4), gateway.peer())
            .await
            .unwrap();
        assert!(result.is_err());
        gateway.close().await;
        let mut spec = definition(1);
        spec.command = "temba-nonexistent-private-command-753804".into();
        let gateway = Gateway::new(spec, CancellationToken::new());
        let error = gateway.peer().await.unwrap_err().to_string();
        assert!(
            !error.contains("private-token")
                && !error.contains("temba-nonexistent-private-command")
        );
    }
}
