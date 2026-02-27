# mail-imap-mcp-rs

A secure, efficient Model Context Protocol (MCP) server for IMAP email access over stdio. Provides read/write operations on IMAP mailboxes with structured output, cursor-based pagination, and security-first design.

## Features

- **Secure by default**: TLS-only connections, password secrets never logged or returned
- **Structured output**: Consistent tool response envelope with summaries and metadata
- **Cursor-based pagination**: Efficient message searching across large mailboxes
- **Message parsing**: Extract text, headers, and attachments with sanitization
- **Multi-account support**: Configure multiple IMAP accounts via environment variables
- **Write operations gated**: Copy, move, flag, and delete tools require explicit opt-in
- **PDF text extraction**: Optional text extraction from PDF attachments
- **Rust-powered**: Fast, memory-safe async/await implementation with tokio

## Installation

### From Source

```bash
cargo build --release
```

The binary will be available at `target/release/mail-imap-mcp-rs`.

### Using Cargo

```bash
cargo install mail-imap-mcp-rs
```

### Using Docker

Build image:

```bash
docker build -t mail-imap-mcp-rs .
```

Run over stdio (for MCP clients that launch a command):

```bash
docker run --rm -i --env-file .env mail-imap-mcp-rs
```

Pull prebuilt multi-arch image from GHCR:

```bash
docker pull ghcr.io/bradsjm/mail-imap-mcp-rs:latest
docker run --rm -i --env-file .env ghcr.io/bradsjm/mail-imap-mcp-rs:latest
```

Example MCP command config:

```json
{
  "command": "docker",
  "args": [
    "run",
    "--rm",
    "-i",
    "--env-file",
    ".env",
    "mail-imap-mcp-rs"
  ]
}
```

### Using NPX

Run without global install:

```bash
npx @bradsjm/mail-imap-mcp-rs@latest
```

Install globally:

```bash
npm install -g @bradsjm/mail-imap-mcp-rs
mail-imap-mcp-rs
```

MCP command config (npx):

```json
{
  "command": "npx",
  "args": ["-y", "@bradsjm/mail-imap-mcp-rs@latest"]
}
```

### Using Curl Installer (Linux/macOS)

Install a pinned release directly from GitHub Releases:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/bradsjm/mail-imap-mcp-rs/releases/download/v0.1.0/mail-imap-mcp-rs-installer.sh | sh
```

Safer alternative (download, inspect, then run):

```bash
curl --proto '=https' --tlsv1.2 -LsSf -o mail-imap-mcp-rs-installer.sh https://github.com/bradsjm/mail-imap-mcp-rs/releases/download/v0.1.0/mail-imap-mcp-rs-installer.sh
sh mail-imap-mcp-rs-installer.sh
```

Install location defaults to `~/.local/bin`. Override with:

```bash
INSTALL_DIR="$HOME/bin" sh mail-imap-mcp-rs-installer.sh --version v0.1.0
```

## Configuration

All configuration is done via environment variables. Copy `.env.example` to `.env`, then replace placeholder values.

```bash
cp .env.example .env
```

### Single Account Setup

```bash
# Required: connection details
MAIL_IMAP_DEFAULT_HOST=imap.example.com
MAIL_IMAP_DEFAULT_USER=your-email@example.com
MAIL_IMAP_DEFAULT_PASS=your-app-password

# Optional: defaults shown
MAIL_IMAP_DEFAULT_PORT=993
MAIL_IMAP_DEFAULT_SECURE=true
```

### Multiple Accounts

```bash
# Default account
MAIL_IMAP_DEFAULT_HOST=imap.gmail.com
MAIL_IMAP_DEFAULT_USER=user@gmail.com
MAIL_IMAP_DEFAULT_PASS=app-password

# Work account
MAIL_IMAP_WORK_HOST=outlook.office365.com
MAIL_IMAP_WORK_USER=user@company.com
MAIL_IMAP_WORK_PASS=work-password

# Personal account
MAIL_IMAP_PERSONAL_HOST=imap.fastmail.com
MAIL_IMAP_PERSONAL_USER=user@fastmail.com
MAIL_IMAP_PERSONAL_PASS=personal-password
```

### Server-Wide Settings

```bash
# Enable write operations (copy, move, delete, flag updates)
MAIL_IMAP_WRITE_ENABLED=false

