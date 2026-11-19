use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cache::Cache;
use crate::server::Server;

pub struct Config {
    pub command: Vec<String>,
    pub idle: Duration,
    pub cache_dir: PathBuf,
    /// keep a separate cache per working folder
    pub per_dir: bool,
}

pub enum Event {
    Client(String),
    ClientClosed,
    Server(u64, String),
    ServerClosed(u64),
}

/// Lists answered from the cache while the server sleeps, each with the notification that marks it changed.
const LISTS: [(&str, &str); 4] = [
    ("tools/list", "notifications/tools/list_changed"),
    ("prompts/list", "notifications/prompts/list_changed"),
    ("resources/list", "notifications/resources/list_changed"),
    (
        "resources/templates/list",
        "notifications/resources/list_changed",
    ),
];

const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

pub fn run(config: Config) {
    let (events, inbox) = mpsc::channel();
    let client_events = events.clone();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if client_events.send(Event::Client(line)).is_err() {
                return;
            }
        }
        let _ = client_events.send(Event::ClientClosed);
    });
    Proxy::new(config, events).run(inbox);
}

enum Pending {
    Forward,
    Cache(String),
}

enum Internal {
    Initialize,
    /// re-reads a cached list after a start, holding the list and its change notification
    Refresh(&'static str, &'static str),
}

struct Proxy {
    command: Vec<String>,
    idle: Duration,
    cache: Cache,
    events: Sender<Event>,
    server: Option<Server>,
    /// bumped on every start and stop, so events from an old server are ignored
    generation: u64,
    /// false while a replayed initialize is in flight
    ready: bool,
    queue: VecDeque<String>,
    client_init: Option<Value>,
    pending: HashMap<String, Pending>,
    internal: HashMap<String, Internal>,
    server_requests: HashSet<String>,
    subscribed: bool,
    next_internal_id: u64,
    last_activity: Instant,
}

impl Proxy {
    fn new(config: Config, events: Sender<Event>) -> Proxy {
        let working_dir = config.per_dir.then(env::current_dir).and_then(Result::ok);
        Proxy {
            cache: Cache::open(&config.cache_dir, &config.command, working_dir.as_deref()),
            command: config.command,
            idle: config.idle,
            events,
            server: None,
            generation: 0,
            ready: false,
            queue: VecDeque::new(),
            client_init: None,
            pending: HashMap::new(),
            internal: HashMap::new(),
            server_requests: HashSet::new(),
            subscribed: false,
            next_internal_id: 0,
            last_activity: Instant::now(),
        }
    }

    fn run(mut self, inbox: Receiver<Event>) {
        loop {
            let event = match self.idle_deadline() {
                Some(deadline) => {
                    match inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(event) => event,
                        Err(RecvTimeoutError::Timeout) => {
                            log("stopping idle server");
                            self.stop_server();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                None => match inbox.recv() {
                    Ok(event) => event,
                    Err(_) => break,
                },
            };
            match event {
                Event::Client(line) => self.on_client(line),
                Event::ClientClosed => break,
                Event::Server(generation, line) => {
                    if generation == self.generation {
                        self.on_server(line);
                    }
                }
                Event::ServerClosed(generation) => {
                    if generation == self.generation {
                        self.on_server_closed();
                    }
                }
            }
        }
        if let Some(server) = self.server.take() {
            let _ = server.stop().join();
        }
    }

    /// When the server may be stopped, or `None` while it is busy or not running.
    fn idle_deadline(&self) -> Option<Instant> {
        let busy = self.server.is_none()
            || self.idle.is_zero()
            || !self.ready
            || self.subscribed
            || !self.queue.is_empty()
            || !self.pending.is_empty()
            || !self.internal.is_empty()
            || !self.server_requests.is_empty();
        if busy {
            None
        } else {
            Some(self.last_activity + self.idle)
        }
    }

    fn on_client(&mut self, line: String) {
        self.last_activity = Instant::now();
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            return self.forward(line);
        };
        let method = message.get("method").and_then(Value::as_str);
        match (method, message.get("id")) {
            (Some(method), Some(id)) => self.on_client_request(method, id.clone(), &message, line),
            (Some(method), None) => self.on_client_notification(method, line),
            (None, Some(id)) => {
                self.server_requests.remove(&id.to_string());
                self.forward_to_running(line);
            }
            (None, None) => {}
        }
    }

    fn on_client_request(&mut self, method: &str, id: Value, message: &Value, line: String) {
        match method {
            "initialize" => {
                self.client_init = message.get("params").cloned();
                let key = initialize_key(self.client_init.as_ref());
                if let Some(result) = self.cache.get(&key).cloned() {
                    return self.reply(&id, result);
                }
                self.pending.insert(id.to_string(), Pending::Cache(key));
                if self.server.is_none() {
                    // no cache yet: the client's own handshake starts the server
                    self.start_server(false);
                }
                self.forward_to_running(line);
            }
            "ping" => self.reply(&id, json!({})),
            _ => {
                if method == "resources/subscribe" {
                    self.subscribed = true;
                }
                let cacheable = LISTS.iter().any(|(list, _)| *list == method)
                    && message.pointer("/params/cursor").is_none();
                if cacheable && self.server.is_none() {
                    if let Some(result) = self.cache.get(method).cloned() {
                        return self.reply(&id, result);
                    }
                }
                let pending = if cacheable {
                    Pending::Cache(method.to_owned())
                } else {
                    Pending::Forward
                };
                self.pending.insert(id.to_string(), pending);
                self.forward(line);
            }
        }
    }

    fn on_client_notification(&mut self, method: &str, line: String) {
        // a sleeping server gets its own initialized notification after the replayed handshake
        if method == "notifications/initialized" && self.server.is_none() {
            return;
        }
        self.forward_to_running(line);
    }

    fn on_server(&mut self, line: String) {
        self.last_activity = Instant::now();
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            return self.write_client(&line);
        };
        let method = message.get("method").and_then(Value::as_str);
        match (method, message.get("id")) {
            (Some(_), Some(id)) => {
                self.server_requests.insert(id.to_string());
            }
            (None, Some(id)) => {
                let key = id.to_string();
                if let Some(kind) = self.internal.remove(&key) {
                    return self.on_internal_response(kind, &message);
                }
                if let Some(Pending::Cache(cache_key)) = self.pending.remove(&key) {
                    if let Some(result) = message.get("result") {
                        self.cache.set(&cache_key, result.clone());
                    }
                }
            }
            (Some(method), None) => self.forget_changed_lists(method),
            (None, None) => {}
        }
        self.write_client(&line);
    }

