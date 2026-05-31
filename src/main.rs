mod keys;
mod manager;
mod session;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::*,
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use crate::keys::process_special_keys;
use crate::manager::SessionManager;
use crate::session::PtySession;

// ── Helpers ────────────────────────────────────────────────────────

fn tool_error(msg: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg.to_string())])
}

fn tool_ok_text(s: String) -> CallToolResult {
    CallToolResult::success(vec![Content::text(s)])
}

fn tool_ok_json(value: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(value.to_string())])
}

// ── Parameter structs ──────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LaunchParams {
    /// Command to run. Defaults to $SHELL or /bin/sh.
    #[serde(default)]
    command: Option<String>,
    /// Command arguments.
    #[serde(default)]
    args: Option<Vec<String>>,
    /// Working directory for the new process.
    #[serde(default)]
    cwd: Option<String>,
    /// Terminal width. Default 80.
    #[serde(default)]
    cols: Option<u16>,
    /// Terminal height. Default 24.
    #[serde(default)]
    rows: Option<u16>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionIdParams {
    /// Session id returned from pty_launch.
    session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendKeysParams {
    session_id: String,
    /// Keys to send. Supports special sequences: [ENTER], [UP], [CTRL+C], ^C, \x1b, etc.
    keys: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ResizeParams {
    session_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ScrollbackParams {
    session_id: String,
    /// Max lines to return. Default 100.
    #[serde(default)]
    lines: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WaitForParams {
    session_id: String,
    /// Regex pattern matched against the screen contents.
    pattern: String,
    /// Timeout in milliseconds. Default 30000.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WaitForIdleParams {
    session_id: String,
    /// Seconds of output silence to consider idle. Default 2.0.
    #[serde(default)]
    idle_seconds: Option<f64>,
    /// Max seconds to wait before returning timeout. Default 60.
    #[serde(default)]
    timeout: Option<f64>,
}

// ── Server ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct PtyServer {
    manager: Arc<Mutex<SessionManager>>,
    tool_router: ToolRouter<Self>,
}

impl PtyServer {
    fn new() -> Self {
        Self {
            manager: Arc::new(Mutex::new(SessionManager::new())),
            tool_router: Self::tool_router(),
        }
    }
}

// ── Tool implementations ───────────────────────────────────────────

#[tool_router]
impl PtyServer {
    #[tool(
        description = "Launch a new terminal (PTY) session running a command. Returns a session_id used by all other pty_* tools. The session persists until pty_kill is called or the child process exits."
    )]
    async fn pty_launch(
        &self,
        Parameters(params): Parameters<LaunchParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let command = params.command.unwrap_or_else(|| {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
        });
        let args = params.args.unwrap_or_default();
        let cols = params.cols.unwrap_or(80);
        let rows = params.rows.unwrap_or(24);
        let cwd = params.cwd.as_ref().map(PathBuf::from);

        let mut mgr = self.manager.lock().await;
        let id = mgr.generate_id();
        let session = match PtySession::spawn(
            id.clone(),
            &command,
            &args,
            cwd.as_deref(),
            cols,
            rows,
        ) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error(format!("launch failed: {e}"))),
        };
        let id = match mgr.insert(session) {
            Ok(id) => id,
            Err(e) => return Ok(tool_error(format!("insert failed: {e}"))),
        };

        Ok(tool_ok_json(serde_json::json!({
            "session_id": id,
            "command": command,
            "cols": cols,
            "rows": rows,
        })))
    }

    #[tool(description = "List all active PTY sessions with their id, command, status, and dimensions.")]
    async fn pty_list(&self) -> std::result::Result<CallToolResult, McpError> {
        let mgr = self.manager.lock().await;
        let sessions = mgr.list();
        Ok(tool_ok_json(serde_json::json!({ "sessions": sessions })))
    }

    #[tool(
        description = "Send keystrokes to a PTY session. Keys are sent through special-key processing: [ENTER] becomes \\r, [UP]/[DOWN]/[LEFT]/[RIGHT] become arrow escapes, ^C / [CTRL+C] becomes 0x03, \\xNN is a hex byte, etc. To submit a command, append [ENTER]."
    )]
    async fn pty_send_keys(
        &self,
        Parameters(params): Parameters<SendKeysParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let data = process_special_keys(&params.keys);
        let mut mgr = self.manager.lock().await;
        let session = match mgr.get_mut(&params.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error(e)),
        };
        if let Err(e) = session.write_bytes(&data) {
            return Ok(tool_error(format!("send_keys failed: {e}")));
        }
        Ok(tool_ok_json(serde_json::json!({
            "bytes_sent": data.len(),
        })))
    }

    #[tool(description = "Get the current visible screen contents (plain text) of a PTY session.")]
    async fn pty_get_screen(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mgr = self.manager.lock().await;
        let session = match mgr.get(&params.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error(e)),
        };
        Ok(tool_ok_text(session.get_screen()))
    }

    #[tool(description = "Get the cursor position (row, col) of a PTY session, 0-indexed.")]
    async fn pty_get_cursor(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mgr = self.manager.lock().await;
        let session = match mgr.get(&params.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error(e)),
        };
        let (row, col) = session.cursor_position();
        Ok(tool_ok_json(serde_json::json!({ "row": row, "col": col })))
    }

    #[tool(
        description = "Get scrollback buffer contents (text that scrolled off the visible screen). Returns up to `lines` rows in chronological order (oldest first)."
    )]
    async fn pty_get_scrollback(
        &self,
        Parameters(params): Parameters<ScrollbackParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let lines = params.lines.unwrap_or(100);
        let mgr = self.manager.lock().await;
        let session = match mgr.get(&params.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error(e)),
        };
        let scrollback = session.get_scrollback(lines);
        Ok(tool_ok_json(serde_json::json!({ "lines": scrollback })))
    }

    #[tool(description = "Resize a PTY session's terminal dimensions.")]
    async fn pty_resize(
        &self,
        Parameters(params): Parameters<ResizeParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mut mgr = self.manager.lock().await;
        let session = match mgr.get_mut(&params.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error(e)),
        };
        if let Err(e) = session.resize(params.cols, params.rows) {
            return Ok(tool_error(format!("resize failed: {e}")));
        }
        Ok(tool_ok_json(serde_json::json!({
            "cols": params.cols,
            "rows": params.rows,
        })))
    }

    #[tool(
        description = "Block until a regex pattern appears in the session's screen contents, or until timeout_ms elapses. Returns status 'matched' or 'timeout'."
    )]
    async fn pty_wait_for(
        &self,
        Parameters(params): Parameters<WaitForParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(30_000));
        let re = match regex::Regex::new(&params.pattern) {
            Ok(re) => re,
            Err(e) => return Ok(tool_error(format!("invalid regex: {e}"))),
        };

        let start = Instant::now();
        let poll = Duration::from_millis(50);
        loop {
            // Snapshot screen under lock, release before sleeping.
            let screen = {
                let mgr = self.manager.lock().await;
                let session = match mgr.get(&params.session_id) {
                    Ok(s) => s,
                    Err(e) => return Ok(tool_error(e)),
                };
                session.snapshot_screen()
            };
            if re.is_match(&screen) {
                return Ok(tool_ok_json(serde_json::json!({
                    "status": "matched",
                    "elapsed_ms": start.elapsed().as_millis() as u64,
                })));
            }
            if start.elapsed() >= timeout {
                return Ok(tool_ok_json(serde_json::json!({
                    "status": "timeout",
                    "elapsed_ms": start.elapsed().as_millis() as u64,
                })));
            }
            tokio::time::sleep(poll).await;
        }
    }

    #[tool(
        description = "Block until the session has produced no output for `idle_seconds` (default 2), or `timeout` seconds elapse (default 60). Returns status 'idle' or 'timeout'."
    )]
    async fn pty_wait_for_idle(
        &self,
        Parameters(params): Parameters<WaitForIdleParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let idle_target = params.idle_seconds.unwrap_or(2.0);
        let timeout = Duration::from_secs_f64(params.timeout.unwrap_or(60.0));
        let start = Instant::now();
        let poll = Duration::from_millis(100);

        loop {
            let idle = {
                let mgr = self.manager.lock().await;
                let session = match mgr.get(&params.session_id) {
                    Ok(s) => s,
                    Err(e) => return Ok(tool_error(e)),
                };
                session.idle_seconds()
            };
            if idle >= idle_target {
                return Ok(tool_ok_json(serde_json::json!({
                    "status": "idle",
                    "idle_seconds": idle,
                    "elapsed_ms": start.elapsed().as_millis() as u64,
                })));
            }
            if start.elapsed() >= timeout {
                return Ok(tool_ok_json(serde_json::json!({
                    "status": "timeout",
                    "idle_seconds": idle,
                    "elapsed_ms": start.elapsed().as_millis() as u64,
                })));
            }
            tokio::time::sleep(poll).await;
        }
    }

    #[tool(description = "Kill a PTY session and remove it from the manager.")]
    async fn pty_kill(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mut mgr = self.manager.lock().await;
        let mut session = match mgr.remove(&params.session_id) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error(e)),
        };
        let _ = session.kill();
        Ok(tool_ok_json(serde_json::json!({
            "session_id": params.session_id,
            "killed": true,
        })))
    }
}

