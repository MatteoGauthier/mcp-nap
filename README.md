# mcp-nap

Keeps stdio MCP servers asleep until a tool is called.

Claude Code, Codex, Cursor and opencode start every local MCP server when a session starts and keep it running until the session ends, even if no tool is ever called. With a few sessions open, that is a few copies of every server. Put `mcp-nap` in front of a server command and the server only runs while it is used.

Chrome DevTools MCP on an Apple Silicon Mac:

| | Without mcp-nap | With mcp-nap |
| --- | --- | --- |
| Server running when a session starts | yes | no |
| Memory of an idle session | ~175 MB | 1.6 MB |
| Memory of 8 idle sessions | ~1.4 GB | ~13 MB |
| Handshake and tool list | ~0.3 s | 2 ms |
| First tool call | ~0.05 s | ~0.3 s, starts the server |
| Next tool calls | ~2 ms | ~2 ms |
| After 10 idle minutes | still running | stopped |

## Install

```sh
cargo install --path .
```

This puts `mcp-nap` in `~/.cargo/bin`. macOS and Linux only.

## Use

Write `mcp-nap` before the command of a stdio server.

Claude Code:

```sh
claude mcp add-json chrome-devtools -s user \
  '{"type":"stdio","command":"mcp-nap","args":["chrome-devtools-mcp","--browser-url=http://127.0.0.1:9222"]}'
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.chrome-devtools]
command = "mcp-nap"
args = ["chrome-devtools-mcp", "--browser-url=http://127.0.0.1:9222"]
```

Cursor (`~/.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "chrome-devtools": {
      "command": "mcp-nap",
      "args": ["chrome-devtools-mcp", "--browser-url=http://127.0.0.1:9222"]
    }
  }
}
```

opencode (`~/.config/opencode/opencode.json`):

```json
{
  "mcp": {
    "chrome-devtools": {
      "type": "local",
      "command": ["mcp-nap", "chrome-devtools-mcp", "--browser-url=http://127.0.0.1:9222"]
    }
  }
}
```

If an app does not find `mcp-nap`, use the full path, for example `/Users/you/.cargo/bin/mcp-nap`. Environment variables set on the server entry reach the real server unchanged.

## Options

- `--idle <seconds>`: idle time before the server is stopped, `0` keeps it running. Default `600`.
- `--cache-dir <dir>`: where results are cached. Default `~/Library/Caches/mcp-nap` on macOS, `$XDG_CACHE_HOME/mcp-nap` or `~/.cache/mcp-nap` on Linux.
- `--per-dir`: keep a separate cache per working folder, for servers whose tools, prompts or resources depend on the project they run in. Off by default, so a new folder reuses the cache of the same command.

Options end at the first argument without a dash, like with `timeout` or `env`, so the server's own flags pass through untouched. Write `--` before the command only if it starts with a dash.

## How it works

mcp-nap speaks both MCP protocol eras, through the official Rust SDK [rmcp](https://github.com/modelcontextprotocol/rust-sdk).

- **2026-07-28**: `server/discover` and the tool, prompt and resource lists are answered from the cache while their `ttlMs` lasts. `public` results are shared by every session, `private` ones stay in their session, and a `ttlMs` of `0` always reaches the server. Requests carry their own protocol version, so a start needs no handshake.
- **2025-11-25 and earlier**: `initialize`, `ping`, `logging/setLevel` and the lists are answered from the cache. A start replays the client's `initialize` and log level first. These results carry no cache hints, so they stay until a fresh server lists something else, which the client then hears as a `list_changed` notification.
- Any other request, such as a tool call, starts the server and is forwarded unchanged.
- The server stops after the idle time with nothing in flight: no pending request, no running task, no open subscription.
- Stopping follows the MCP stdio shutdown: close stdin, then `SIGTERM`, then `SIGKILL`, to the server's whole process group, so launchers like `npx` and `uvx` go with their children. mcp-nap does the same when its client stops it.
- The first session for a command starts the server once to fill the cache.

## Limits

- The cache is keyed by the command and its arguments, plus the working folder with `--per-dir`, not by environment variables.
- State kept inside the server, such as a selected browser tab, starts over after it sleeps.
- A server on 2026-07-28 that keeps the SDK default `ttlMs` of `0` is asked for its lists in every session, as the spec requires.

## Develop

```sh
cargo test
```

The unit tests cover the protocol rules in `src/session.rs` without processes. The integration tests drive the binary with the rmcp client, on both protocol eras, against `examples/fake_server.rs`, an rmcp server that logs what it handles.

## Todo

- [ ] CI: a GitHub Actions workflow running `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test` on macOS and Linux.
- [ ] Release binaries, for example with `cargo-dist`, so mcp-nap installs without a Rust toolchain.
- [ ] Publish to crates.io, so `cargo install mcp-nap` works.

## License

MIT
