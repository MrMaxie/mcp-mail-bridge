# McpMailBridge

McpMailBridge is a Rust MCP server for local mail account access. It exposes a stdio MCP server, stores account settings in a local SQLite database, and enforces per-account permissions before mail tools run.

The mail backend is still a stub. Account management, permission checks, the MCP tool surface, and the terminal account UI are in place.

## Storage

Account data is stored in `mmb.db` next to the executable by default. Use `--database <path>` to point the CLI, TUI, or MCP server at another SQLite database.

Do not commit `mmb.db`. It may contain secrets, OAuth tokens, or passwords once accounts are configured.

## Commands

Build and verify the project:

```sh
cargo build
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Start the MCP server over stdio:

```sh
cargo run -- serve
```

Manage accounts from the CLI:

```sh
cargo run -- config list
cargo run -- config add
cargo run -- config edit <account-id>
cargo run -- config remove <account-id>
cargo run -- config check
```

Use a specific database:

```sh
cargo run -- --database ./mmb.db config list
```

Open the terminal UI:

```sh
cargo run -- tui
```

In the TUI, `Account id` is a local alias used by CLI commands and MCP requests. It is not the account's email address. Use a short stable value such as `work`, `personal`, or `gmail-main`.

TUI account form controls:

- `Tab` or down arrow moves to the next field.
- Up arrow moves to the previous field.
- Left/right changes provider, auth kind, or the focused permission.
- Space toggles the focused permission.
- Enter saves.
- Esc cancels.

## MCP tools

The server currently registers these tools:

- `list_accounts`
- `list_messages`
- `read_message`
- `send_message`
- `mark_as_read`

Mail operations return placeholder responses until provider-specific backends are added. Permission checks already run against configured accounts.

## Project layout

- `src/main.rs` wires CLI parsing, tracing, and command dispatch.
- `src/cli.rs` contains non-interactive account commands and account prompts.
- `src/config.rs` owns account types, validation, database path resolution, and SQLite persistence.
- `src/permissions.rs` defines account permissions.
- `src/mcp.rs` registers MCP tools and serves stdio transport.
- `src/tui.rs` contains the terminal account manager.

## Notes for contributors

Keep credentials out of tracked files, examples, tests, logs, and issue comments. Use `cargo add crate@=x.y.z` for new dependencies so dependency changes stay explicit.
