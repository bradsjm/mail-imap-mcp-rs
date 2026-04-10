# mail-imap-mcp-rs

A secure, efficient Model Context Protocol (MCP) server for IMAP email access over stdio by default, with optional streamable HTTP transport. Provides read/write operations on IMAP mailboxes with structured output, cursor-based pagination, and security-first design.

## Features

- **Secure by default**: TLS-only connections, password secrets never logged or returned
- **Structured output**: Consistent tool response envelope with summaries and metadata
- **Cursor-based pagination**: Efficient message searching across large mailboxes
- **Message parsing**: Extract text, HTML, headers, and attachments with sanitization
- **Multi-account support**: Configure multiple IMAP accounts via environment variables
- **PDF text extraction**: Optional text extraction from PDF attachments
- **Rust-powered**: Fast, memory-safe async/await implementation with tokio
- **Write operations**: Message mutations and mailbox management require explicit enable

## Installation

Choose an installation method based on your environment and preferences.

### Using NPX (Recommended)

Easiest method - no global install required.

```bash
npx @bradsjm/mail-imap-mcp-rs@latest
```

Optional HTTP transport example:

```bash
npx @bradsjm/mail-imap-mcp-rs@latest --transport http
```

Supported npm/native targets:
- macOS: Apple Silicon (`aarch64-apple-darwin`), Intel (`x86_64-apple-darwin`)
- Linux `arm64`: glibc (`aarch64-unknown-linux-gnu`)
- Linux x64: glibc (`x86_64-unknown-linux-gnu`), musl/Alpine (`x86_64-unknown-linux-musl`)
- Windows x64: MSVC (`x86_64-pc-windows-msvc`)

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

Optional HTTP transport example:

```bash
docker run --rm -p 127.0.0.1:8000:8000 --env-file .env ghcr.io/bradsjm/mail-imap-mcp-rs:latest --transport http
```

Build locally:

```bash
docker build -t mail-imap-mcp-rs .
docker run --rm -i --env-file .env mail-imap-mcp-rs
```

Docker remains an option for Linux `arm64`, but it is now part of the npm/native release matrix.

### From Source

```bash
cargo install --path .
```

Binary available at `$HOME/.cargo/mail-imap-mcp-rs`.

## Transport Modes

The server defaults to `stdio`, which is the right mode for most MCP desktop integrations.

Use streamable HTTP only when you explicitly need it:

```bash
mail-imap-mcp-rs --transport http
```

Optional bind controls:

```bash
mail-imap-mcp-rs --transport http --http-bind-address 127.0.0.1 --http-port 8000
```

HTTP mode serves MCP at `http://127.0.0.1:8000/mcp` by default.

Security note:
- The HTTP transport defaults to localhost only.
- The server does not add built-in HTTP authentication or TLS termination.
- Do not leave this server publicly reachable unless exposure is intentional and protected by a trusted boundary such as a reverse proxy, firewall, or private network.

## Quick Start

### Configure MCP

Use this example MCP configuration and add your credentials:

```json
{
  "mcpServers": {
    "server-name": {
      "command": "npx",
      "args": ["-y", "@bradsjm/mail-imap-mcp-rs@latest"],
      "env": {
        "MAIL_IMAP_DEFAULT_HOST": "imap.gmail.com",
        "MAIL_IMAP_DEFAULT_USER": "your-email@gmail.com",
        "MAIL_IMAP_DEFAULT_PASS": "your-app-password"
      }
    }
  }
}
```

```bash
# Optional: defaults shown
MAIL_IMAP_DEFAULT_PORT=993
MAIL_IMAP_DEFAULT_SECURE=true
```

**Important:** Use an app-specific password, not your account password. See your email provider's documentation for generating app passwords.

### Enabling Write Operations

**By default, write operations (message mutations and mailbox management) are disabled**. Enable explicitly:

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

For timeouts, cursor settings, read-session cache tuning, and other advanced options, see [Advanced Configuration](docs/advanced-configuration.md).

To trust a private or self-signed IMAP server certificate without disabling TLS verification, set:

```bash
MAIL_IMAP_CA_CERT_PATH=/path/to/ca-certificates.pem
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

| Tool | Purpose |
|------|---------|
| `imap_list_accounts` | List configured accounts without exposing credentials |
| `imap_list_mailboxes` | List all visible mailboxes/folders |
| `imap_search_messages` | Search with cursor-based pagination |
| `imap_get_message` | Get parsed message details |
| `imap_get_message_raw` | Get RFC822 source for diagnostics |

### Write Operations

| Tool | Purpose |
|------|---------|
| `imap_apply_to_messages` | Apply one action (`move`, `copy`, `delete`) to explicit messages |
| `imap_update_message_flags` | Add, remove, or replace flags on explicit messages |
| `imap_manage_mailbox` | Create, rename, or delete a mailbox |
| `imap_get_operation` | Poll a write operation until it reaches a terminal state |
| `imap_cancel_operation` | Request cancellation for a running write operation |

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

Set `MAIL_IMAP_WRITE_ENABLED=true` to enable message mutation and mailbox management operations.

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
- **Local HTTP default**: Optional HTTP transport binds to localhost unless you override it
- **Password secrecy**: Passwords never logged or returned
- **Bounded outputs**: Body text, HTML, attachments truncated to limits
- **Write gating**: Destructive operations require explicit opt-in
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
