# MCP Tool Contract

This document defines the first build artifact for `mail-imap-mcp-rs`: the server-facing MCP contract.
It is the source of truth for tool names, input/output shapes, validation bounds, and safety rules.

## Design Decisions

- Transport: stdio only.
- Auth/config: environment variables only.
- Message locator: stable `message_id` format `imap:{account_id}:{mailbox}:{uidvalidity}:{uid}`.
- Destructive/write operations: disabled by default and explicitly gated.
- Output style: concise summaries with bounded structured data.
- Compatibility: no backward compatibility requirement with earlier implementations.
- MCP input schemas must remain client-safe: plain object properties only, with conditional rules enforced at runtime rather than schema unions.

## Shared Input Types

### `account_id`

- Type: string
- Pattern: `^[A-Za-z0-9_-]{1,64}$`
- Default: `"default"`

### `mailbox`

- Type: string
- Length: 1..256

### `message_id`

- Type: string
- Format: `imap:{account_id}:{mailbox}:{uidvalidity}:{uid}`
- Validation rules:
  - Prefix must be `imap`.
  - `uidvalidity` and `uid` must be non-negative integers.
  - Parsed `account_id` must match requested account.

### `limit`

- Type: integer
- Range: 1..50
- Default: 10

## Shared Output Envelope

All tools return:

```json
{
  "summary": "human-readable one-line outcome",
  "data": {},
  "meta": {
    "now_utc": "ISO-8601 UTC timestamp",
    "duration_ms": 0
  }
}
```

Error responses use a consistent shape:

```json
{
  "error": {
    "code": "invalid_input|auth_failed|not_found|timeout|conflict|internal",
    "message": "actionable message",
    "details": {}
  },
  "meta": {
    "now_utc": "ISO-8601 UTC timestamp",
    "duration_ms": 0
  }
}
```

Runtime IMAP command failures are returned in successful `data` payloads whenever
possible (to preserve partial results for the LLM), using:

- `status`: `ok|partial|failed`
- `issues`: array of `{ code, stage, message, retryable, uid?, message_id? }`
- `next_action`: `{ instruction, tool, arguments }`

Hard MCP errors are reserved for validation/precondition failures (for example:
invalid input, malformed ids, conflicting cursor state, write-gate disabled).

## Tool Set

### 1) `imap_list_accounts`

Purpose: list configured accounts without exposing secrets.

Input:
- none

Output `data`:
- `accounts`: array (max 50) of `{ account_id, host, port, secure }`
- `next_action`: `{ instruction, tool, arguments }` (recommended follow-up is `imap_list_mailboxes`)

### 2) `imap_list_mailboxes`

Purpose: list visible mailboxes/folders.

Input:
- `account_id` (optional)

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `next_action`: `{ instruction, tool, arguments }`
- `account_id`
- `mailboxes`: array (max 200) of `{ name, delimiter? }`

### 3) `imap_search_messages`

Purpose: search mailbox and return paginated message summaries.

Input:
- `account_id` (optional)
- `mailbox` (required)
- `cursor?` (string, opaque)
- search criteria fields:
  - `query?` (1..256)
  - `from?` (1..256)
  - `to?` (1..256)
  - `subject?` (1..256)
  - `unread_only?` (boolean)
  - `last_days?` (1..365)
  - `start_date?` (`YYYY-MM-DD`)
  - `end_date?` (`YYYY-MM-DD`)
- `limit` (optional)
- `include_snippet?` (boolean, default false)
- `snippet_max_chars?` (50..500, default 200; only valid if `include_snippet=true`)

Validation:
- When `cursor` is present, pagination resumes the stored cursor snapshot and ignores search criteria fields plus request-level snippet settings.
- `last_days` cannot be combined with `start_date`/`end_date`.
- `start_date <= end_date`.
- Search text fields and mailbox values must not contain ASCII control characters.
- Searches matching more than 20,000 messages are rejected; narrow filters and retry.

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `next_action`: `{ instruction, tool, arguments }`
- `account_id`
- `mailbox`
- `total` (integer)
- `attempted` (integer)
- `returned` (integer)
- `failed` (integer)
- `messages`: array (max 50) of:
  - `message_id`
  - `message_uri`
  - `message_raw_uri`
  - `mailbox`
  - `uidvalidity`
  - `uid`
  - `date?`
  - `from?`
  - `subject?`
  - `flags?` (string[])
  - `snippet?`
- `next_cursor?` (string)
- `has_more` (boolean)

### 4) `imap_get_message`

Purpose: return parsed message details with optional bounded enrichments.

Input:
- `account_id` (optional)
- `message_id` (required)
- `body_max_chars?` (100..20000, default 2000)
- `include_headers?` (boolean, default true)
- `include_all_headers?` (boolean, default false)
- `include_html?` (boolean, default false; returned HTML is sanitized)
- `extract_attachment_text?` (boolean, default false)
- `attachment_text_max_chars?` (100..50000, default 10000; only valid when extraction is enabled)

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `message`:
  - `message_id`
  - `message_uri`
  - `message_raw_uri`
  - `mailbox`
  - `uidvalidity`
  - `uid`
  - `date?`
  - `from?`
  - `to?`
  - `cc?`
  - `subject?`
  - `flags?`
  - `headers?` (curated by default; full when requested)
  - `body_text?` (bounded; prefers `text/plain`, otherwise derived from sanitized HTML when no meaningful plain-text body exists)
  - `body_html?` (sanitized and bounded)
  - `attachments?`: array (max 50) of:
    - `filename?`
    - `content_type`
    - `size_bytes`
    - `part_id`
    - `extracted_text?` (bounded)

