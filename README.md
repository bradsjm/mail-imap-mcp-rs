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

Choose an installation method based on your environment and preferences.

### Using NPX (Recommended)

Easiest method - no global install required.

```bash
npx @bradsjm/mail-imap-mcp-rs@latest
```

Or install globally:

```bash
npm install -g @bradsjm/mail-imap-mcp-rs
mail-imap-mcp-rs
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

### Using Docker

Pull prebuilt multi-arch image from GHCR:

```bash
docker pull ghcr.io/bradsjm/mail-imap-mcp-rs:latest
docker run --rm -i --env-file .env ghcr.io/bradsjm/mail-imap-mcp-rs:latest
```

Build locally:

```bash
docker build -t mail-imap-mcp-rs .
docker run --rm -i --env-file .env mail-imap-mcp-rs
```

### Using Cargo

```bash
cargo install mail-imap-mcp-rs
```

### From Source

```bash
cargo build --release
```

Binary available at `target/release/mail-imap-mcp-rs`.

## Quick Start

### 1. Configure Your Account

Copy the example environment file and add your credentials:

```bash
cp .env.example .env
```

Edit `.env` with your IMAP details:

```bash
# Required: connection details
MAIL_IMAP_DEFAULT_HOST=imap.gmail.com
MAIL_IMAP_DEFAULT_USER=your-email@gmail.com
MAIL_IMAP_DEFAULT_PASS=your-app-password

