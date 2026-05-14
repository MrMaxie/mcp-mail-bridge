# McpMailBridge

McpMailBridge is a local MCP server for Gmail. It runs over stdio, stores account settings in SQLite, and checks account permissions before it lists, reads, sends, or changes mail state.

Gmail is the only implemented provider in version 1.0. The config model also accepts IMAP/SMTP and Microsoft 365 accounts so those providers can be added later.

## Quick start

Build and check the project:

```sh
cargo build
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Create or edit accounts:

```sh
cargo run -- config add
cargo run -- config list
cargo run -- config edit <account-id>
cargo run -- config remove <account-id>
cargo run -- config check
```

Run the MCP server:

```sh
cargo run -- serve
```

Open the terminal UI:

```sh
cargo run -- tui
```

By default, account data lives in `mmb.db` next to the executable. Use `--database <path>` when you want a separate local database:

```sh
cargo run -- --database ./.local/dev.mmb.db config list
cargo run -- --database ./.local/dev.mmb.db serve
```

Never commit an `mmb.db` file. It can contain OAuth tokens, OAuth client credentials, cached message metadata, and cached message bodies.

## Gmail setup

Use these account values:

- Provider: `gmail`
- Auth kind: `oauth_token`
- Account id: a short local alias such as `work`, `personal`, or `gmail-main`

The account id is not the Gmail address. It is the name MCP clients pass as `account_id`.

`cargo run -- config add` can run Google's device OAuth flow and store the resulting token bundle. You can also paste a local bundle by hand:

```json
{
  "access_token": "optional-current-access-token",
  "refresh_token": "local-refresh-token",
  "client_id": "local-oauth-client-id",
  "client_secret": "local-oauth-client-secret",
  "token_uri": "https://oauth2.googleapis.com/token",
  "expires_at_unix": 1770000000
}
```

The example values are placeholders. Store real values only through the CLI or TUI prompt so they land in the local SQLite database.

Token rules:

- `refresh_token`, `client_id`, and `client_secret` are required for refreshable bundles.
- `token_uri`, when present, must be `https://oauth2.googleapis.com/token`.
- If `expires_at_unix` is missing, McpMailBridge treats any cached `access_token` as stale and refreshes immediately.
- Gmail identity validation must pass before a Gmail OAuth account is saved or used.

## MCP client config

Use stdio transport. Pass `--database` if the client should use a specific local database:

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

This config contains no credentials. Account setup stays in SQLite.

## Tools and permissions

| MCP tool | Required permission | Notes |
| --- | --- | --- |
| `list_accounts` | none | Lists configured accounts without secrets. |
| `list_messages` | `search` | Returns bounded summaries only. |
| `read_message` | `read` | Reads one selected message body. |
| `send_message` | `send` | Sends one message from the account. |
| `mark_as_read` | `mark_as_read` | Marks one selected message as read. |
| `mark_as_unread` | `mark_as_unread` | Marks one selected message as unread. |

Stored `read` permissions also allow summary listing for compatibility. Legacy stored `write` permissions load as `send`.

## Message listing rules

`list_messages` requires `account_id`. It also accepts:

- `query`
- `label`, for example `INBOX`, `SENT`, or a Gmail label id
- `start_unix` and `end_unix`
- `read_state`, either `read` or `unread`
- `limit`
- `page_token`

If no time window is supplied, McpMailBridge searches the last 30 days. If one bound is supplied, both bounds are required. Windows wider than 90 days are rejected.

There is no fetch-all mailbox path. Listing returns summaries only; `read_message` fetches a body only for the requested `message_id`.

## Cache behavior

McpMailBridge caches bounded message lists, selected message bodies, remote version markers, and read state in SQLite.

Cached data is used only for transient Gmail availability or transport failures. Authentication failures, identity mismatches, rejected requests, and missing messages return errors instead of stale cache data. Responses served from cache use `source = "gmail-cache"`.

## Sending mail

`send_message` requires `account_id`, `to`, `subject`, and a non-empty `body`. It also accepts `cc`, `bcc`, and `body_format`.

Supported body formats:

- `text/plain`
- `plain`
- `text/html`
- `html`

Recipient and header fields reject control characters, line breaks, non-ASCII header text, and malformed recipient addresses.

## Development notes

Keep credentials out of tracked files, tests, logs, issue comments, PR comments, and chat. README examples must stay fake.

Use `cargo add crate@=x.y.z` for new dependencies so dependency changes are explicit.