# Timeouts (defaults shown)
MAIL_IMAP_CONNECT_TIMEOUT_MS=30000
MAIL_IMAP_GREETING_TIMEOUT_MS=15000
MAIL_IMAP_SOCKET_TIMEOUT_MS=300000

# Cursor pagination (defaults shown)
MAIL_IMAP_CURSOR_TTL_SECONDS=600
MAIL_IMAP_CURSOR_MAX_ENTRIES=512
```

## Tool Reference

All tools return a consistent envelope:

```json
{
  "summary": "Human-readable outcome",
  "data": { /* tool-specific data */ },
  "meta": {
    "now_utc": "2024-02-26T10:30:45.123Z",
    "duration_ms": 245
  }
}
```

### Read Operations

#### `imap_list_accounts`
List configured accounts without exposing credentials.

```json
{
  "account_id": "default"
}
```

Returns account metadata (host, port, secure).

#### `imap_verify_account`
Test connectivity, authentication, and query server capabilities.

```json
{
  "account_id": "default"
}
```

Returns capabilities list and latency.

#### `imap_list_mailboxes`
List all visible mailboxes/folders for an account.

```json
{
  "account_id": "default"
}
```

Returns up to 200 mailboxes with names and delimiters.

#### `imap_search_messages`
Search messages with cursor-based pagination.

```json
{
  "account_id": "default",
  "mailbox": "INBOX",
  "limit": 10,
  "from": "sender@example.com",
  "subject": "project update",
  "last_days": 7,
  "unread_only": true,
  "include_snippet": true,
  "snippet_max_chars": 200
}
```

**Search criteria** (mutually exclusive with cursor):
- `query`: Full-text search
- `from`, `to`, `subject`: Header filters
- `unread_only`: Boolean filter
- `last_days`: Integer 1..365
- `start_date`, `end_date`: ISO 8601 dates (`YYYY-MM-DD`)
- Search text and mailbox values must not contain ASCII control characters.
- Searches matching more than 20,000 messages are rejected; narrow filters and retry.

**Pagination**: Use `next_cursor` from response to fetch next page.

#### `imap_get_message`
Get parsed message details with bounded enrichment.

```json
{
  "account_id": "default",
  "message_id": "imap:default:INBOX:12345:42",
  "body_max_chars": 2000,
  "include_headers": true,
  "include_all_headers": false,
  "include_html": false,
  "extract_attachment_text": false,
  "attachment_text_max_chars": 10000
}
```

**Message ID format**: `imap:{account_id}:{mailbox}:{uidvalidity}:{uid}`

#### `imap_get_message_raw`
Get bounded RFC822 source for diagnostics.

```json
{
  "account_id": "default",
  "message_id": "imap:default:INBOX:12345:42",
  "max_bytes": 200000
}
```

### Write Operations

Write operations require `MAIL_IMAP_WRITE_ENABLED=true`.

#### `imap_update_message_flags`
Add or remove IMAP flags (e.g., `\\Seen`, `\\Flagged`, `\\Draft`).

```json
{
  "account_id": "default",
  "message_id": "imap:default:INBOX:12345:42",
  "add_flags": ["\\Flagged", "Important"],
  "remove_flags": ["\\Seen"]
}
```

#### `imap_copy_message`
Copy message to mailbox (same or different account).

```json
{
  "account_id": "default",
  "message_id": "imap:default:INBOX:12345:42",
  "destination_mailbox": "Archive",
  "destination_account_id": "work"
}
```

#### `imap_move_message`
Move message to mailbox in same account.

```json
{
  "account_id": "default",
  "message_id": "imap:default:INBOX:12345:42",
  "destination_mailbox": "Done"
}
```

Prefer IMAP `MOVE` capability; falls back to COPY + DELETE.

#### `imap_delete_message`
Delete message from mailbox.

```json
{
  "account_id": "default",
  "message_id": "imap:default:INBOX:12345:42",
  "confirm": true
}
```

Requires explicit `confirm: true` for safety.

## Usage Examples

### Search and Process Messages

```bash
# List accounts
imap_list_accounts

# Search recent unread messages
imap_search_messages '{"account_id":"default","mailbox":"INBOX","unread_only":true,"last_days":7}'