#[tool_handler]
impl ServerHandler for PtyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "Stateful PTY MCP server. Spawn shells or interactive programs with \
                 pty_launch, drive them with pty_send_keys, observe with pty_get_screen / \
                 pty_get_cursor / pty_get_scrollback, synchronize with pty_wait_for / \
                 pty_wait_for_idle, and clean up with pty_kill."
                    .to_string(),
            ),
        }
    }
}

// ── Main ───────────────────────────────────────────────────────────

#[cfg(not(tarpaulin_include))]
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Starting mcp-pty-server");

    let server = PtyServer::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_text(result: &CallToolResult) -> String {
        match &result.content[0].raw {
            RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn launch_and_list() {
        let server = PtyServer::new();
        let res = server
            .pty_launch(Parameters(LaunchParams {
                command: Some("/bin/sh".into()),
                args: Some(vec!["-c".into(), "sleep 2".into()]),
                cwd: None,
                cols: Some(80),
                rows: Some(24),
            }))
            .await
            .unwrap();
        assert!(!res.is_error.unwrap_or(false));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&res)).unwrap();
        let id = body["session_id"].as_str().unwrap().to_string();

        let res = server.pty_list().await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&extract_text(&res)).unwrap();
        assert_eq!(body["sessions"].as_array().unwrap().len(), 1);

        let _ = server
            .pty_kill(Parameters(SessionIdParams { session_id: id }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn send_keys_and_wait_for() {
        let server = PtyServer::new();
        let res = server
            .pty_launch(Parameters(LaunchParams {
                command: Some("/bin/sh".into()),
                args: Some(vec!["-c".into(), "echo READY; sleep 5".into()]),
                cwd: None,
                cols: Some(80),
                rows: Some(24),
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&extract_text(&res)).unwrap();
        let id = body["session_id"].as_str().unwrap().to_string();

        let res = server
            .pty_wait_for(Parameters(WaitForParams {
                session_id: id.clone(),
                pattern: "READY".into(),
                timeout_ms: Some(3000),
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&extract_text(&res)).unwrap();
        assert_eq!(body["status"], "matched");

        let _ = server
            .pty_kill(Parameters(SessionIdParams { session_id: id }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn wait_for_idle_reports_idle() {
        let server = PtyServer::new();
        let res = server
            .pty_launch(Parameters(LaunchParams {
                command: Some("/bin/sh".into()),
                args: Some(vec!["-c".into(), "echo hi; sleep 5".into()]),
                cwd: None,
                cols: Some(80),
                rows: Some(24),
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&extract_text(&res)).unwrap();
        let id = body["session_id"].as_str().unwrap().to_string();

        let res = server
            .pty_wait_for_idle(Parameters(WaitForIdleParams {
                session_id: id.clone(),
                idle_seconds: Some(0.5),
                timeout: Some(5.0),
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&extract_text(&res)).unwrap();
        assert_eq!(body["status"], "idle");

        let _ = server
            .pty_kill(Parameters(SessionIdParams { session_id: id }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn get_cursor_and_screen() {
        let server = PtyServer::new();
        let res = server
            .pty_launch(Parameters(LaunchParams {
                command: Some("/bin/sh".into()),
                args: Some(vec!["-c".into(), "printf abc; sleep 5".into()]),
                cwd: None,
                cols: Some(80),
                rows: Some(24),
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&extract_text(&res)).unwrap();
        let id = body["session_id"].as_str().unwrap().to_string();

        // Give the child a moment to write.
        let _ = server
            .pty_wait_for(Parameters(WaitForParams {
                session_id: id.clone(),
                pattern: "abc".into(),
                timeout_ms: Some(2000),
            }))
            .await
            .unwrap();

        let res = server
            .pty_get_screen(Parameters(SessionIdParams {
                session_id: id.clone(),
            }))
            .await
            .unwrap();
        assert!(extract_text(&res).contains("abc"));

        let res = server
            .pty_get_cursor(Parameters(SessionIdParams {
                session_id: id.clone(),
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&extract_text(&res)).unwrap();
        assert_eq!(body["row"], 0);
        assert_eq!(body["col"], 3);

        let _ = server
            .pty_kill(Parameters(SessionIdParams { session_id: id }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn kill_removes_session() {
        let server = PtyServer::new();
        let res = server
            .pty_launch(Parameters(LaunchParams {
                command: Some("/bin/sh".into()),
                args: Some(vec!["-c".into(), "sleep 5".into()]),
                cwd: None,
                cols: None,
                rows: None,
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&extract_text(&res)).unwrap();
        let id = body["session_id"].as_str().unwrap().to_string();

        let _ = server
            .pty_kill(Parameters(SessionIdParams {
                session_id: id.clone(),
            }))
            .await
            .unwrap();

        // After kill, the session should be gone.
        let res = server
            .pty_get_screen(Parameters(SessionIdParams { session_id: id }))
            .await
            .unwrap();
        assert!(res.is_error.unwrap_or(false));
    }

    #[tokio::test]
    async fn get_screen_unknown_session_errors() {
        let server = PtyServer::new();
        let res = server
            .pty_get_screen(Parameters(SessionIdParams {
                session_id: "nope".into(),
            }))
            .await
            .unwrap();
        assert!(res.is_error.unwrap_or(false));
        assert!(extract_text(&res).contains("not found"));
    }

    #[tokio::test]
    async fn server_info_has_tools() {
        let server = PtyServer::new();
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        assert!(info.instructions.unwrap().contains("PTY"));
    }
}
