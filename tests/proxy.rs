use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A temporary cache directory and server log for one test.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("mcp-nap-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Sandbox { dir }
    }

    fn session(&self, idle: u64, tools: &str) -> Session {
        self.launch(
            &["--idle", &idle.to_string()],
            &fake_server(),
            &[("FAKE_SERVER_TOOLS", tools)],
            &self.dir,
        )
    }

    /// Starts mcp-nap with extra `options` in front of `server`, from the working folder `cwd`.
    fn launch(&self, options: &[&str], server: &Path, env: &[(&str, &str)], cwd: &Path) -> Session {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mcp-nap"))
            .arg("--cache-dir")
            .arg(self.dir.join("cache"))
            .args(options)
            .arg(server)
            .current_dir(cwd)
            .env("FAKE_SERVER_LOG", self.dir.join("server.log"))
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(serde_json::from_str(&line).unwrap()).is_err() {
                    break;
                }
            }
        });
        Session {
            stdin: child.stdin.take(),
            child,
            messages,
            notifications: Vec::new(),
        }
    }

    /// Runs one session so the next ones start with a full cache, then clears the log.
    fn fill_cache(&self, tools: &str) {
        let mut session = self.session(600, tools);
        session.handshake();
        session.request(2, "tools/list", json!({}));
        session.close();
        let _ = fs::remove_file(self.dir.join("server.log"));
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("server.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn started_pids(&self) -> Vec<i32> {
        self.log()
            .iter()
            .filter_map(|entry| entry.strip_prefix("start "))
            .map(|pid| pid.parse().unwrap())
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// One client connected to mcp-nap.
struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Value>,
    notifications: Vec<Value>,
}

impl Session {
    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let message = self.messages.recv_timeout(TIMEOUT).expect("no response");
            if message["id"] == id {
                return message;
            }
            self.notifications.push(message);
        }
    }

    fn handshake(&mut self) -> Value {
        let response = self.request(
            1,
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
        );
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        response
    }

    fn call(&mut self, id: u64, tool: &str) -> Value {
        self.request(id, "tools/call", json!({ "name": tool, "arguments": {} }))
    }

    fn wait_for_notification(&mut self, method: &str) -> Value {
        if let Some(index) = self
            .notifications
            .iter()
            .position(|message| message["method"] == method)
        {
            return self.notifications.remove(index);
        }
        loop {
            let message = self
                .messages
                .recv_timeout(TIMEOUT)
                .expect("no notification");
            if message["method"] == method {
                return message;
            }
        }
    }

    fn close(mut self) {
        drop(self.stdin.take());
        assert!(
            wait_until(|| self.child.try_wait().unwrap().is_some()),
            "mcp-nap did not exit"
        );
    }
}

fn fake_server() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_mcp-nap"))
        .parent()
        .unwrap()
        .join("examples/fake_server")
}

fn is_running(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn text(response: &Value) -> &str {
    response["result"]["content"][0]["text"].as_str().unwrap()
}

#[test]
fn first_session_passes_the_handshake_through_and_fills_the_cache() {
    let sandbox = Sandbox::new("first");
    let mut session = sandbox.session(600, "echo");

    assert_eq!(session.handshake()["result"]["serverInfo"]["name"], "fake");
    assert_eq!(
        session.request(2, "tools/list", json!({}))["result"]["tools"][0]["name"],
        "echo"
    );
    assert_eq!(sandbox.started_pids().len(), 1);
    session.close();

    assert_eq!(fs::read_dir(sandbox.dir.join("cache")).unwrap().count(), 1);
}

#[test]
fn cached_session_starts_the_server_only_for_a_tool_call() {
    let sandbox = Sandbox::new("cached");
    sandbox.fill_cache("echo");
    let mut session = sandbox.session(600, "echo");

    assert_eq!(session.handshake()["result"]["serverInfo"]["name"], "fake");
    assert_eq!(
        session.request(2, "tools/list", json!({}))["result"]["tools"][0]["name"],
        "echo"
    );
    assert_eq!(session.request(3, "ping", json!({}))["result"], json!({}));
    assert!(
        sandbox.log().is_empty(),
        "server started too early: {:?}",
        sandbox.log()
    );

    assert_eq!(text(&session.call(4, "echo")), "called echo");
    assert_eq!(
        sandbox.log()[1..4],
        ["initialize", "notifications/initialized", "tools/call"]
    );

    let pid = sandbox.started_pids()[0];
    session.close();
    assert!(wait_until(|| !is_running(pid)), "server outlived mcp-nap");
}

#[test]
fn idle_server_is_stopped_and_started_again() {
    let sandbox = Sandbox::new("idle");
    sandbox.fill_cache("echo");
    let mut session = sandbox.session(1, "echo");
    session.handshake();

    assert_eq!(text(&session.call(2, "echo")), "called echo");
    let first = sandbox.started_pids()[0];
    assert!(
        wait_until(|| !is_running(first)),
        "idle server was not stopped"
    );

    assert_eq!(text(&session.call(3, "echo")), "called echo");
    assert_eq!(sandbox.started_pids().len(), 2);
    session.close();
}

#[test]
fn changed_tools_are_announced_after_a_start() {
    let sandbox = Sandbox::new("changed");
    sandbox.fill_cache("echo");
    let mut session = sandbox.session(600, "echo,extra");
    session.handshake();

    assert_eq!(
        session.request(2, "tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    session.call(3, "echo");
    session.wait_for_notification("notifications/tools/list_changed");
    assert_eq!(
        session.request(4, "tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    session.close();
}

#[test]
fn changed_prompts_are_announced_after_a_start() {
    let sandbox = Sandbox::new("prompts");
    let open = |prompts: &str| {
        sandbox.launch(
            &[],
            &fake_server(),
            &[("FAKE_SERVER_PROMPTS", prompts)],
            &sandbox.dir,
        )
    };
    let prompt_count = |response: Value| response["result"]["prompts"].as_array().unwrap().len();

    let mut session = open("greet");
    session.handshake();
    session.request(2, "prompts/list", json!({}));
    session.close();

    let mut session = open("greet,bye");
    session.handshake();
    assert_eq!(
        prompt_count(session.request(2, "prompts/list", json!({}))),
        1
    );
    session.call(3, "echo");
    session.wait_for_notification("notifications/prompts/list_changed");
    assert_eq!(
        prompt_count(session.request(4, "prompts/list", json!({}))),
        2
    );
    session.close();
}

#[test]
fn a_crashing_server_fails_the_request_and_restarts_on_the_next_call() {
    let sandbox = Sandbox::new("crash");
    sandbox.fill_cache("echo,crash");
    let mut session = sandbox.session(600, "echo,crash");
    session.handshake();

    assert_eq!(session.call(2, "crash")["error"]["code"], -32603);
    assert_eq!(text(&session.call(3, "echo")), "called echo");
    assert_eq!(sandbox.started_pids().len(), 2);
    session.close();
}

#[test]
fn a_missing_server_fails_the_handshake() {
    let sandbox = Sandbox::new("missing");
    let mut session = sandbox.launch(&[], Path::new("/nonexistent/mcp-server"), &[], &sandbox.dir);

    assert_eq!(session.handshake()["error"]["code"], -32603);
    session.close();
}