# Get a specific message
imap_get_message '{"account_id":"default","message_id":"imap:default:INBOX:12345:42"}'
```

### Flag and Archive

```bash
# Mark as important
imap_update_message_flags '{"account_id":"default","message_id":"imap:default:INBOX:12345:42","add_flags":["\\Flagged"]}'

# Copy to archive
imap_copy_message '{"account_id":"default","message_id":"imap:default:INBOX:12345:42","destination_mailbox":"Archive"}'

# Delete after processing
imap_delete_message '{"account_id":"default","message_id":"imap:default:INBOX:12345:42","confirm":true}'
```

### Multi-Account Workflow

```bash
# Search work account
imap_search_messages '{"account_id":"work","mailbox":"INBOX","subject":"urgent"}'

# Copy to personal account
imap_copy_message '{"account_id":"work","message_id":"imap:work:INBOX:67890:99","destination_mailbox":"Inbox","destination_account_id":"personal"}'
```

## Security Considerations

- **TLS enforcement**: Insecure connections are rejected. Set `MAIL_IMAP_<ACCOUNT>_SECURE=true` (default).
- **Password secrecy**: Passwords are stored with `SecretString` and never logged.
- **Bounded outputs**: Body text, HTML, and attachment text are truncated to configured limits.
- **Write gating**: Destructive operations require `MAIL_IMAP_WRITE_ENABLED=true`.
- **Delete confirmation**: `imap_delete_message` requires explicit `confirm: true`.
- **HTML sanitization**: HTML bodies are sanitized using `ammonia` before return.
- **Attachment limits**: PDF extraction limited to 5MB files; extraction failures don't fail tool calls.

## Message ID Format

The `message_id` is a stable, opaque string that encodes the message's location:

```
imap:{account_id}:{mailbox}:{uidvalidity}:{uid}
```

Example:
- `imap:default:INBOX:12345:42` → Account "default", mailbox "INBOX", UIDVALIDITY 12345, UID 42

Mailbox names containing colons are preserved (e.g., `imap:acct:Projects:2026:Q1:999:7`).

## Cursor Pagination

Search results include pagination metadata:

```json
{
  "total": 150,
  "messages": [...],
  "next_cursor": "550e8400-e29b-41d4-a716-446655440000",
  "has_more": true
}
```

To fetch the next page:

```json
{
  "account_id": "default",
  "mailbox": "INBOX",
  "cursor": "550e8400-e29b-41d4-a716-446655440000"
}
```

Cursors expire after 10 minutes (configurable via `MAIL_IMAP_CURSOR_TTL_SECONDS`).

## Troubleshooting

### Connection Timeout

```
Error: operation timed out: tcp connect timeout
```

Increase `MAIL_IMAP_CONNECT_TIMEOUT_MS` (default: 30,000 ms).

### Authentication Failed

```
Error: authentication failed: [AUTHENTICATIONFAILED] Authentication failed.
```

Verify:
- Username and password are correct
- Using an app password (not account password) for Gmail/Outlook
- Account allows IMAP access

### Write Operations Disabled

```
Error: invalid input: write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true
```

Set `MAIL_IMAP_WRITE_ENABLED=true` to enable copy, move, flag, and delete operations.

### Cursor Invalid/Expired

```
Error: invalid input: cursor is invalid or expired
```

Rerun the search without a cursor to get a fresh result set.

### Search Too Broad

```
Error: invalid input: search matched <n> messages; narrow filters to at most 20000 results
```

Add tighter filters (`last_days`, `from`, `subject`, dates, etc.) and rerun.

### Mailbox Snapshot Changed

```
Error: conflict: mailbox snapshot changed; rerun search
```

The mailbox's `UIDVALIDITY` changed (e.g., server reset). Rerun search.

## Development

See `AGENTS.md` for contributor guidelines and build/lint/test commands.

```bash
# Run tests
cargo test

# Run GreenMail-backed IMAP integration smoke test
scripts/test-greenmail.sh

# Format check
cargo fmt -- --check

# Lint
cargo clippy --all-targets -- -D warnings
```

## Acknowledgments

This project used OpenAI GPT-5 coding assistance for implementation and documentation support.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Contributing

Contributions are welcome! Please ensure code is formatted, linted, and tested before submitting.
