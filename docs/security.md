# Security Considerations

This document outlines security features and considerations for the `mail-imap-mcp-rs` MCP server.

## Transport Exposure

The server defaults to MCP over stdio. Optional streamable HTTP transport is available only when started with `--transport http`.

### HTTP Defaults

- HTTP binds to `127.0.0.1:8000` if you do not specify `--http-bind-address` or `--http-port`
- MCP is served at `/mcp`
- This localhost default reduces accidental network exposure during local development

### HTTP Exposure Warning

- The HTTP transport is plain HTTP unless you place it behind external TLS termination
- The server does not add built-in HTTP authentication
- Do not leave this server publicly reachable unless exposure is intentional and protected by a trusted boundary such as a reverse proxy, firewall, or private network
- Binding to `0.0.0.0` or `::` increases exposure and should be treated as an intentional network-serving decision

## TLS Enforcement

All IMAP connections require TLS encryption by default. Insecure connections are rejected.

### Configuration

```bash
# Per-account (default: true)
MAIL_IMAP_<ACCOUNT>_SECURE=true

# Common IMAP TLS ports
MAIL_IMAP_<ACCOUNT>_PORT=993   # IMAPS (implicit TLS)
```

### Behavior

- TLS certificate verification is enforced
- Hostname verification is performed
- Connection failures occur if certificates cannot be validated
- STARTTLS is not supported; use implicit TLS (IMAPS) on port 993

## Password Secrecy

Passwords are handled with strict secrecy guarantees:

### Storage

- Passwords are stored using Rust's `SecretString` type
- Passwords are never included in log output
- Passwords are never returned in tool responses

### Environment Variables

```bash
# Password in environment (never logged)
MAIL_IMAP_DEFAULT_PASS=your-app-password
```

### Best Practices

- Use app-specific passwords instead of account passwords when available
- Never commit `.env` files to version control
- Use secure credential managers for production deployments
- Rotate credentials periodically

## Write Operation Gating

Destructive operations are disabled by default and require explicit opt-in.

### Enabling Write Operations

```bash
MAIL_IMAP_WRITE_ENABLED=true
```

### Affected Tools

When `MAIL_IMAP_WRITE_ENABLED=false`, these tools return errors:
- `imap_apply_to_messages` - Bulk message mutation
- `imap_update_message_flags` - Bulk message flag updates
- `imap_update_message_labels` - Bulk Gmail label updates
- `imap_manage_mailbox` - Mailbox lifecycle operations

### Server Capability Requirements

- `imap_apply_to_messages` delete requires `UIDPLUS`.
- `imap_apply_to_messages` move requires native `MOVE` or `UIDPLUS`.
- `imap_apply_to_messages` copy requires neither capability.
- `imap_update_message_labels` requires `X-GM-EXT-1`.
- The server checks these capabilities in preflight and again on the execution session before mutation.
- The server never replaces delete with plain `EXPUNGE` or a Trash move.
- After a sent write with an unknown outcome, the result is `partial` with a non-retryable issue. Check the mailbox before another write.

## Output Bounding

All potentially large outputs are bounded to prevent resource exhaustion.

### Body Text

```json
{
  "body_max_chars": 2000  // Range: 1..16000, default: 2000
}
```

### HTML Output

- HTML is sanitized using `ammonia` before return
- Potentially dangerous tags and attributes are stripped
- CSS styles are removed
- JavaScript is completely removed

### Attachment Text Extraction

```json
{
  "attachment_mode": "extract_text",
  "attachment_text_max_chars": 10000  // Range: 1..64000, default: 10000
}
```

### Raw Message Source

```json
{
  "offset_bytes": 0,
  "max_bytes": 16000  // Range: 1..64000, default: 16000
}
```

### Search Previews

```json
{
  "preview_max_chars": 120  // Range: 50..500; omitted by default
}
```

Previews are whitespace-normalized and truncated to the requested Unicode-character count. The server requests at most `min(32768, MAIL_IMAP_MESSAGE_FETCH_BUDGET_BYTES)` message bytes per mailbox group. A missing or oversized body keeps the header summary and adds an issue.

### Attachment Content

```json
{
  "mode": "text",
  "max_chars": 10000  // Range: 1..64000, default: 10000
}
```

```json
{
  "mode": "bytes",
  "offset_bytes": 0,
  "max_bytes": 16000  // Range: 1..64000, default: 16000
}
```

