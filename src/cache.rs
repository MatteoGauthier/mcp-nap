use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Handshake and list responses of one server command, stored as one JSON file.
pub struct Cache {
    path: PathBuf,
    entries: Map<String, Value>,
}

impl Cache {
    pub fn open(dir: &Path, command: &[String]) -> Cache {
        let path = dir.join(format!("{:016x}.json", command_hash(command)));
        let entries = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Cache { path, entries }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.get(key)
    }

    pub fn set(&mut self, key: &str, value: Value) {
        if self.entries.get(key) == Some(&value) {
            return;
        }
        self.entries.insert(key.to_owned(), value);
        self.save();
    }

    pub fn remove(&mut self, key: &str) {
        if self.entries.remove(key).is_some() {
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
        fs::write(&temporary, serde_json::to_vec(&self.entries)?)?;
        fs::rename(&temporary, &self.path)
    }
}

pub fn default_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_CACHE_HOME").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir).join("mcp-nap");
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/mcp-nap")
    } else {
        home.join(".cache/mcp-nap")
    }
}

/// FNV-1a over the command parts, stable across Rust versions unlike `DefaultHasher`.
fn command_hash(command: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in command {
        for byte in part.bytes().chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn keeps_entries_across_opens() {
        let dir = env::temp_dir().join(format!("mcp-nap-cache-{}", std::process::id()));
        let server = command(&["server", "--flag"]);

        let mut cache = Cache::open(&dir, &server);
        cache.set("tools/list", json!({ "tools": [] }));
        cache.set("prompts/list", json!({ "prompts": [] }));
        cache.remove("prompts/list");

        let reopened = Cache::open(&dir, &server);
        assert_eq!(reopened.get("tools/list"), Some(&json!({ "tools": [] })));
        assert_eq!(reopened.get("prompts/list"), None);
        assert_eq!(
            Cache::open(&dir, &command(&["other"])).get("tools/list"),
            None
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hash_separates_arguments() {
        assert_ne!(
            command_hash(&command(&["ab", "c"])),
            command_hash(&command(&["a", "bc"]))
        );
        assert_eq!(
            command_hash(&command(&["server"])),
            command_hash(&command(&["server"]))
        );
    }
}
