//! The MCP side of mcp-nap, without any IO: client and server lines in, actions out.

use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;
use std::time::{Duration, Instant, SystemTime};

use rmcp::model::{
    ClientJsonRpcMessage, ClientNotification, ClientRequest, CreateTaskResult, CustomResult,
    ErrorData, GetMeta, GetTaskResult, RequestId, ServerJsonRpcMessage, ServerNotification,
    ServerResult,
};
use serde_json::{Value, json};

use crate::cache::{self, Cache};

/// What the IO side does next.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// start the server, which receives the `ToServer` lines that follow
    Start,
    /// stop the server and its whole process group
    Stop,
    ToServer(String),
    ToClient(String),
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

const DISCOVER: &str = "server/discover";

/// The protocol the client speaks, known from its first request.
enum Era {
    Unknown,
    /// before 2026-07-28: a server must see the client's `initialize` before anything else
    Legacy {
        version: String,
        initialize: Box<ClientRequest>,
        log_level: Option<Box<ClientRequest>>,
    },
    /// 2026-07-28 and later: every request carries its own protocol version
    Modern {
        version: String,
    },
}

/// Requests mcp-nap sends to the server on its own.
enum Internal {
    Initialize,
    SetLevel,
    Refresh {
        key: String,
        notification: &'static str,
    },
}

pub struct Session {
    cache: Cache,
    idle: Duration,
    era: Era,
    running: bool,
    /// false while a replayed `initialize` is in flight
    ready: bool,
    queue: VecDeque<String>,
    /// client requests the server has not answered yet, with the cache key of their result
    pending: HashMap<RequestId, Option<String>>,
    internal: HashMap<RequestId, Internal>,
    server_requests: HashSet<RequestId>,
    /// tasks the server is still working on, with the end of their `ttlMs`
    tasks: HashMap<String, Option<Instant>>,
    subscribed: bool,
    next_internal_id: u64,
    last_activity: Instant,
    actions: Vec<Action>,
}

impl Session {
    pub fn new(cache: Cache, idle: Duration, now: Instant) -> Session {
        Session {
            cache,
            idle,
            era: Era::Unknown,
            running: false,
            ready: false,
            queue: VecDeque::new(),
            pending: HashMap::new(),
            internal: HashMap::new(),
            server_requests: HashSet::new(),
            tasks: HashMap::new(),
            subscribed: false,
            next_internal_id: 0,
            last_activity: now,
            actions: Vec::new(),
        }
    }

    pub fn on_client(&mut self, line: &str, now: Instant) -> Vec<Action> {
        self.last_activity = now;
        match serde_json::from_str::<ClientJsonRpcMessage>(line) {
            Ok(ClientJsonRpcMessage::Request(request)) => {
                self.on_client_request(request.id, request.request, line)
            }
            Ok(ClientJsonRpcMessage::Notification(notification)) => {
                self.on_client_notification(notification.notification, line)
            }
            Ok(ClientJsonRpcMessage::Response(response)) => {
                self.server_requests.remove(&response.id);
                self.forward_to_running(line);
            }
            Ok(ClientJsonRpcMessage::Error(error)) => {
                if let Some(id) = &error.id {
                    self.server_requests.remove(id);
                }
                self.forward_to_running(line);
            }
            // not a message rmcp knows, so only the server can answer it
            Err(_) => self.forward(line),
        }
        mem::take(&mut self.actions)
    }

