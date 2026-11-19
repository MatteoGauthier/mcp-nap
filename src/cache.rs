use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::model::CacheScope;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::warn;

/// Longest `ttlMs` honored, the same cap as the rmcp client cache.
const MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Handshake, discover and list results of one server, as the MCP cache hints allow.
///
/// Public and hint-less results are shared by every session through a JSON file.
/// Private results stay in this session, since another session may hold other credentials.
pub struct Cache {
    path: PathBuf,
    shared: Stored,
    private: HashMap<String, Entry>,
}

/// What a cache file holds.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    /// the server program, without its arguments, which may hold secrets
    program: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    working_dir: Option<PathBuf>,
    entries: HashMap<String, Entry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    result: Value,
    /// unix milliseconds after which the result is stale, `None` until the server lists something else
    expires_at_ms: Option<u64>,
}

impl Cache {
    /// Opens the cache of `command`, kept apart per `working_dir` when one is given.
    pub fn open(dir: &Path, command: &[String], working_dir: Option<&Path>) -> Cache {
        let path = dir.join(file_name(command, working_dir));
        let mut shared: Stored = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        shared.program = command[0].clone();
        shared.working_dir = working_dir.map(Path::to_path_buf);
        Cache {
            path,
            shared,
            private: HashMap::new(),
        }
    }

    /// The cached result for `key`, if it is still fresh.
    pub fn get(&self, key: &str, now: SystemTime) -> Option<&Value> {
        let now = unix_ms(now);
        self.private
            .get(key)
            .or_else(|| self.shared.entries.get(key))
            .filter(|entry| entry.expires_at_ms.is_none_or(|at| at > now))
            .map(|entry| &entry.result)
    }

    /// Stores a server result under `key`, following its `ttlMs` and `cacheScope` hints.
    pub fn put(&mut self, key: &str, result: Value, now: SystemTime) {
        let hints = Hints::of(&result);
        self.private.remove(key);
        let shared_before = self.shared.entries.remove(key);
        let entry = |ttl: Option<Duration>| Entry {
            expires_at_ms: ttl.map(|ttl| unix_ms(now) + ttl.as_millis() as u64),
            result,
        };
        match hints {
            Hints::None => {
                self.shared.entries.insert(key.to_owned(), entry(None));
            }
            Hints::Ttl { ttl, .. } if ttl.is_zero() => {}
            Hints::Ttl {
                ttl,
                scope: CacheScope::Private,
            } => {
                self.private.insert(key.to_owned(), entry(Some(ttl)));
            }
            Hints::Ttl { ttl, .. } => {
                self.shared.entries.insert(key.to_owned(), entry(Some(ttl)));
            }
        }
        if self.shared.entries.get(key) != shared_before.as_ref() {
            self.save();
        }
    }

    /// Drops every cached result of `method`, for all protocol versions.
    pub fn forget(&mut self, method: &str) {
        let matches = |key: &String| key == method || key.starts_with(&format!("{method}@"));
        self.private.retain(|key, _| !matches(key));
        let before = self.shared.entries.len();
        self.shared.entries.retain(|key, _| !matches(key));
        if self.shared.entries.len() != before {
            self.save();
        }
    }

    fn save(&self) {
        if let Err(error) = self.write() {
            warn!("cannot write {}: {error}", self.path.display());
        }
    }

    fn write(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        // write then rename, so a concurrent session never reads a half-written file
        let temporary = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temporary, serde_json::to_vec(&self.shared)?)?;
        fs::rename(&temporary, &self.path)
    }
}

