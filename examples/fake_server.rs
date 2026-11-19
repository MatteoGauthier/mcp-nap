//! A tiny MCP server used by the integration tests.
//!
//! It appends `start <pid>` and every method it receives to `FAKE_SERVER_LOG`,
//! lists the tools named in `FAKE_SERVER_TOOLS` and the prompts named in `FAKE_SERVER_PROMPTS`,
//! and exits when the `crash` tool is called.

use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let log = env::var("FAKE_SERVER_LOG").ok();
    let record = |entry: &str| {
        if let Some(path) = &log {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(file, "{entry}").unwrap();
        }
    };
    record(&format!("start {}", std::process::id()));

    let names = |variable: &str, default: &str| -> Vec<String> {
        env::var(variable)
            .unwrap_or_else(|_| default.into())
            .split(',')
            .map(str::to_owned)
            .collect()
    };
    let tools: Vec<Value> = names("FAKE_SERVER_TOOLS", "echo")
        .iter()
        .map(|name| json!({ "name": name, "inputSchema": { "type": "object" } }))
        .collect();
    let prompts: Vec<Value> = names("FAKE_SERVER_PROMPTS", "greet")
        .iter()
        .map(|name| json!({ "name": name }))
        .collect();

    for line in io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let Some(method) = message["method"].as_str() else {
            continue;
        };
        record(method);
        let Some(id) = message.get("id") else {
            continue;
        };
        let result = match method {
            "initialize" => json!({
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": { "tools": { "listChanged": true }, "prompts": { "listChanged": true } },
                "serverInfo": { "name": "fake", "version": "1" }
            }),
            "tools/list" => json!({ "tools": tools }),
            "prompts/list" => json!({ "prompts": prompts }),
            "tools/call" if message["params"]["name"] == "crash" => std::process::exit(1),
            "tools/call" => json!({
                "content": [{ "type": "text", "text": format!("called {}", message["params"]["name"].as_str().unwrap_or_default()) }]
            }),
            _ => json!({}),
        };
        let mut stdout = io::stdout().lock();
        writeln!(
            stdout,
            "{}",
            json!({ "jsonrpc": "2.0", "id": id, "result": result })
        )
        .unwrap();
        stdout.flush().unwrap();
    }
}