    pub fn on_server(&mut self, line: &str, now: Instant) -> Vec<Action> {
        self.last_activity = now;
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.send_client(line.to_owned());
            return mem::take(&mut self.actions);
        };
        match serde_json::from_value::<ServerJsonRpcMessage>(value.clone()) {
            Ok(ServerJsonRpcMessage::Request(request)) => {
                self.server_requests.insert(request.id);
            }
            Ok(ServerJsonRpcMessage::Notification(notification)) => {
                self.on_server_notification(&notification.notification);
            }
            Ok(ServerJsonRpcMessage::Response(response)) => {
                let result = &value["result"];
                if let Some(kind) = self.internal.remove(&response.id) {
                    self.on_internal_result(kind, result);
                    return mem::take(&mut self.actions);
                }
                self.track_task(result, now);
                if let Some(Some(key)) = self.pending.remove(&response.id) {
                    self.cache.put(&key, result.clone(), SystemTime::now());
                }
            }
            Ok(ServerJsonRpcMessage::Error(error)) => {
                if let Some(id) = &error.id {
                    if let Some(kind) = self.internal.remove(id) {
                        self.on_internal_error(kind);
                        return mem::take(&mut self.actions);
                    }
                    self.pending.remove(id);
                }
            }
            Err(_) => {}
        }
        self.send_client(line.to_owned());
        mem::take(&mut self.actions)
    }

    /// The server exited on its own: requests waiting on it fail.
    pub fn on_server_exit(&mut self, reason: &str) -> Vec<Action> {
        if self.running {
            self.stop();
            self.fail_pending(reason);
        }
        mem::take(&mut self.actions)
    }

    /// When the server may be stopped, or `None` while it is busy or not running.
    pub fn idle_deadline(&self) -> Option<Instant> {
        let busy = !self.running
            || self.idle.is_zero()
            || !self.ready
            || self.subscribed
            || !self.queue.is_empty()
            || !self.pending.is_empty()
            || !self.internal.is_empty()
            || !self.server_requests.is_empty();
        if busy {
            return None;
        }
        let mut deadline = self.last_activity + self.idle;
        for expiry in self.tasks.values() {
            deadline = deadline.max((*expiry)?);
        }
        Some(deadline)
    }

    /// Stops the server once its idle deadline has passed.
    pub fn on_tick(&mut self, now: Instant) -> Vec<Action> {
        self.tasks
            .retain(|_, expiry| expiry.is_none_or(|at| at > now));
        if self.idle_deadline().is_some_and(|deadline| deadline <= now) {
            self.stop();
        }
        mem::take(&mut self.actions)
    }

    fn on_client_request(&mut self, id: RequestId, request: ClientRequest, line: &str) {
        match &request {
            ClientRequest::InitializeRequest(initialize) => {
                let version = initialize.params.protocol_version.to_string();
                let key = cache::key(request.method(), &version);
                self.era = Era::Legacy {
                    version,
                    initialize: Box::new(request.clone()),
                    log_level: None,
                };
                if let Some(result) = self.cache.get(&key, SystemTime::now()).cloned() {
                    return self.reply(id, result);
                }
                self.pending.insert(id, Some(key));
                if !self.running {
                    // nothing cached yet: the client's own handshake starts the server
                    self.start(false);
                }
                self.forward_to_running(line);
            }
            ClientRequest::PingRequest(_) => self.reply(id, json!({})),
            ClientRequest::SetLevelRequest(_) if !self.running => {
                // replayed after the handshake when the server starts
                if let Era::Legacy { log_level, .. } = &mut self.era {
                    *log_level = Some(Box::new(request.clone()));
                }
                self.reply(id, json!({}));
            }
            _ => {
                if let (Era::Unknown, Some(version)) =
                    (&self.era, request.get_meta().protocol_version())
                {
                    self.era = Era::Modern {
                        version: version.to_string(),
                    };
                }
                let key = self.cache_key(&request);
                if let (false, Some(key)) = (self.running, &key) {
                    if let Some(result) = self.cache.get(key, SystemTime::now()).cloned() {
                        return self.reply(id, result);
                    }
                }
                if matches!(request, ClientRequest::SubscribeRequest(_)) {
                    self.subscribed = true;
                }
                self.pending.insert(id, key);
                self.forward(line);
            }
        }
    }

    fn on_client_notification(&mut self, notification: ClientNotification, line: &str) {
        match notification {
            // a sleeping server gets its own initialized notification after the replayed handshake
            ClientNotification::InitializedNotification(_) if !self.running => {}
            ClientNotification::CancelledNotification(cancelled) => {
                let was_pending = cancelled
                    .params
                    .request_id
                    .as_ref()
                    .is_some_and(|id| self.pending.remove(id).is_some());
                if was_pending {
                    self.forward_to_running(line);
                }
            }
            _ => self.forward_to_running(line),
        }
    }

    fn on_server_notification(&mut self, notification: &ServerNotification) {
        let stale: &[&str] = match notification {
            ServerNotification::ToolListChangedNotification(_) => &["tools/list"],
            ServerNotification::PromptListChangedNotification(_) => &["prompts/list"],
            ServerNotification::ResourceListChangedNotification(_) => {
                &["resources/list", "resources/templates/list"]
            }
            ServerNotification::TaskStatusNotification(status) => {
                if status.params.status().is_terminal() {
                    self.tasks.remove(&status.params.task.task.task_id);
                }
                &[]
            }
            _ => &[],
        };
        for method in stale {
            self.cache.forget(method);
        }
    }

    fn on_internal_result(&mut self, kind: Internal, result: &Value) {
        match kind {
            Internal::Initialize => {
                if let Era::Legacy { version, .. } = &self.era {
                    let key = cache::key("initialize", version);
                    self.cache.put(&key, result.clone(), SystemTime::now());
                }
                self.send_server_message(
                    json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                );
                if let Era::Legacy {
                    log_level: Some(set_level),
                    ..
                } = &self.era
                {
                    let set_level = (**set_level).clone();
                    self.internal_request(Internal::SetLevel, set_level);
                }
                self.ready = true;
                while let Some(line) = self.queue.pop_front() {
                    self.send_server(line);
                }
                self.refresh_lists();
            }
            Internal::SetLevel => {}
            Internal::Refresh { key, notification } => {
                if self.cache.get(&key, SystemTime::now()) != Some(result) {
                    self.cache.put(&key, result.clone(), SystemTime::now());
                    self.send_client(
                        json!({ "jsonrpc": "2.0", "method": notification }).to_string(),
                    );
                }
            }
        }
    }

    fn on_internal_error(&mut self, kind: Internal) {
        if let Internal::Initialize = kind {
            eprintln!("mcp-nap: the server rejected the replayed initialize");
            self.stop();
            self.fail_pending("mcp-nap: the server rejected the replayed initialize");
        }
    }

    /// Checks the lists this client got from the cache against the server that just started.
    ///
    /// Only before 2026-07-28: newer clients follow `ttlMs` and hear of changes on `subscriptions/listen`.
    fn refresh_lists(&mut self) {
        let Era::Legacy { version, .. } = &self.era else {
            return;
        };
        let version = version.clone();
        for (list, notification) in LISTS {
            let key = cache::key(list, &version);
            if self.cache.get(&key, SystemTime::now()).is_none() {
                continue;
            }
            let Ok(request) = serde_json::from_value::<ClientRequest>(json!({ "method": list }))
            else {
                continue;
            };
            self.internal_request(Internal::Refresh { key, notification }, request);
        }
    }

    /// Keeps the server awake while it works on a task, until a terminal status or the task's `ttlMs`.
    fn track_task(&mut self, result: &Value, now: Instant) {
        if result.get("taskId").is_none() {
            return;
        }
        if let Ok(created) = serde_json::from_value::<CreateTaskResult>(result.clone()) {
            let expiry = created
                .task
                .ttl_ms
                .map(|ttl_ms| now + Duration::from_millis(ttl_ms));
            self.tasks.insert(created.task.task_id, expiry);
        } else if let Ok(current) = serde_json::from_value::<GetTaskResult>(result.clone()) {
            if current.task.status().is_terminal() {
                self.tasks.remove(&current.task.task.task_id);
            }
        }
    }

    /// The cache key of a discover request, or of the first page of a list.
    fn cache_key(&self, request: &ClientRequest) -> Option<String> {
        let cursor = match request {
            ClientRequest::DiscoverRequest(_) => return Some(DISCOVER.to_owned()),
            ClientRequest::ListToolsRequest(list) => list.params.as_ref().map(|p| &p.cursor),
            ClientRequest::ListPromptsRequest(list) => list.params.as_ref().map(|p| &p.cursor),
            ClientRequest::ListResourcesRequest(list) => list.params.as_ref().map(|p| &p.cursor),
            ClientRequest::ListResourceTemplatesRequest(list) => {
                list.params.as_ref().map(|p| &p.cursor)
            }
            _ => return None,
        };
        if cursor.is_some_and(Option::is_some) {
            return None;
        }
        let version = match &self.era {
            Era::Legacy { version, .. } | Era::Modern { version } => version.as_str(),
            Era::Unknown => "",
        };
        Some(cache::key(request.method(), version))
    }

    fn start(&mut self, replay: bool) {
        self.running = true;
        self.ready = true;
        self.actions.push(Action::Start);
        if let (true, Era::Legacy { initialize, .. }) = (replay, &self.era) {
            self.ready = false;
            let initialize = (**initialize).clone();
            self.internal_request(Internal::Initialize, initialize);
        }
    }

    fn stop(&mut self) {
        self.running = false;
        self.ready = false;
        self.subscribed = false;
        self.queue.clear();
        self.internal.clear();
        self.server_requests.clear();
        self.tasks.clear();
        self.actions.push(Action::Stop);
    }

    /// Answers every request still waiting on the server with an error.
    fn fail_pending(&mut self, reason: &str) {
        for (id, _) in mem::take(&mut self.pending) {
            let error = ServerJsonRpcMessage::error(
                ErrorData::internal_error(reason.to_owned(), None),
                Some(id),
            );
            self.send_client(serde_json::to_string(&error).expect("an error serializes"));
        }
    }

    /// Sends a line to the server, starting it first if it sleeps.
    fn forward(&mut self, line: &str) {
        if !self.running {
            self.start(true);
        }
        self.forward_to_running(line);
    }

    fn forward_to_running(&mut self, line: &str) {
        if !self.running {
            return;
        }
        if self.ready {
            self.send_server(line.to_owned());
        } else {
            self.queue.push_back(line.to_owned());
        }
    }

    fn internal_request(&mut self, kind: Internal, request: ClientRequest) {
        self.next_internal_id += 1;
        let id = RequestId::String(format!("mcp-nap-{}", self.next_internal_id).into());
        self.internal.insert(id.clone(), kind);
        self.send_server_message(ClientJsonRpcMessage::request(request, id));
    }

    fn reply(&mut self, id: RequestId, result: Value) {
        let response =
            ServerJsonRpcMessage::response(ServerResult::CustomResult(CustomResult(result)), id);
        self.send_client(serde_json::to_string(&response).expect("a response serializes"));
    }

    fn send_server_message(&mut self, message: impl serde::Serialize) {
        self.send_server(serde_json::to_string(&message).expect("a message serializes"));
    }

    fn send_server(&mut self, line: String) {
        self.actions.push(Action::ToServer(line));
    }

    fn send_client(&mut self, line: String) {
        self.actions.push(Action::ToClient(line));
    }
}
