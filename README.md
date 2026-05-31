# mcp-pty-server

A stateful MCP (Model Context Protocol) server that spawns and drives PTY (pseudo-terminal) sessions over stdio. Built with Rust using the `rmcp` crate.

Sessions persist across tool calls. Each session owns a real PTY plus a background reader thread that feeds a `vt100` parser, so the screen state stays in sync and can be observed or synchronized against without manual pumping.

## Building

```bash
cargo build           # dev
cargo build --release # release binary at target/release/mcp-pty-server
```

Rust edition 2021. No external system dependencies beyond a working `cc` toolchain (for transitive C crates like `portable-pty`).

## Running

The server speaks JSON-RPC on stdout, logs on stderr. It is meant to be launched by an MCP client (e.g. Claude Code).

### Claude Code configuration

Add to `.mcp.json` or `~/.claude.json`:

```json
{
  "mcpServers": {
    "mcp-pty-server": {
      "command": "/path/to/mcp-pty-server"
    }
  }
}
```

## Tools

All tools are scoped under the `pty_` prefix. A session is identified by the `session_id` string returned from `pty_launch` (e.g. `"pty-1"`).

| Tool                  | Required params                 | Purpose                                                                 |
|-----------------------|----------------------------------|-------------------------------------------------------------------------|
| `pty_launch`          | —                                | Spawn a new PTY session. Returns `session_id`.                          |
| `pty_list`            | —                                | List all active sessions with status and dimensions.                    |
| `pty_send_keys`       | `session_id`, `keys`             | Send keystrokes (with special-key processing) to a session.             |
| `pty_get_screen`      | `session_id`                     | Get the current visible screen text.                                    |
| `pty_get_cursor`      | `session_id`                     | Get cursor position as `{row, col}` (0-indexed).                        |
| `pty_get_scrollback`  | `session_id`                     | Get lines that scrolled off the top of the visible screen.              |
| `pty_resize`          | `session_id`, `cols`, `rows`     | Resize the session's terminal.                                          |
| `pty_wait_for`        | `session_id`, `pattern`          | Block until a regex matches the screen, or `timeout_ms` elapses.        |
| `pty_wait_for_idle`   | `session_id`                     | Block until no output for `idle_seconds`, or `timeout` elapses.         |
| `pty_kill`            | `session_id`                     | Kill the session and remove it from the manager.                        |

### Optional parameters

- `pty_launch` accepts `command` (default: `$SHELL` or `/bin/sh`), `args`, `cwd`, `cols` (default 80), `rows` (default 24).
- `pty_get_scrollback` accepts `lines` (default 100, oldest first).
- `pty_wait_for` accepts `timeout_ms` (default 30000).
- `pty_wait_for_idle` accepts `idle_seconds` (default 2.0) and `timeout` (default 60.0).

### Special key sequences in `pty_send_keys`

The `keys` string is processed to translate convenient placeholders into the raw bytes a terminal expects:

- `^A` … `^Z` — control characters (case-insensitive)
- `[CTRL+A]` … `[CTRL+Z]` — same as `^A` … `^Z`
- `\n`, `\r`, `\t` — literal byte escapes
- `\xNN` — hex byte
- `[ENTER]` (sends `\r`), `[TAB]`, `[SHIFT+TAB]`, `[ESCAPE]`, `[BACKSPACE]`, `[DELETE]`
- `[UP]`, `[DOWN]`, `[LEFT]`, `[RIGHT]`, `[HOME]`, `[END]`, `[PGUP]`, `[PGDN]`
- `[F1]` … `[F12]`
- `[PASTE_START]`, `[PASTE_END]` — bracketed-paste markers

Anything else passes through unchanged.

### Example session

```
pty_launch  {}                                                  -> { "session_id": "pty-1", ... }
pty_send_keys { session_id: "pty-1", keys: "ls -la[ENTER]" }    -> { "bytes_sent": 7 }
pty_wait_for_idle { session_id: "pty-1", idle_seconds: 0.5 }    -> { "status": "idle", ... }
pty_get_screen { session_id: "pty-1" }                          -> "<file listing>"
pty_kill { session_id: "pty-1" }                                -> { "killed": true }
```

## Architecture

Four files under `src/`:

- **`main.rs`** — `PtyServer`: the MCP entrypoint. Defines the 10 tool handlers, parameter structs (`schemars`-derived), and the rmcp `ServerHandler` implementation.
- **`session.rs`** — `PtySession`: owns the `portable_pty` master + writer, spawns a per-session reader thread that pumps bytes into a `vt100::Parser` behind a `Mutex<SharedState>`, and tracks the last-output time for idle detection. Also defines `PtyError` and `SessionStatus`.
- **`manager.rs`** — `SessionManager`: `HashMap<SessionId, PtySession>` with monotonic id allocation (`pty-1`, `pty-2`, …), a session cap, and natural-sorted listing.
- **`keys.rs`** — `process_special_keys`: pure string-to-bytes translator for the placeholders listed above.

### Concurrency model

- The MCP runtime is `tokio`. A single `Arc<tokio::sync::Mutex<SessionManager>>` guards the session table.
- Each session has one dedicated OS thread doing blocking reads on the PTY master fd. When bytes arrive it locks `state` (a `std::sync::Mutex<SharedState>`), feeds the parser, updates the timestamp, and releases.
- Tool handlers grab the manager lock, then the session's state lock, take a snapshot (e.g. of screen contents), and release before doing any waiting. `pty_wait_for` and `pty_wait_for_idle` poll on a `tokio::time::sleep` cadence rather than holding the lock.
- When the child exits, the reader thread sees EOF, reaps the child, and stores `SessionStatus::Exited(code)` in shared state. Subsequent writes return `PtyError::Exited`.

## Dependencies

| Crate                              | Purpose                                       |
|------------------------------------|-----------------------------------------------|
| `rmcp` 0.16                        | MCP server framework (stdio transport)        |
| `tokio` 1                          | Async runtime                                 |
| `portable-pty` 0.8                 | Cross-platform PTY spawning                   |
| `vt100` 0.15                       | VT100/xterm screen state machine              |
| `regex` 1                          | Pattern matching for `pty_wait_for`           |
| `serde` / `serde_json`             | JSON serialization                            |
| `schemars` 0.8                     | JSON Schema generation for tool params        |
| `thiserror` 1                      | `PtyError` enum                               |
| `anyhow`                           | Error handling in `main`                      |
| `tracing` / `tracing-subscriber`   | Structured logging to stderr                  |

## Tests

```bash
cargo test
```

Covers special-key processing, manager bookkeeping, real PTY spawn/output/resize/exit lifecycle, and end-to-end tool calls (launch / list / send / wait_for / wait_for_idle / cursor / kill / error paths).

## Reference implementations

This server is modeled after:

- **`../mcp-read-file`** — rmcp plumbing pattern (single-binary, stdio transport, `tool_router`/`tool_handler` macros).
- **`../tttt/crates/tttt-pty`** — PTY session semantics and the special-key vocabulary.

## License

MIT — see [LICENSE](LICENSE).