# Optional: defaults shown
MAIL_IMAP_DEFAULT_PORT=993
MAIL_IMAP_DEFAULT_SECURE=true
```

**Important:** Use an app-specific password, not your account password. See your email provider's documentation for generating app passwords.

### 2. Verify Connection

Use the MCP client tool to verify your account:

```json
{
  "account_id": "default"
}
```

This tests connectivity, authentication, and returns server capabilities.

### 3. List Mailboxes

```json
{
  "account_id": "default"
}
```

### 4. Search Messages

```json
{
  "account_id": "default",
  "mailbox": "INBOX",
  "limit": 10,
  "unread_only": true,
  "last_days": 7
}
```

### 5. Fetch a Message

Copy the `message_id` from search results and fetch details:

```json
{
  "account_id": "default",
  "message_id": "imap:default:INBOX:12345:42",
  "body_max_chars": 2000
}
```

## Configuration

### Single Account

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

### Enabling Write Operations

By default, write operations (copy, move, delete, flag) are disabled. Enable explicitly:

```bash
MAIL_IMAP_WRITE_ENABLED=true
```

### Advanced Configuration

For timeouts, cursor settings, and other advanced options, see [Advanced Configuration](docs/advanced-configuration.md).

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

| Tool | Purpose |
|------|---------|
| `imap_list_accounts` | List configured accounts without exposing credentials |
| `imap_verify_account` | Test connectivity, authentication, and capabilities |
| `imap_list_mailboxes` | List all visible mailboxes/folders |
| `imap_search_messages` | Search with cursor-based pagination |
| `imap_get_message` | Get parsed message details |
| `imap_get_message_raw` | Get RFC822 source for diagnostics |

### Write Operations

| Tool | Purpose |
|------|---------|
| `imap_update_message_flags` | Add/remove flags (`\Seen`, `\Flagged`, etc.) |
| `imap_copy_message` | Copy to mailbox (same or different account) |
| `imap_move_message` | Move to mailbox in same account |
| `imap_delete_message` | Delete message (requires explicit confirmation) |

Write operations require `MAIL_IMAP_WRITE_ENABLED=true`.

For complete tool contracts, input/output schemas, and validation rules, see [Tool Contract](docs/tool-contract.md).

## Usage Examples

### Search and Read Messages

1. **List accounts**: `imap_list_accounts`
2. **Search recent unread**:
   ```json
   {
     "account_id": "default",
     "mailbox": "INBOX",
     "unread_only": true,
     "last_days": 7
   }
   ```
3. **Fetch message**: Use `message_id` from search results
   ```json
   {
     "account_id": "default",
     "message_id": "imap:default:INBOX:12345:42",
     "body_max_chars": 2000
   }
   ```

### Paginate Through Results

1. **Initial search**:
   ```json
   {
     "account_id": "default",
     "mailbox": "INBOX",
     "limit": 10
   }
   ```
2. **Next page**: Use `next_cursor` from response
   ```json
   {
     "account_id": "default",
     "mailbox": "INBOX",
     "cursor": "550e8400-e29b-41d4-a716-446655440000"
   }
   ```

For details on cursor behavior, expiration, and error handling, see [Cursor Pagination](docs/cursor-pagination.md).

### Flag and Archive

Requires `MAIL_IMAP_WRITE_ENABLED=true`.

1. **Mark as flagged**:
   ```json
   {
     "account_id": "default",
     "message_id": "imap:default:INBOX:12345:42",
     "add_flags": ["\\Flagged"]
   }
   ```
2. **Copy to archive**:
   ```json
   {
     "account_id": "default",
     "message_id": "imap:default:INBOX:12345:42",
     "destination_mailbox": "Archive"
   }
   ```
3. **Delete after processing**:
   ```json
   {
     "account_id": "default",
     "message_id": "imap:default:INBOX:12345:42",
     "confirm": true
   }
   ```

### Multi-Account Workflow

1. **Search work account**:
   ```json
   {
     "account_id": "work",
     "mailbox": "INBOX",
     "subject": "urgent"
   }
   ```
2. **Copy to personal account**:
   ```json
   {
     "account_id": "work",
     "message_id": "imap:work:INBOX:67890:99",
     "destination_mailbox": "Inbox",
     "destination_account_id": "personal"
   }
   ```

## Troubleshooting

### Connection Timeout

```
Error: operation timed out: tcp connect timeout
```

Increase `MAIL_IMAP_CONNECT_TIMEOUT_MS` (default: 30,000 ms). See [Advanced Configuration](docs/advanced-configuration.md).

### Authentication Failed

```
Error: authentication failed: [AUTHENTICATIONFAILED] Authentication failed.
```

- Verify username and password are correct
- Use an app-specific password (not account password) for Gmail/Outlook
- Check account allows IMAP access

### Write Operations Disabled

```
Error: invalid input: write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true
```

Set `MAIL_IMAP_WRITE_ENABLED=true` to enable copy, move, flag, and delete operations.

### Cursor Invalid/Expired

```
Error: invalid input: cursor is invalid or expired
```

Rerun the search without a cursor. See [Cursor Pagination](docs/cursor-pagination.md) for details.

### Search Too Broad

```
Error: invalid input: search matched 25000 messages; narrow filters to at most 20000 results
```

Add tighter filters (`last_days`, `from`, `subject`, date ranges) and rerun.

### Mailbox Snapshot Changed

```
Error: conflict: mailbox snapshot changed; rerun search
```

The mailbox's `UIDVALIDITY` changed. Rerun search. See [Message ID Format](docs/message-id-format.md).

## Security

For comprehensive security documentation, see [Security Considerations](docs/security.md).

Key security features:
- **TLS enforcement**: Insecure connections rejected
- **Password secrecy**: Passwords never logged or returned
- **Bounded outputs**: Body text, HTML, attachments truncated to limits
- **Write gating**: Destructive operations require explicit opt-in
- **Delete confirmation**: Requires explicit `confirm: true`
- **HTML sanitization**: HTML sanitized using `ammonia`

## Documentation

- [Tool Contract](docs/tool-contract.md) - Complete tool definitions, input/output schemas, validation rules
- [Message ID Format](docs/message-id-format.md) - Stable message identifier format and behavior
- [Cursor Pagination](docs/cursor-pagination.md) - Pagination behavior, expiration, error handling
- [Security Considerations](docs/security.md) - Security features, best practices, limitations
- [Advanced Configuration](docs/advanced-configuration.md) - Timeouts, cursors, performance tuning

## Development

See `AGENTS.md` for contributor guidelines and build/lint/test commands.

```bash
cargo test
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

## License

MIT License - see [LICENSE](LICENSE) file for details.

## Contributing

Contributions welcome! Ensure code is formatted, linted, and tested before submitting.

## Acknowledgement

Code and documentation in this repository was AI assisted using [OpenCode](https://opencode.ai/) with various models including GPT-5 models from [OpenAI](https://openai.com/).
