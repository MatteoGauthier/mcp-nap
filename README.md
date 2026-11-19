# mcp-nap

Keeps stdio MCP servers asleep until a tool is called.

Coding agent like Claude Code starts every local MCP server when a session starts and keep it running until the session ends, even if no tool is ever called. With a few sessions open, that is a few copies of every server, which wastes memory and CPU. Put `mcp-nap` in front of a server command and the server only runs while it is used.

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

## Getting started

The quickest way: paste this into your coding agent.

> Install mcp-nap with `cargo install --git https://github.com/MatteoGauthier/mcp-nap`, then for each local stdio MCP server configured in my coding agents and editors, propose putting the full path of `mcp-nap` before its command, show me the diff with secrets masked, and write the files only after I approve.

### Install

```sh
cargo install --git https://github.com/MatteoGauthier/mcp-nap
```

This puts `mcp-nap` in `~/.cargo/bin`. It needs Rust 1.88 or later. It runs on macOS and Linux, and builds for Windows, where it has not been run yet. From a clone, use `cargo install --path .`.

### Configure coding agents

Write `mcp-nap` before the command of each local stdio MCP server.

<details>
<summary>Claude Code</summary>

```sh
claude mcp add-json chrome-devtools -s user \
  '{"type":"stdio","command":"mcp-nap","args":["chrome-devtools-mcp","--browser-url=http://127.0.0.1:9222"]}'
```

</details>

<details>
<summary>Codex <code>~/.codex/config.toml</code></summary>

```toml
[mcp_servers.chrome-devtools]
command = "mcp-nap"
args = ["chrome-devtools-mcp", "--browser-url=http://127.0.0.1:9222"]
```

</details>

<details>
<summary>Cursor <code>~/.cursor/mcp.json</code></summary>

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

</details>

<details>
<summary>opencode <code>~/.config/opencode/opencode.json</code></summary>

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

</details>

If an app does not find `mcp-nap`, use the full path, for example `/Users/you/.cargo/bin/mcp-nap`. Environment variables set on the server entry reach the real server unchanged.

## Help

```console
$ mcp-nap --help
Keeps a stdio MCP server asleep until a tool is called.

Answers the handshake and the tool, prompt and resource lists from a cache, starts SERVER on the first other request, and stops it after the idle time.

Usage: mcp-nap [OPTIONS] <SERVER>...
       mcp-nap cache <list|clear> [OPTIONS] [SERVER]...

Commands:
  cache  Show or clear cached results

Arguments:
  <SERVER>...
          The server command and its arguments, passed through untouched.

          Options end at the first argument without a dash, so the server's own flags are left alone. Write -- before a server that starts with a dash or is named cache.

Options:
      --idle <SECONDS>
          Seconds without activity before the server is stopped, 0 keeps it running

          [default: 600]

      --cache-dir <DIR>
          Where cached results are kept.

          [default: ~/Library/Caches/mcp-nap on macOS, $XDG_CACHE_HOME/mcp-nap or ~/.cache/mcp-nap on Linux, %LOCALAPPDATA%\mcp-nap on Windows]

      --per-dir
          Keep a separate cache per working folder, for servers whose tools, prompts or resources depend on the project they run in.

  -q, --quiet
          Log errors only

  -v, --verbose
          Also log each request answered from the cache or forwarded to the server

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## How it works

mcp-nap speaks both MCP protocol eras, through the official Rust SDK [rmcp](https://github.com/modelcontextprotocol/rust-sdk).

- **2026-07-28**: `server/discover` and the tool, prompt and resource lists are answered from the cache while their `ttlMs` lasts. `public` results are shared by every session, `private` ones stay in their session, and a `ttlMs` of `0` always reaches the server. Requests carry their own protocol version, so a start needs no handshake.
- **2025-11-25 and earlier**: `initialize`, `ping`, `logging/setLevel` and the lists are answered from the cache. A start replays the client's `initialize` and log level first. These results carry no cache hints, so they stay until a fresh server lists something else, which the client then hears as a `list_changed` notification.
- Any other request, such as a tool call, starts the server and is forwarded unchanged.
- The server stops after the idle time with nothing in flight: no pending request, no running task, no open subscription.
- Stopping follows the MCP stdio shutdown: close stdin, then `SIGTERM`, then `SIGKILL`, to the server's whole process group, so launchers like `npx` and `uvx` go with their children. On Windows the server runs in a job object, which is closed after stdin. mcp-nap does the same when its client stops it.
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

- [ ] CI: a GitHub Actions workflow running `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test` on macOS, Linux and Windows.
- [ ] Release binaries, for example with `cargo-dist`, so mcp-nap installs without a Rust toolchain.
- [ ] Publish to crates.io, so `cargo install mcp-nap` works.

## License

MIT