PDF extraction rules:
- only `application/pdf`
- max attachment size for extraction: 5 MB
- extraction failures do not fail the whole tool call

### 5) `imap_get_message_raw`

Purpose: return bounded RFC822 source for diagnostics.

Input:
- `account_id` (optional)
- `message_id` (required)
- `max_bytes?` (1024..1000000, default 200000)

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `message_id`
- `message_uri`
- `message_raw_uri`
- `size_bytes`
- `raw_source_base64` (byte-faithful RFC822 source, base64 encoded)
- `raw_source_encoding` (`"base64"` on success)

### 6) `imap_apply_to_messages`

Purpose: apply one mutation action to a selected set of messages.

Write gate: requires `MAIL_IMAP_WRITE_ENABLED=true`.

Input:
- `account_id` (optional)
- `selector` (required), exactly one of:
  - `message_ids`: string[] (1..1000)
  - `search`: object with:
    - `mailbox` (required)
    - `cursor?` (string, opaque)
    - `query?` (1..256)
    - `from?` (1..256)
    - `to?` (1..256)
    - `subject?` (1..256)
    - `unread_only?` (boolean)
    - `last_days?` (1..365)
    - `start_date?` (`YYYY-MM-DD`)
    - `end_date?` (`YYYY-MM-DD`)
- `action` (required), one of:
  - `move` with `destination_mailbox`
  - `copy` with `destination_mailbox`, `destination_account_id?`
  - `delete`
  - `update_flags` with `add_flags?`, `remove_flags?`
- action-specific top-level fields:
  - `destination_mailbox?`
  - `destination_account_id?`
  - `add_flags?`
  - `remove_flags?`
- `max_messages` (optional, default `100`, range `1..1000`)
- `dry_run` (optional, default `false`)

Validation:
- selector must include exactly one of `message_ids` or `search`
- when `search.cursor` is present, search criteria replay is accepted and ignored
- `update_flags` requires at least one of `add_flags` or `remove_flags`
- `move` is same-account only
- the resolved selection must not exceed `max_messages`

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `action`: `move|copy|delete|update_flags`
- `dry_run` (boolean)
- `matched`: integer
- `attempted`: integer
- `succeeded`: integer
- `failed`: integer
- `results`: array of:
  - `message_id`
  - `status`: `planned|ok|partial|failed`
  - `issues`
  - `source_mailbox`
  - `destination_mailbox?`
  - `destination_account_id?`
  - `flags?`
  - `new_message_id?`

### 7) `imap_manage_mailbox`

Purpose: create, rename, or delete a mailbox.

Write gate: requires `MAIL_IMAP_WRITE_ENABLED=true`.

Input:
- `account_id` (optional)
- `action` (required): `create|rename|delete`
- `mailbox` (required)
- `destination_mailbox?` (required for `rename`, rejected for `create` and `delete`)

Behavior:
- `create` auto-creates missing parent mailboxes before the target mailbox
- `rename` is the mailbox move primitive and auto-creates missing destination parents
- `delete` is non-recursive and surfaces the server error for non-empty mailboxes or mailboxes with children

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `action`: `create|rename|delete`
- `mailbox`
- `destination_mailbox?`

## Security and Guardrails

- Never return secrets (`*_PASS`, tokens, cookies, auth headers).
- Redact secret-like keys in logs.
- Enforce all bounds before IMAP fetch/download when possible.
- Limit attachment bytes and text extraction output.
- Use TLS certificate and hostname verification by default.
- Reject ambiguous or conflicting inputs with explicit `invalid_input` errors.

## Environment Variables

Per account:

- `MAIL_IMAP_<ACCOUNT>_HOST` (required)
- `MAIL_IMAP_<ACCOUNT>_PORT` (default `993`)
- `MAIL_IMAP_<ACCOUNT>_SECURE` (default `true`)
- `MAIL_IMAP_<ACCOUNT>_USER` (required)
- `MAIL_IMAP_<ACCOUNT>_PASS` (required)

Server-wide:

- `MAIL_IMAP_WRITE_ENABLED` (default `false`)
- `MAIL_IMAP_CA_CERT_PATH` (optional PEM CA bundle; adds trusted roots without disabling hostname verification)
- `MAIL_IMAP_CONNECT_TIMEOUT_MS` (default `30000`)
- `MAIL_IMAP_GREETING_TIMEOUT_MS` (default `15000`)
- `MAIL_IMAP_SOCKET_TIMEOUT_MS` (default `300000`)

## Implementation Notes for Next Artifact

Next artifact will generate Rust types and schema definitions from this contract, then register all tools in an `rmcp` stdio server skeleton with unified error handling.
