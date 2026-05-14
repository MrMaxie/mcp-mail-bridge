# McpMailBridge

McpMailBridge is a Rust MCP server for local Gmail account access. It exposes a stdio MCP server, stores account settings and OAuth credential material in a local SQLite database, and enforces per-account permissions before mail tools run.

Version 1.0 focuses on Gmail. IMAP/SMTP and Microsoft 365 account types are accepted by configuration for future compatibility, but Gmail is the implemented provider.

## Storage

Account data is stored in `mmb.db` next to the executable by default. Use `--database <path>` to point the CLI, TUI, or MCP server at another SQLite database.

Do not commit `mmb.db`. It can contain OAuth access tokens, refresh tokens, client credentials, cached message metadata, and cached message bodies.

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
cargo run -- --database ./mmb.db serve
```

Open the terminal UI:

```sh
cargo run -- tui
```

## Gmail Account Setup

Use `Provider = gmail` and `Auth kind = oauth_token`.

For local testing, the account secret can be either a short-lived Gmail OAuth access token or a JSON OAuth token bundle. `cargo run -- config add` can create this bundle through Google's device OAuth flow, or you can paste an existing local bundle. A bundle lets the server refresh access tokens without repeating local login:

```json
{
  "access_token": "optional-current-access-token",
  "refresh_token": "local-refresh-token",
  "client_id": "local-oauth-client-id",
  "client_secret": "local-oauth-client-secret",
  "expires_at_unix": 1770000000
}
```

`expires_at_unix` is optional. When a bundle includes `access_token` but omits `expires_at_unix`, McpMailBridge treats the cached access token as stale and refreshes immediately with the refresh token. When `expires_at_unix` is present, the cached access token is used only if it remains valid for more than the 60-second refresh safety window.

Store that JSON only through the CLI or TUI prompt so it lands in the local SQLite database. If `token_uri` is present, it must be `https://oauth2.googleapis.com/token`; custom token endpoints are rejected before refresh credentials are used so they are not sent to non-Google hosts. Do not paste real tokens into chat, README examples, Linear, GitHub, logs, or tracked files.

`Account id` is a local alias used by CLI commands and MCP requests. It is not the account email address. Use a short stable value such as `work`, `personal`, or `gmail-main`.

The Gmail adapter validates that the authenticated Gmail profile email matches the configured account email before mail operations run.

## Permissions

Accounts can have these permissions:

- `search` - list/search bounded message summaries.
- `read` - read one selected message body.
- `send` - send mail from the account.
- `mark_as_read` - mark one selected message as read.
- `mark_as_unread` - mark one selected message as unread.

For compatibility, stored `read` permissions also allow summary search/listing, and legacy stored `write` permissions load as `send`.

## MCP Tools

The server registers these tools:

- `list_accounts`
- `list_messages`
- `read_message`
- `send_message`
- `mark_as_read`
- `mark_as_unread`

`list_messages` requires `account_id` and accepts:

- `query`
- `label`: one Gmail label/mailbox atom such as `INBOX`, `SENT`, or a custom label id
- `start_unix`
- `end_unix`
- `read_state`: `read` or `unread`
- `limit`
- `page_token`

When `start_unix` and `end_unix` are both omitted, the server uses a safe default window covering the last 30 days. If one bound is provided, both must be provided. Any requested window wider than 90 days is rejected. Search/list responses return summaries only and never message bodies.

`read_message` requires `account_id` and `message_id`. Bodies are fetched only for the selected message and cached locally after a successful provider read.

`send_message` requires `account_id`, `to`, `subject`, and a non-empty `body`. It also accepts optional `cc`, `bcc`, and `body_format` values. Supported body formats are `text/plain`, `plain`, `text/html`, and `html`. Recipient and header fields reject unsafe control characters, line breaks, non-ASCII header text, and malformed recipient addresses.

`mark_as_read` and `mark_as_unread` require `account_id` and `message_id`. They mutate one selected message only and update local cached state after Gmail confirms the change.

## Cache Behavior

McpMailBridge stores bounded summary windows, fetched message bodies, remote version markers, and message read state in SQLite. The cache is keyed by account id, query, Gmail label id, date window, read-state filter, limit, and page token.

There is no fetch-all mailbox path. Cache entries are created from bounded list/search requests or explicit reads of one message id. Cached message bodies can be returned when metadata refresh is temporarily unavailable; a Gmail `not found` response is still reported as an error.

Cached responses are used only for transient Gmail availability or transport failures. Authentication failures, identity mismatches, rejected requests, and missing messages are returned as errors instead of falling back to stale local data. Responses served from local cache use `source = "gmail-cache"` so clients can distinguish them from live Gmail responses.

## TUI Controls

In the TUI account form:

- `Tab` or down arrow moves to the next field.
- Up arrow moves to the previous field.
- Left/right changes provider, auth kind, or the focused permission.
- Space toggles the focused permission.
- Enter saves.
- Esc cancels.

## MCP Client Example

Use stdio transport and pass the database path if the client should not use the executable-adjacent default:

```json
{
  "mcpServers": {
    "mcp-mail-bridge": {
      "command": "cargo",
      "args": [
        "run",
        "--",
        "--database",
        "./mmb.db",
        "serve"
      ]
    }
  }
}
```

The example contains no credentials. Account setup stays in the local database.

## Project Layout

- `src/main.rs` wires CLI parsing, tracing, and command dispatch.
- `src/cli.rs` contains non-interactive account commands and account prompts.
- `src/config.rs` owns account types, validation, database path resolution, SQLite persistence, migrations, and local mail cache persistence.
- `src/permissions.rs` defines account permissions.
- `src/mail.rs` defines the mail adapter contract and Gmail implementation.
- `src/mcp.rs` registers MCP tools and serves stdio transport.
- `src/tui.rs` contains the terminal account manager.

## Notes For Contributors

Keep credentials out of tracked files, examples, tests, logs, issue comments, and chat output. Use `cargo add crate@=x.y.z` for new dependencies so dependency changes stay explicit.
