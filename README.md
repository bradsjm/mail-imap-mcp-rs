# mail-imap-mcp-rs

A secure, efficient Model Context Protocol (MCP) server for IMAP email access over stdio. Provides read/write operations on IMAP mailboxes with structured output, cursor-based pagination, and security-first design.

## Features

- **Secure by default**: TLS-only connections, password secrets never logged or returned
- **Structured output**: Consistent tool response envelope with summaries and metadata
- **Cursor-based pagination**: Efficient message searching across large mailboxes
- **Message parsing**: Extract text, headers, and attachments with sanitization
- **Multi-account support**: Configure multiple IMAP accounts via environment variables
- **PDF text extraction**: Optional text extraction from PDF attachments
- **Rust-powered**: Fast, memory-safe async/await implementation with tokio
- **Write operations**: Copy, move, flag, and delete tools require explicit enable

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

### From Source

```bash
cargo install --path .
```

Binary available at `$HOME/.cargo/mail-imap-mcp-rs`.

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

### Enabling Write Operations

**By default, write operations (copy, move, delete, flag) are disabled**. Enable explicitly:

```bash
MAIL_IMAP_WRITE_ENABLED=true
```

## Multiple Accounts

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
