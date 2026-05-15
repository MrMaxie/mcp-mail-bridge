# McpMailBridge

McpMailBridge is a local MCP server for mail accounts. It runs over stdio, keeps account state in SQLite, and checks per-account permissions before it lists, reads, sends, or changes mail state.

Gmail is the only implemented provider in `1.0.0`. The config model already has room for IMAP/SMTP and Microsoft 365 accounts, but those providers are not wired to mail transport yet.

## Requirements

- Rust toolchain with edition 2024 support
- A Google OAuth client if you want to use Gmail device login
- An MCP client that can start a stdio server

## Build and Check

```sh
cargo build
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

## Configure Accounts

Account data is stored in `mmb.db` next to the executable by default. Use `--database <path>` for a separate local database during development or recovery work.

```sh
cargo run -- config add
cargo run -- config list
cargo run -- config edit <account-id>
cargo run -- config remove <account-id>
cargo run -- config check
```

For Gmail accounts, use:

- provider: `gmail`
- auth kind: `oauth_token`
- account id: a short local alias, for example `work` or `personal`

The account id is not the Gmail address. MCP clients pass it as `account_id`.

`cargo run -- config add` can run Google's device OAuth flow and store the resulting token bundle in SQLite. You can also paste a local OAuth token bundle when prompted. Do not put real tokens, OAuth client secrets, mailbox content, or `mmb.db` files in git, chat, issues, PRs, logs, or docs.

## Run

Start the MCP server:

```sh
cargo run -- serve
```

Open the terminal UI for account management:

```sh
cargo run -- tui
```

Use an explicit database path when the MCP client should not use the default database:

```sh
cargo run -- --database ./.local/dev.mmb.db serve
```

## MCP Client Config

Use stdio transport:

```json
{
  "mcpServers": {
    "mcp-mail-bridge": {
      "command": "cargo",
      "args": ["run", "--", "--database", "./mmb.db", "serve"]
    }
  }
}
```

This config contains no credentials. Account setup stays in SQLite.

## Tools

| MCP tool | Permission | Behavior |
| --- | --- | --- |
| `list_accounts` | none | Lists configured accounts without secrets. |
| `list_messages` | `search` | Lists bounded message summaries. |
| `read_message` | `read` | Reads one selected message body. |
| `send_message` | `send` | Sends one message from the account. |
| `mark_as_read` | `mark_as_read` | Marks one selected message as read. |
| `mark_as_unread` | `mark_as_unread` | Marks one selected message as unread. |

Stored `read` permissions also allow summary listing for compatibility. Legacy stored `write` permissions load as `send`.

## Message Listing

`list_messages` requires `account_id` and accepts:

- `query`
- `label`, for example `INBOX`, `SENT`, or a Gmail label id
- `start_unix` and `end_unix`
- `read_state`, either `read` or `unread`
- `limit`
- `page_token`

If no time window is supplied, McpMailBridge searches the last 30 days. If one bound is supplied, both bounds are required. Windows wider than 90 days are rejected.

Listing returns summaries only. `read_message` fetches a body only for the requested `message_id`.

## Cache Rules

McpMailBridge caches bounded message lists, selected message bodies, remote version markers, and read state in SQLite.

The server reads cache data only after transient Gmail availability or transport failures. Authentication failures, identity mismatches, rejected requests, and missing messages return errors instead of stale cache data. Cached responses use `source = "gmail-cache"`.

## Sending Mail

`send_message` requires `account_id`, `to`, `subject`, and a non-empty `body`. It also accepts `cc`, `bcc`, and `body_format`.

Supported body formats:

- `text/plain`
- `plain`
- `text/html`
- `html`

Recipient and header fields reject control characters, line breaks, non-ASCII header text, and malformed recipient addresses.

## Development Notes

- Keep credentials, local databases, logs, screenshots, and scratch output out of git.
- Keep local notes and development databases under `.local/`.
- Add dependencies with exact versions, for example `cargo add crate@=x.y.z`.
- Keep MCP transport on stdio unless a human explicitly asks for another transport.

## License

See [LICENSE](LICENSE).