    fn forget_changed_lists(&mut self, method: &str) {
        for (list, notification) in LISTS {
            if notification == method {
                self.cache.remove(list);
            }
        }
    }

    fn on_internal_response(&mut self, kind: Internal, message: &Value) {
        let result = message.get("result");
        match (kind, result) {
            (Internal::Initialize, None) => {
                log("the server rejected the replayed handshake");
                self.stop_server();
                self.fail_all("mcp-nap: the server rejected the replayed handshake");
            }
            (Internal::Initialize, Some(result)) => {
                let key = initialize_key(self.client_init.as_ref());
                self.cache.set(&key, result.clone());
                self.send(INITIALIZED);
                self.ready = true;
                while let Some(line) = self.queue.pop_front() {
                    self.send(&line);
                }
                // lists the client got from the cache are checked against the running server
                for (list, notification) in LISTS {
                    if self.cache.get(list).is_some() {
                        let id = self.internal_request(Internal::Refresh(list, notification));
                        self.send(
                            &json!({ "jsonrpc": "2.0", "id": id, "method": list }).to_string(),
                        );
                    }
                }
            }
            (Internal::Refresh(list, notification), Some(result)) => {
                if self.cache.get(list) != Some(result) {
                    self.cache.set(list, result.clone());
                    self.write_client(
                        &json!({ "jsonrpc": "2.0", "method": notification }).to_string(),
                    );
                }
            }
            (Internal::Refresh(..), None) => {}
        }
    }

    fn on_server_closed(&mut self) {
        log("server exited");
        self.stop_server();
        self.fail_all("mcp-nap: the server exited");
    }

    fn start_server(&mut self, replay: bool) {
        self.generation += 1;
        match Server::spawn(&self.command, self.generation, self.events.clone()) {
            Ok(server) => {
                log("starting server");
                self.server = Some(server);
                self.ready = true;
                if let (true, Some(params)) = (replay, self.client_init.clone()) {
                    self.ready = false;
                    let id = self.internal_request(Internal::Initialize);
                    self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": "initialize", "params": params }).to_string());
                }
            }
            Err(error) => {
                log(&format!("cannot start {}: {error}", self.command[0]));
                self.fail_all(&format!("mcp-nap: cannot start the server: {error}"));
            }
        }
    }

    fn stop_server(&mut self) {
        self.generation += 1;
        self.ready = false;
        self.subscribed = false;
        self.internal.clear();
        self.server_requests.clear();
        if let Some(server) = self.server.take() {
            // the reaper thread finishes on its own
            drop(server.stop());
        }
    }

    /// Answers every request still waiting on the server with an error.
    fn fail_all(&mut self, reason: &str) {
        self.queue.clear();
        let ids: Vec<String> = self.pending.drain().map(|(id, _)| id).collect();
        for id in ids {
            let id: Value = serde_json::from_str(&id).unwrap_or(Value::Null);
            let error = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": reason } });
            self.write_client(&error.to_string());
        }
    }

    /// Sends a line to the server, starting it first if it sleeps.
    fn forward(&mut self, line: String) {
        if self.server.is_none() {
            self.start_server(true);
        }
        self.forward_to_running(line);
    }

    fn forward_to_running(&mut self, line: String) {
        if self.server.is_none() {
            return;
        }
        if self.ready {
            self.send(&line);
        } else {
            self.queue.push_back(line);
        }
    }

    fn send(&mut self, line: &str) {
        let Some(server) = self.server.as_mut() else {
            return;
        };
        if server.send(line).is_err() {
            self.on_server_closed();
        }
    }

    fn internal_request(&mut self, kind: Internal) -> Value {
        self.next_internal_id += 1;
        let id = Value::String(format!("mcp-nap-{}", self.next_internal_id));
        self.internal.insert(id.to_string(), kind);
        id
    }

    fn reply(&mut self, id: &Value, result: Value) {
        self.write_client(&json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string());
    }

    fn write_client(&mut self, line: &str) {
        let mut stdout = io::stdout().lock();
        // a failed write means the client is gone, and its closed stdin ends the loop
        let _ = writeln!(stdout, "{line}").and_then(|_| stdout.flush());
    }
}

fn initialize_key(params: Option<&Value>) -> String {
    let version = params
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    format!("initialize {version}")
}

fn log(message: &str) {
    eprintln!("mcp-nap: {message}");
}
