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

/// Longest `ttlMs` honored, the same cap as the rmcp client cache.
const MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Handshake, discover and list results of one server, as the MCP cache hints allow.
///
/// Public and hint-less results are shared by every session through a JSON file.
/// Private results stay in this session, since another session may hold other credentials.
pub struct Cache {
    path: PathBuf,
    shared: HashMap<String, Entry>,
    private: HashMap<String, Entry>,
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
        let shared = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
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
            .or_else(|| self.shared.get(key))
            .filter(|entry| entry.expires_at_ms.is_none_or(|at| at > now))
            .map(|entry| &entry.result)
    }

    /// Stores a server result under `key`, following its `ttlMs` and `cacheScope` hints.
    pub fn put(&mut self, key: &str, result: Value, now: SystemTime) {
        let hints = Hints::of(&result);
        self.private.remove(key);
        let shared_before = self.shared.remove(key);
        let entry = |ttl: Option<Duration>| Entry {
            expires_at_ms: ttl.map(|ttl| unix_ms(now) + ttl.as_millis() as u64),
            result,
        };
        match hints {
            Hints::None => {
                self.shared.insert(key.to_owned(), entry(None));
            }
            Hints::Ttl { ttl, .. } if ttl.is_zero() => {}
            Hints::Ttl {
                ttl,
                scope: CacheScope::Private,
            } => {
                self.private.insert(key.to_owned(), entry(Some(ttl)));
            }
            Hints::Ttl { ttl, .. } => {
                self.shared.insert(key.to_owned(), entry(Some(ttl)));
            }
        }
        if self.shared.get(key) != shared_before.as_ref() {
            self.save();
        }
    }

    /// Drops every cached result of `method`, for all protocol versions.
    pub fn forget(&mut self, method: &str) {
        let matches = |key: &String| key == method || key.starts_with(&format!("{method}@"));
        self.private.retain(|key, _| !matches(key));
        let before = self.shared.len();
        self.shared.retain(|key, _| !matches(key));
        if self.shared.len() != before {
            self.save();
        }
    }

    fn save(&self) {
        if let Err(error) = self.write() {
            eprintln!("mcp-nap: cannot write {}: {error}", self.path.display());
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