/// Removes the cache of `command`, kept per `working_dir` when given, or every cache file in `dir` without one.
///
/// Returns how many files were removed. Files mcp-nap did not write are left alone.
pub fn clear(dir: &Path, command: Option<(&[String], Option<&Path>)>) -> io::Result<usize> {
    let mut removed = 0;
    for file in cache_files(dir, command)? {
        match fs::remove_file(&file) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

/// One cached server, as `--show-cache` lists it.
pub struct Summary {
    /// the server program, `None` for a file written by an older mcp-nap
    pub program: Option<String>,
    pub working_dir: Option<PathBuf>,
    /// name and version the server reported in its handshake or discover result
    pub server: Option<String>,
    pub entries: Vec<EntrySummary>,
}

pub struct EntrySummary {
    pub key: String,
    /// how many tools, prompts, resources or templates a list holds
    pub items: Option<usize>,
    pub freshness: Freshness,
}

pub enum Freshness {
    /// no `ttlMs`: kept until a running server lists something else
    UntilChanged,
    Fresh(Duration),
    Stale,
}

/// The cached servers in `dir`, or only the one of `command`, sorted by program.
pub fn summaries(
    dir: &Path,
    command: Option<(&[String], Option<&Path>)>,
    now: SystemTime,
) -> io::Result<Vec<Summary>> {
    let mut summaries: Vec<Summary> = cache_files(dir, command)?
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| fs::read(path).ok())
        .map(|bytes| summarize(&bytes, now))
        .collect();
    summaries.sort_by(|a, b| a.program.cmp(&b.program));
    Ok(summaries)
}

fn summarize(bytes: &[u8], now: SystemTime) -> Summary {
    let Ok(stored) = serde_json::from_slice::<Stored>(bytes) else {
        return Summary {
            program: None,
            working_dir: None,
            server: None,
            entries: Vec::new(),
        };
    };
    let now = unix_ms(now);
    let server = stored
        .entries
        .values()
        .find_map(|entry| server_info(&entry.result));
    let mut entries: Vec<EntrySummary> = stored
        .entries
        .into_iter()
        .map(|(key, entry)| EntrySummary {
            items: ["tools", "prompts", "resources", "resourceTemplates"]
                .iter()
                .find_map(|list| entry.result.get(list)?.as_array().map(Vec::len)),
            freshness: match entry.expires_at_ms {
                None => Freshness::UntilChanged,
                Some(at) if at > now => Freshness::Fresh(Duration::from_millis(at - now)),
                Some(_) => Freshness::Stale,
            },
            key,
        })
        .collect();
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    Summary {
        program: Some(stored.program),
        working_dir: stored.working_dir,
        server,
        entries,
    }
}

/// `name version` from an `initialize` result, or from the server info of a `server/discover` result.
fn server_info(result: &Value) -> Option<String> {
    let info = result
        .get("serverInfo")
        .or_else(|| result.pointer("/_meta/io.modelcontextprotocol~1serverInfo"))?;
    let name = info.get("name")?.as_str()?;
    Some(match info.get("version").and_then(Value::as_str) {
        Some(version) => format!("{name} {version}"),
        None => name.to_owned(),
    })
}

/// The cache file of `command`, or every file mcp-nap wrote in `dir` without one.
fn cache_files(
    dir: &Path,
    command: Option<(&[String], Option<&Path>)>,
) -> io::Result<Vec<PathBuf>> {
    if let Some((command, working_dir)) = command {
        return Ok(vec![dir.join(file_name(command, working_dir))]);
    }
    match fs::read_dir(dir) {
        Ok(entries) => Ok(entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_cache_file(path))
            .collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// A file mcp-nap wrote: a SHA-256 name ending in `.json`, or a temporary copy of one.
fn is_cache_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let digest = name.split('.').next().unwrap_or_default();
    digest.len() == 64
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        && (name.ends_with(".json") || name.ends_with(".tmp"))
}

/// The cache key of `method` for a client speaking `version`.
pub fn key(method: &str, version: &str) -> String {
    format!("{method}@{version}")
}

pub fn default_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(env::temp_dir)
        .join("mcp-nap")
}

/// Cache hints of a result, as defined for protocol version 2026-07-28.
enum Hints {
    /// No `ttlMs`: a server on an older protocol version, kept until a running server lists something else.
    None,
    Ttl {
        ttl: Duration,
        scope: CacheScope,
    },
}

impl Hints {
    fn of(result: &Value) -> Hints {
        let Some(ttl_ms) = result.get("ttlMs") else {
            return Hints::None;
        };
        // a negative or unreadable ttl is immediately stale, and a missing scope is private, as in rmcp
        let ttl = Duration::from_millis(ttl_ms.as_u64().unwrap_or(0)).min(MAX_TTL);
        let scope = result
            .get("cacheScope")
            .and_then(|scope| CacheScope::deserialize(scope).ok())
            .unwrap_or(CacheScope::Private);
        Hints::Ttl { ttl, scope }
    }
}

/// Hex SHA-256 of the length-prefixed command parts, then the working folder.
fn file_name(command: &[String], working_dir: Option<&Path>) -> String {
    let mut hasher = Sha256::new();
    for part in command {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    if let Some(dir) = working_dir {
        hasher.update(b"\0dir\0");
        hasher.update(dir.as_os_str().as_encoded_bytes());
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{digest}.json")
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn results_without_hints_stay_until_forgotten_and_are_shared() {
        let dir = tempfile::tempdir().unwrap();
        let server = command(&["server"]);
        let tools = json!({ "tools": [] });

        let mut cache = Cache::open(dir.path(), &server, None);
        cache.put("tools/list@2025-11-25", tools.clone(), at(0));

        let mut reopened = Cache::open(dir.path(), &server, None);
        assert_eq!(
            reopened.get("tools/list@2025-11-25", at(1_000_000)),
            Some(&tools)
        );
        reopened.forget("tools/list");
        assert_eq!(reopened.get("tools/list@2025-11-25", at(0)), None);
        assert_eq!(
            Cache::open(dir.path(), &server, None).get("tools/list@2025-11-25", at(0)),
            None
        );
    }

    #[test]
    fn public_results_expire_after_their_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(dir.path(), &command(&["server"]), None);
        let tools = json!({ "tools": [], "ttlMs": 60_000, "cacheScope": "public" });

        cache.put("tools/list@2026-07-28", tools.clone(), at(100));
        assert_eq!(cache.get("tools/list@2026-07-28", at(159)), Some(&tools));
        assert_eq!(cache.get("tools/list@2026-07-28", at(160)), None);
    }

    #[test]
    fn private_results_stay_in_the_session_and_zero_ttl_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let server = command(&["server"]);
        let mut cache = Cache::open(dir.path(), &server, None);
        let private = json!({ "tools": [], "ttlMs": 60_000, "cacheScope": "private" });
        let no_scope = json!({ "prompts": [], "ttlMs": 60_000 });
        let stale = json!({ "resources": [], "ttlMs": 0, "cacheScope": "public" });

        cache.put("tools/list@2026-07-28", private.clone(), at(0));
        cache.put("prompts/list@2026-07-28", no_scope.clone(), at(0));
        cache.put("resources/list@2026-07-28", stale, at(0));

        assert_eq!(cache.get("tools/list@2026-07-28", at(1)), Some(&private));
        assert_eq!(cache.get("prompts/list@2026-07-28", at(1)), Some(&no_scope));
        assert_eq!(cache.get("resources/list@2026-07-28", at(0)), None);
        let reopened = Cache::open(dir.path(), &server, None);
        assert_eq!(reopened.get("tools/list@2026-07-28", at(1)), None);
        assert_eq!(reopened.get("prompts/list@2026-07-28", at(1)), None);
    }

    #[test]
    fn clear_removes_one_server_or_every_cache_file_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let (one, two) = (command(&["one"]), command(&["two"]));
        let tools = json!({ "tools": [] });
        Cache::open(dir.path(), &one, None).put("tools/list@v", tools.clone(), at(0));
        Cache::open(dir.path(), &two, None).put("tools/list@v", tools.clone(), at(0));
        fs::write(dir.path().join("notes.json"), "{}").unwrap();

        assert_eq!(clear(dir.path(), Some((&one, None))).unwrap(), 1);
        assert_eq!(
            Cache::open(dir.path(), &one, None).get("tools/list@v", at(0)),
            None
        );
        assert_eq!(
            Cache::open(dir.path(), &two, None).get("tools/list@v", at(0)),
            Some(&tools)
        );

        assert_eq!(clear(dir.path(), None).unwrap(), 1);
        assert!(dir.path().join("notes.json").exists());
        assert_eq!(clear(&dir.path().join("missing"), None).unwrap(), 0);
    }

    #[test]
    fn summaries_name_the_server_and_describe_each_entry() {
        let dir = tempfile::tempdir().unwrap();
        let server = command(&["/bin/server", "--token", "secret"]);
        let mut cache = Cache::open(dir.path(), &server, None);
        let initialize = json!({ "protocolVersion": "2025-11-25", "serverInfo": { "name": "demo", "version": "1.2" } });
        cache.put("initialize@2025-11-25", initialize, at(0));
        cache.put("tools/list@2025-11-25", json!({ "tools": [{}, {}] }), at(0));
        cache.put(
            "prompts/list@2026-07-28",
            json!({ "prompts": [{}], "ttlMs": 60_000, "cacheScope": "public" }),
            at(0),
        );

        let summaries = summaries(dir.path(), None, at(30)).unwrap();
        let [summary] = summaries.as_slice() else {
            panic!("one cached server");
        };
        assert_eq!(summary.program.as_deref(), Some("/bin/server"));
        assert_eq!(summary.server.as_deref(), Some("demo 1.2"));
        let described: Vec<_> = summary
            .entries
            .iter()
            .map(|entry| {
                let freshness = match entry.freshness {
                    Freshness::UntilChanged => "until changed".to_owned(),
                    Freshness::Fresh(left) => format!("fresh {}s", left.as_secs()),
                    Freshness::Stale => "stale".to_owned(),
                };
                (entry.key.as_str(), entry.items, freshness)
            })
            .collect();
        assert_eq!(
            described,
            [
                ("initialize@2025-11-25", None, "until changed".to_owned()),
                ("prompts/list@2026-07-28", Some(1), "fresh 30s".to_owned()),
                ("tools/list@2025-11-25", Some(2), "until changed".to_owned()),
            ]
        );
        let file = fs::read_to_string(
            fs::read_dir(dir.path())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(
            !file.contains("secret"),
            "arguments stay out of the cache file"
        );
    }

    #[test]
    fn file_names_separate_arguments_and_folders() {
        let project = Some(Path::new("/project"));
        assert_ne!(
            file_name(&command(&["ab", "c"]), None),
            file_name(&command(&["a", "bc"]), None)
        );
        assert_ne!(
            file_name(&command(&["server"]), None),
            file_name(&command(&["server"]), project)
        );
        assert_ne!(
            file_name(&command(&["server"]), project),
            file_name(&command(&["server"]), Some(Path::new("/other")))
        );
        assert_eq!(
            file_name(&command(&["server"]), project),
            file_name(&command(&["server"]), project)
        );
    }
}
