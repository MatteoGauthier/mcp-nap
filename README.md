# mcp-nap

Keeps stdio MCP servers asleep until a tool is called.

Claude Code, Codex, Cursor and opencode start every local MCP server when a session starts and keep it running until the session ends, even if no tool is ever called. With a few sessions open, that is a few copies of every server. Put `mcp-nap` in front of a server command and the server only runs while it is used.

Chrome DevTools MCP on an Apple Silicon Mac:

| | Without mcp-nap | With mcp-nap |
| --- | --- | --- |
| Server running when a session starts | yes | no |
| Memory of an idle session | ~175 MB | 1.7 MB |
| Memory of 8 idle sessions | ~1.4 GB | ~14 MB |
| Handshake and tool list | ~0.3 s | 3 ms |
| First tool call | ~0.05 s | ~0.6 s, starts the server |
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
- `--cache-dir <dir>`: where responses are cached. Default `~/Library/Caches/mcp-nap` on macOS, `~/.cache/mcp-nap` on Linux, or `$XDG_CACHE_HOME/mcp-nap`.
- `--per-dir`: keep a separate cache per working folder, for servers whose tools, prompts or resources depend on the project they run in. Off by default, so a new folder reuses the cache of the same command.

Options end at the first argument without a dash, like with `timeout` or `env`, so the server's own flags pass through untouched. Write `--` before the command only if it starts with a dash.

## How it works

- `initialize`, `ping` and the tool, prompt and resource lists are answered from a cache.
- Any other request, such as a tool call, starts the server, replays the client's `initialize`, then forwards the request.
- After the idle time with nothing in flight, the server's whole process group is stopped, so launchers like `npx` and `uvx` go with their children. The next call starts it again.
- When a fresh server lists different tools, prompts or resources than the cache, the cache is updated and the client gets the matching `list_changed` notification.
- The first session for a command starts the server once to fill the cache.

## Limits

- The cache is keyed by the command and its arguments, plus the working folder with `--per-dir`, not by environment variables.
- State kept inside the server, such as a selected browser tab, starts over after it sleeps.
- A resource subscription keeps the server awake for the rest of the session.

## Develop

```sh
cargo test
```

The integration tests drive the binary against `examples/fake_server.rs`, a tiny MCP server that logs what it receives.

## License

MIT
