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
use tracing::{debug, error, info};

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
            debug!("idle for {:?}", self.idle);
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
                    debug!(%key, "answered from the cache");
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
                if let (false, Some(key)) = (self.running, &key)
                    && let Some(result) = self.cache.get(key, SystemTime::now()).cloned()
                {
                    debug!(%key, "answered from the cache");
                    return self.reply(id, result);
                }
                if matches!(request, ClientRequest::SubscribeRequest(_)) {
                    self.subscribed = true;
                }
                debug!(method = request.method(), "forwarded to the server");
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
                    info!(%key, "the server lists something new, telling the client");
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
            error!("the server rejected the replayed initialize");
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
            debug!(task = %created.task.task_id, "a task keeps the server awake");
            self.tasks.insert(created.task.task_id, expiry);
        } else if let Ok(current) = serde_json::from_value::<GetTaskResult>(result.clone())
            && current.task.status().is_terminal()
        {
            self.tasks.remove(&current.task.task.task_id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    const LEGACY: &str = "2025-11-25";
    const MODERN: &str = "2026-07-28";
    const IDLE: Duration = Duration::from_secs(60);

    struct Harness {
        session: Session,
        now: Instant,
        _dir: TempDir,
    }

    impl Harness {
        fn new() -> Harness {
            Harness::with_cache(tempfile::tempdir().unwrap())
        }

        fn with_cache(dir: TempDir) -> Harness {
            let cache = Cache::open(dir.path(), &["server".to_owned()], None);
            let now = Instant::now();
            Harness {
                session: Session::new(cache, IDLE, now),
                now,
                _dir: dir,
            }
        }

        fn client(&mut self, message: Value) -> Vec<Action> {
            self.session.on_client(&message.to_string(), self.now)
        }

        fn server(&mut self, message: Value) -> Vec<Action> {
            self.session.on_server(&message.to_string(), self.now)
        }

        /// Runs a cold legacy session to fill the cache with `tools`, then a fresh session on the same cache.
        fn warm_legacy(tools: Value) -> Harness {
            let mut cold = Harness::new();
            cold.client(initialize(1));
            cold.server(result(1, initialize_result()));
            cold.client(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
            cold.server(result(2, tools));
            Harness::with_cache(cold._dir)
        }
    }

    fn initialize(id: u64) -> Value {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": { "protocolVersion": LEGACY, "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }
        })
    }

    fn initialize_result() -> Value {
        json!({ "protocolVersion": LEGACY, "capabilities": { "tools": {} }, "serverInfo": { "name": "server", "version": "1" } })
    }

    fn modern(id: u64, method: &str) -> Value {
        json!({
            "jsonrpc": "2.0", "id": id, "method": method,
            "params": { "_meta": {
                "io.modelcontextprotocol/protocolVersion": MODERN,
                "io.modelcontextprotocol/clientCapabilities": {}
            } }
        })
    }

    fn call(id: u64) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": "echo" } })
    }

    fn result(id: impl Into<Value>, result: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id.into(), "result": result })
    }

    fn tools(names: &[&str]) -> Value {
        let tools: Vec<Value> = names
            .iter()
            .map(|name| json!({ "name": name, "inputSchema": { "type": "object" } }))
            .collect();
        json!({ "tools": tools })
    }

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    /// The JSON sent to the server, in order.
    fn to_server(actions: &[Action]) -> Vec<Value> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::ToServer(line) => Some(parse(line)),
                _ => None,
            })
            .collect()
    }

    /// The JSON sent to the client, in order.
    fn to_client(actions: &[Action]) -> Vec<Value> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::ToClient(line) => Some(parse(line)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_cold_legacy_handshake_starts_the_server_and_passes_through() {
        let mut harness = Harness::new();

        let actions = harness.client(initialize(1));
        assert_eq!(actions[0], Action::Start);
        assert_eq!(to_server(&actions), [initialize(1)]);

        let actions = harness.server(result(1, initialize_result()));
        assert_eq!(to_client(&actions), [result(1, initialize_result())]);
    }

    #[test]
    fn a_warm_legacy_session_answers_from_the_cache_until_a_tool_call() {
        let mut harness = Harness::warm_legacy(tools(&["echo"]));

        assert_eq!(
            to_client(&harness.client(initialize(1))),
            [result(1, initialize_result())]
        );
        assert!(
            harness
                .client(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .is_empty()
        );
        assert_eq!(
            to_client(
                &harness.client(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            ),
            [result(2, tools(&["echo"]))]
        );
        assert_eq!(
            to_client(&harness.client(json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }))),
            [result(3, json!({}))]
        );

        let actions = harness.client(call(4));
        assert_eq!(actions[0], Action::Start);
        let sent = to_server(&actions);
        assert_eq!(sent.len(), 1, "the call waits for the replayed handshake");
        assert_eq!(sent[0]["method"], "initialize");
        assert_eq!(sent[0]["params"]["clientInfo"]["name"], "test");

        let actions = harness.server(result(sent[0]["id"].clone(), initialize_result()));
        let sent = to_server(&actions);
        assert_eq!(sent[0]["method"], "notifications/initialized");
        assert_eq!(sent[1], call(4));
        assert_eq!(sent[2]["method"], "tools/list");
        assert!(
            to_client(&actions).is_empty(),
            "the replayed handshake stays internal"
        );
    }

    #[test]
    fn a_changed_list_after_a_start_is_cached_and_announced() {
        let mut harness = Harness::warm_legacy(tools(&["echo"]));
        harness.client(initialize(1));
        let replay = to_server(&harness.client(call(2)));
        let refresh =
            to_server(&harness.server(result(replay[0]["id"].clone(), initialize_result())));

        let actions = harness.server(result(refresh[2]["id"].clone(), tools(&["echo", "extra"])));
        assert_eq!(
            to_client(&actions),
            [json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" })]
        );

        let unchanged = harness.server(result(refresh[2]["id"].clone(), tools(&["echo", "extra"])));
        assert!(
            to_client(&unchanged)
                .iter()
                .all(|message| message["method"].is_null())
        );
    }

    #[test]
    fn a_log_level_set_while_asleep_is_answered_and_replayed_on_start() {
        let mut harness = Harness::warm_legacy(tools(&["echo"]));
        harness.client(initialize(1));
        let set_level = json!({ "jsonrpc": "2.0", "id": 2, "method": "logging/setLevel", "params": { "level": "debug" } });
        assert_eq!(
            to_client(&harness.client(set_level)),
            [result(2, json!({}))]
        );

        let replay = to_server(&harness.client(call(3)));
        let sent = to_server(&harness.server(result(replay[0]["id"].clone(), initialize_result())));
        assert_eq!(sent[1]["method"], "logging/setLevel");
        assert_eq!(sent[1]["params"]["level"], "debug");
        assert_eq!(sent[2], call(3));
    }

    #[test]
    fn modern_results_follow_their_ttl_and_scope() {
        let mut harness = Harness::new();
        let fresh = json!({ "tools": [], "ttlMs": 60_000, "cacheScope": "public" });
        let stale = json!({ "prompts": [], "ttlMs": 0, "cacheScope": "public" });

        assert_eq!(harness.client(modern(1, "tools/list"))[0], Action::Start);
        harness.server(result(1, fresh.clone()));
        harness.client(modern(2, "prompts/list"));
        harness.server(result(2, stale));
        harness.session.on_server_exit("stopped for the test");

        assert_eq!(
            to_client(&harness.client(modern(3, "tools/list"))),
            [result(3, fresh)]
        );
        assert_eq!(harness.client(modern(4, "prompts/list"))[0], Action::Start);
    }

    #[test]
    fn a_modern_start_needs_no_handshake() {
        let mut harness = Harness::new();
        let actions = harness.client(modern(1, "tools/call"));
        assert_eq!(actions[0], Action::Start);
        assert_eq!(to_server(&actions), [modern(1, "tools/call")]);
    }

    #[test]
    fn discover_results_are_cached_by_their_hints() {
        let mut harness = Harness::new();
        let discover = json!({
            "resultType": "complete", "supportedVersions": [MODERN], "capabilities": {},
            "ttlMs": 60_000, "cacheScope": "public"
        });
        harness.client(modern(1, "server/discover"));
        harness.server(result(1, discover.clone()));
        harness.session.on_server_exit("stopped for the test");

        assert_eq!(
            to_client(&harness.client(modern(2, "server/discover"))),
            [result(2, discover)]
        );
    }

    #[test]
    fn the_server_stops_after_the_idle_time_with_nothing_in_flight() {
        let mut harness = Harness::new();
        harness.client(modern(1, "tools/call"));
        assert_eq!(
            harness.session.idle_deadline(),
            None,
            "a request is in flight"
        );

        harness.server(result(1, json!({ "content": [] })));
        let deadline = harness.session.idle_deadline().unwrap();
        assert_eq!(deadline, harness.now + IDLE);
        assert!(
            harness
                .session
                .on_tick(deadline - Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(harness.session.on_tick(deadline), [Action::Stop]);
    }

    #[test]
    fn a_running_task_keeps_the_server_awake_until_it_ends() {
        let mut harness = Harness::new();
        harness.client(modern(1, "tools/call"));
        let task = json!({
            "resultType": "task", "taskId": "task-1", "status": "working",
            "createdAt": "2026-11-19T00:00:00Z", "lastUpdatedAt": "2026-11-19T00:00:00Z", "ttlMs": null
        });
        harness.server(result(1, task));
        assert_eq!(harness.session.idle_deadline(), None, "the task has no end");

        harness.server(json!({
            "jsonrpc": "2.0", "method": "notifications/tasks",
            "params": {
                "taskId": "task-1", "status": "completed", "result": { "content": [] },
                "createdAt": "2026-11-19T00:00:00Z", "lastUpdatedAt": "2026-11-19T00:01:00Z", "ttlMs": null
            }
        }));
        assert!(harness.session.idle_deadline().is_some());
    }

    #[test]
    fn a_server_exit_fails_the_requests_in_flight() {
        let mut harness = Harness::new();
        harness.client(modern(1, "tools/call"));
        let actions = harness.session.on_server_exit("mcp-nap: the server exited");
        assert_eq!(actions[0], Action::Stop);
        let failed = to_client(&actions);
        assert_eq!(failed[0]["id"], 1);
        assert_eq!(failed[0]["error"]["message"], "mcp-nap: the server exited");
    }

    #[test]
    fn a_cancelled_request_no_longer_keeps_the_server_awake() {
        let mut harness = Harness::new();
        harness.client(modern(1, "subscriptions/listen"));
        assert_eq!(harness.session.idle_deadline(), None);
        let actions = harness.client(json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1 }
        }));
        assert_eq!(to_server(&actions)[0]["method"], "notifications/cancelled");
        assert!(harness.session.idle_deadline().is_some());
    }
}