- Text mode accepts only `max_chars`.
- Bytes mode accepts only `offset_bytes` and `max_bytes`.
- Incomplete or zero-budget content returns `limit_exceeded` with no attachment payload.
- Only a complete decoded attachment can report `size_bytes` or byte pagination.
- Unsupported text types return a structured issue and an explicit bytes-mode next action.

### Attachment Size Limits

PDF text extraction is limited to attachments ≤ 5MB. Larger attachments are skipped but do not fail the tool call.

## Input Validation

All inputs are validated before IMAP operations:

### Length Bounds

- `query`, `from`, `to`, `subject`, `message_id_header`, `gmail_query`: 1..256 characters
- `account_id`: 1..64 characters, pattern `^[A-Za-z0-9_-]+$`
- `mailbox`: 1..256 characters
- search `mailboxes`: 1..10 distinct names
- search `limit`: 1..100 messages
- mailbox-list `limit`: 1..200 without counts, 1..50 with counts
- `preview_max_chars`: 50..500
- `keyword`: 1..64 characters, a custom IMAP atom without a leading backslash
- `part_id`: 1..64 characters
- Gmail labels: 1..256 characters each, 0..32 labels per update
- message ids per write: 1..250
- flags per update: 1..32

### Content Sanitization

- Search text fields must not contain ASCII control characters
- Mailbox names must not contain ASCII control characters
- Gmail labels must not contain ASCII control characters
- Search keywords must not contain spaces, quotes, brackets, or wildcards

### Search Result Limits

Searches matching more than 1,000 combined messages across all requested mailboxes are rejected:

```
Error: invalid input: search matched 25000 messages so far across the requested mailboxes; narrow filters or mailboxes to at most 1000 results
```

Resolution: Add tighter filters (`last_days`, `from`, `subject`, date ranges) or remove mailboxes.

## Timeout Protection

All network operations have configurable timeouts:

```bash
# Connection establishment
MAIL_IMAP_CONNECT_TIMEOUT_MS=30000      # 30 seconds

# Server greeting
MAIL_IMAP_GREETING_TIMEOUT_MS=15000     # 15 seconds

# Socket operations (idle, read, write)
MAIL_IMAP_SOCKET_TIMEOUT_MS=300000     # 5 minutes
```

Timeouts prevent indefinite hanging and ensure the server remains responsive.

## Logging and Auditing

### Log Redaction

- Passwords are never logged
- Secret-like keys (`*_PASS`, `*_TOKEN`, `*_KEY`) are redacted in logs
- Message bodies and attachments are not logged

### Response Metadata

All tool responses include metadata for auditing:

```json
{
  "meta": {
    "now_utc": "2024-02-26T10:30:45.123Z",
    "duration_ms": 245
  }
}
```

## Security Best Practices

### For End Users

1. **Use app passwords**: For Gmail, Outlook, and other services, use app-specific passwords rather than account passwords
2. **Enable 2FA**: Require two-factor authentication on email accounts
3. **Review access logs**: Periodically review email account access logs for suspicious activity
4. **Restrict write access**: Keep `MAIL_IMAP_WRITE_ENABLED=false` unless needed
5. **Secure .env files**: Ensure `.env` files have restrictive permissions (`chmod 600 .env`)

### For Operators

1. **Principle of least privilege**: Run the server with minimal required permissions
2. **Network isolation**: Deploy in isolated network segments where possible, especially for HTTP mode
3. **Regular updates**: Keep dependencies and the server updated
4. **Audit logs**: Monitor server logs for unusual patterns or errors
5. **Rate limiting**: Consider implementing additional rate limiting at the infrastructure layer
6. **TLS termination**: If you must expose HTTP mode beyond localhost, terminate TLS and enforce access controls before traffic reaches the server

### For Development

1. **Security review**: Changes to security-sensitive code should be reviewed
2. **Dependency auditing**: Regularly audit dependencies for vulnerabilities
3. **Test boundaries**: Test input validation and output bounding thoroughly
4. **Secret management**: Never hardcode credentials in code or tests

## Known Limitations

1. **No STARTTLS support**: Only implicit TLS (IMAPS) is supported
2. **No certificate pinning**: Certificates are validated per standard PKI; custom CA chains are not supported
3. **No client authentication**: Client certificates are not supported
4. **No encryption at rest**: Credentials are in memory only; disk encryption is the user's responsibility
