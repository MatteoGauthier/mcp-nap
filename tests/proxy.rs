//! mcp-nap between the official rmcp client and an rmcp server, on both MCP protocol versions.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ProtocolVersion};
use rmcp::service::{ClientInitializeError, NotificationContext, RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt, RoleClient};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy)]
enum Protocol {
    /// the `initialize` handshake of 2025-11-25 and earlier
    Legacy,
    /// `server/discover` and per-request metadata of 2026-07-28
    Modern,
}

fn fake_server() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        escargot::CargoBuild::new()
            .example("fake_server")
            .current_release()
            .current_target()
            .run()
            .expect("the fake server builds")
            .path()
            .to_owned()
    })
}

/// Records the list changes mcp-nap announces to the client.
struct Recorder {
    changes: mpsc::UnboundedSender<&'static str>,
}

impl ClientHandler for Recorder {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let _ = self.changes.send("tools");
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let _ = self.changes.send("prompts");
    }
}

/// A cache folder and a server log for one test.
struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    fn new() -> Sandbox {
        Sandbox {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    async fn connect(&self, protocol: Protocol, options: &[&str], env: &[(&str, &str)]) -> Client {
        self.try_connect(protocol, options, env, self.dir.path(), fake_server())
            .await
            .expect("the client connects")
    }

    async fn try_connect(
        &self,
        protocol: Protocol,
        options: &[&str],
        env: &[(&str, &str)],
        cwd: &Path,
        server: &Path,
    ) -> Result<Client, ClientInitializeError> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mcp-nap"));
        command
            .arg("--cache-dir")
            .arg(self.dir.path().join("cache"))
            .args(options)
            .arg(server)
            .current_dir(cwd)
            .env("FAKE_SERVER_LOG", self.dir.path().join("server.log"))
            .envs(env.iter().copied());
        let (transport, _) = TokioChildProcess::builder(command)
            .stderr(Stdio::null())
            .spawn()
            .expect("mcp-nap starts");
        let mcp_nap = transport.id().expect("mcp-nap has a pid");
        let lifecycle = match protocol {
            Protocol::Legacy => ClientLifecycleMode::Initialize,
            Protocol::Modern => ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        };
        let (changes, changed) = mpsc::unbounded_channel();
        let service = timeout(
            TIMEOUT,
            Recorder { changes }.serve_with_lifecycle(transport, lifecycle),
        )
        .await
        .expect("the handshake finishes in time")?;
        Ok(Client {
            service,
            changed,
            mcp_nap,
        })
    }

    /// Runs one session so the next ones start with a full cache, then clears the log.
    async fn fill_cache(&self, protocol: Protocol, env: &[(&str, &str)]) {
        let client = self.connect(protocol, &[], env).await;
        client.tool_names().await;
        client.prompt_names().await;
        client.close().await;
        let _ = fs::remove_file(self.dir.path().join("server.log"));
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("server.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn started_pids(&self) -> Vec<u32> {
        self.log()
            .iter()
            .filter_map(|entry| entry.strip_prefix("start "))
            .map(|pid| pid.parse().unwrap())
            .collect()
    }
}

struct Client {
    service: RunningService<RoleClient, Recorder>,
    changed: mpsc::UnboundedReceiver<&'static str>,
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "only the Unix signal test reads it")
    )]
    mcp_nap: u32,
}

impl Client {
    async fn tool_names(&self) -> Vec<String> {
        let tools = self.service.list_tools(None).await.expect("tools/list");
        tools
            .tools
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }

    async fn prompt_names(&self) -> Vec<String> {
        let prompts = self.service.list_prompts(None).await.expect("prompts/list");
        prompts
            .prompts
            .into_iter()
            .map(|prompt| prompt.name)
            .collect()
    }

    async fn call(&self, tool: &str) -> Result<String, ServiceError> {
        let result = self
            .service
            .call_tool(CallToolRequestParams::new(tool.to_owned()))
            .await?;
        Ok(result.content[0]
            .as_text()
            .expect("a text result")
            .text
            .clone())
    }

    async fn next_change(&mut self) -> &'static str {
        timeout(TIMEOUT, self.changed.recv())
            .await
            .expect("a list change in time")
            .expect("the client is open")
    }

    async fn close(self) {
        let _ = self.service.cancel().await;
    }
}

#[cfg(unix)]
fn is_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn is_running(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")))
}

async fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if condition() {
            return true;
        }
        sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn a_first_legacy_session_passes_through_and_fills_the_cache() {
    let sandbox = Sandbox::new();
    let client = sandbox.connect(Protocol::Legacy, &[], &[]).await;

    assert_eq!(client.tool_names().await, ["echo"]);
    assert_eq!(sandbox.started_pids().len(), 1);
    client.close().await;

    assert_eq!(
        fs::read_dir(sandbox.dir.path().join("cache"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn a_warm_legacy_session_starts_the_server_only_for_a_tool_call() {
    let sandbox = Sandbox::new();
    sandbox.fill_cache(Protocol::Legacy, &[]).await;
    let client = sandbox.connect(Protocol::Legacy, &[], &[]).await;

    assert_eq!(client.tool_names().await, ["echo"]);
    assert_eq!(client.prompt_names().await, ["greet"]);
    assert!(
        sandbox.log().is_empty(),
        "started too early: {:?}",
        sandbox.log()
    );

    assert_eq!(client.call("echo").await.unwrap(), "called echo");
    let pid = sandbox.started_pids()[0];
    client.close().await;
    assert!(
        wait_until(|| !is_running(pid)).await,
        "the server outlived mcp-nap"
    );
}

#[tokio::test]
async fn an_idle_server_is_stopped_and_started_again() {
    let sandbox = Sandbox::new();
    sandbox.fill_cache(Protocol::Legacy, &[]).await;
    let client = sandbox
        .connect(Protocol::Legacy, &["--idle", "1"], &[])
        .await;

    assert_eq!(client.call("echo").await.unwrap(), "called echo");
    let first = sandbox.started_pids()[0];
    assert!(
        wait_until(|| !is_running(first)).await,
        "the idle server was not stopped"
    );

    assert_eq!(client.call("echo").await.unwrap(), "called echo");
    assert_eq!(sandbox.started_pids().len(), 2);
    client.close().await;
}

#[tokio::test]
async fn changed_lists_are_announced_after_a_legacy_start() {
    let sandbox = Sandbox::new();
    sandbox.fill_cache(Protocol::Legacy, &[]).await;
    let changed = [
        ("FAKE_SERVER_TOOLS", "echo,extra"),
        ("FAKE_SERVER_PROMPTS", "greet,bye"),
    ];
    let mut client = sandbox.connect(Protocol::Legacy, &[], &changed).await;

    assert_eq!(client.tool_names().await, ["echo"], "the cached list");
    client.call("echo").await.unwrap();
    let mut changes = [client.next_change().await, client.next_change().await];
    changes.sort();
    assert_eq!(changes, ["prompts", "tools"]);
    assert_eq!(client.tool_names().await, ["echo", "extra"]);
    client.close().await;
}

#[tokio::test]
async fn a_modern_session_answers_from_the_cache_when_the_server_allows_it() {
    let sandbox = Sandbox::new();
    let cacheable = [("FAKE_SERVER_TTL_MS", "60000")];
    sandbox.fill_cache(Protocol::Modern, &cacheable).await;
    let client = sandbox.connect(Protocol::Modern, &[], &cacheable).await;

    assert_eq!(client.tool_names().await, ["echo"]);
    assert!(
        sandbox.log().is_empty(),
        "started too early: {:?}",
        sandbox.log()
    );

    assert_eq!(client.call("echo").await.unwrap(), "called echo");
    assert_eq!(
        sandbox.log()[1..],
        ["tools/call"],
        "no handshake before the call"
    );
    client.close().await;
}

#[tokio::test]
async fn a_modern_server_without_ttl_is_asked_every_time() {
    let sandbox = Sandbox::new();
    sandbox.fill_cache(Protocol::Modern, &[]).await;
    let client = sandbox.connect(Protocol::Modern, &[], &[]).await;

    assert_eq!(client.tool_names().await, ["echo"]);
    assert!(
        sandbox.log().contains(&"tools/list".to_owned()),
        "a ttlMs of 0 is never served from the cache"
    );
    client.close().await;
}

#[tokio::test]
async fn a_crashing_server_fails_the_call_and_restarts_on_the_next() {
    let sandbox = Sandbox::new();
    let tools = [("FAKE_SERVER_TOOLS", "echo,crash")];
    sandbox.fill_cache(Protocol::Legacy, &tools).await;
    let client = sandbox.connect(Protocol::Legacy, &[], &tools).await;

    assert!(client.call("crash").await.is_err());
    assert_eq!(client.call("echo").await.unwrap(), "called echo");
    assert_eq!(sandbox.started_pids().len(), 2);
    client.close().await;
}

#[tokio::test]
async fn a_missing_server_fails_the_handshake() {
    let sandbox = Sandbox::new();
    let missing = Path::new("/nonexistent/mcp-server");
    let result = sandbox
        .try_connect(Protocol::Legacy, &[], &[], sandbox.dir.path(), missing)
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn per_dir_keeps_one_cache_per_working_folder() {
    let sandbox = Sandbox::new();
    let (first, second) = (
        sandbox.dir.path().join("first"),
        sandbox.dir.path().join("second"),
    );
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let starts_after_handshake_in = async |dir: &Path| {
        let client = sandbox
            .try_connect(Protocol::Legacy, &["--per-dir"], &[], dir, fake_server())
            .await
            .unwrap();
        client.close().await;
        sandbox.started_pids().len()
    };

    assert_eq!(
        starts_after_handshake_in(&first).await,
        1,
        "the first folder fills its cache"
    );
    assert_eq!(
        starts_after_handshake_in(&first).await,
        1,
        "the first folder is cached"
    );
    assert_eq!(
        starts_after_handshake_in(&second).await,
        2,
        "the second folder has its own cache"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_terminate_signal_stops_the_server_with_mcp_nap() {
    let sandbox = Sandbox::new();
    sandbox.fill_cache(Protocol::Legacy, &[]).await;
    let client = sandbox.connect(Protocol::Legacy, &[], &[]).await;
    client.call("echo").await.unwrap();
    let server = sandbox.started_pids()[0];

    std::process::Command::new("kill")
        .arg(client.mcp_nap.to_string())
        .status()
        .unwrap();
    assert!(
        wait_until(|| !is_running(server)).await,
        "the server outlived a terminated mcp-nap"
    );
    client.close().await;
}
