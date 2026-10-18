# MCP Tool Contract

This document defines the server-facing MCP contract for `mail-imap-mcp-rs`.
It is the source of truth for tool names, input/output shapes, validation bounds, and safety rules.

## Design Decisions

- Transport: stdio by default, with optional streamable HTTP transport at `/mcp`.
- Auth/config: environment variables only.
- Message locator: stable `message_id` format `imap:{account_id}:{mailbox}:{uidvalidity}:{uid}`.
- Destructive/write operations: disabled by default and explicitly gated.
- Output style: concise summaries with bounded structured data.
- Compatibility: no backward compatibility requirement with earlier implementations.
- MCP input schemas must remain client-safe: plain object properties only, with conditional rules enforced at runtime rather than schema unions.
- Tool count: 12.
- Gmail interoperability: implemented from the IMAP extensions and unverified against a live Gmail account in this repository.

## Shared Input Types

### `account_id`

- Type: string
- Pattern: `^[A-Za-z0-9_-]{1,64}$`
- Default: `"default"`

### `mailbox`

- Type: string
- Length: 1..256

The server canonicalizes the reserved name `INBOX` case-insensitively to `INBOX`.
It preserves the case of every other mailbox name.

### `message_id`

- Type: string
- Format: `imap:{account_id}:{mailbox}:{uidvalidity}:{uid}`
- Validation rules:
  - Prefix must be `imap`.
  - `uidvalidity` and `uid` must be non-negative integers.
  - Parsed `account_id` must match requested account.

### `limit` (message search)

- Type: integer
- Range: 1..100
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
- `cursor?` (string, opaque account-bound continuation token)
- `include_counts?` (boolean, default `false`)
- `limit?` (integer, 1..200, default 50)

Validation:
- `limit` is 1..200 when `include_counts=false`.
- `limit` is 1..50 when `include_counts=true`. An explicit value above 50 is invalid.
- When `cursor` is present, the tool resumes the stored mailbox snapshot for the
  same account. The cursor is opaque and cannot be replayed against another
  account.
- A continuation without counts does not use the network.

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `next_action`: `{ instruction, tool, arguments }` (when another page is available, recommends `imap_list_mailboxes` with `account_id`, `cursor`, `limit`, and count mode when requested)
- `account_id`
- `returned` (integer)
- `total?` (integer; omitted when the server's complete mailbox count is unavailable)
- `has_more` (boolean)
- `next_cursor?` (string, opaque; present when `has_more=true`)
- `truncated` (boolean)
- `mailboxes`: array (max 200) of:
  - `name`
  - `delimiter?`
  - `attributes` (string[])
  - `role?` (`inbox|all|archive|drafts|flagged|important|junk|sent|trash`)
  - `selectable` (boolean)
  - `provider_managed` (boolean)
  - `total_messages?` (integer; only when `include_counts=true` and the count is known)
  - `unread_messages?` (integer; only when `include_counts=true` and the count is known)

Count rules:
- Counts apply only to selectable mailboxes on the returned page.
- The server does not store live counts in the cursor.
- An observed zero is a known value and is returned as `0`.
- A missing or failed count is absent, never invented as zero.
- A count failure keeps the mailbox page and pagination. The response uses
  `status=partial` with an issue that names the count stage.

### 3) `imap_search_messages`

Purpose: search 1 to 10 mailboxes and return paginated message summaries.

Input:
- `account_id` (optional)
- `mailboxes` (required): string[] (1..10 names, each 1..256 characters). Results are grouped in request order.
- `cursor?` (string, opaque)
- search criteria fields:
  - `query?` (1..256)
  - `from?` (1..256)
  - `to?` (1..256)
  - `subject?` (1..256)
  - `unread_only?` (boolean)
  - `flagged?` (boolean; `true` keeps flagged messages, `false` keeps unflagged messages)
  - `answered?` (boolean; `true` keeps answered messages, `false` keeps unanswered messages)
  - `keyword?` (1..64 characters; a custom IMAP atom without a leading backslash)
  - `message_id_header?` (1..256; matches Message-ID header text)
  - `gmail_query?` (1..256; Gmail raw search expression)
  - `thread_of_message_id?` (stable message id of the source conversation message)
  - `last_days?` (1..365)
  - `start_date?` (`YYYY-MM-DD`)
  - `end_date?` (`YYYY-MM-DD`)
- `limit?` (1..100, default 10)
- `preview_max_chars?` (50..500; when present, previews are returned up to this length)

Validation:
- `mailboxes` must contain 1..10 distinct canonical names. `INBOX` is case-insensitive for this check. Duplicates are rejected; the server never merges messages across mailboxes.
- The combined match count across all mailboxes must not exceed 1,000.
- `last_days` cannot be combined with `start_date`/`end_date`.
- `start_date <= end_date`.
- Search text fields and mailbox values must not contain ASCII control characters.
- `keyword` must be a custom atom. It must not be a backslash system flag and must not contain spaces, quotes, brackets, or wildcards.
- `gmail_query` is valid only on a new search against a server that advertises `X-GM-EXT-1`.
- On a new search, `thread_of_message_id` must belong to the searched account. The server verifies the source mailbox UIDVALIDITY.
- When `cursor` is present, the server validates the account and the ordered mailbox list. Replayed search filters and presentation options are ignored semantically, but their syntax is still validated.
- Searches matching more than 1,000 messages are rejected; narrow filters and retry.

Conversation discovery:
- All ordinary filters AND with the conversation criterion.
- On Gmail, the server requires the source message's numeric `X-GM-THRID` and searches with `X-GM-THRID N`. Output `thread_method="gmail"`.
- On other servers, the server reads Message-ID, In-Reply-To, and References, keeps at most 20 unique IDs, and searches the three headers with nested `OR HEADER` criteria. Output `thread_method="headers"`.
- Header search is candidate discovery. The server verifies candidate headers with exact, case-sensitive parsed-ID intersection before it builds the snapshot. This verification uses one additional metadata fetch. A candidate header that cannot be parsed provides no membership evidence.
- On header-based discovery, `thread_possibly_incomplete=true`. This value describes discovery semantics; it does not guarantee that related messages exist in the selected mailboxes.
- If no usable source ID remains, the tool returns `invalid_input` with `message has no usable Message-ID, References, or In-Reply-To; thread lookup is unavailable`.
- If a source header is too large, unparseable, or omitted at the 20-ID cap, the source lookup adds one non-retryable issue with stage `thread_discovery`.
- The search scope remains the explicit `mailboxes` list. For an account-wide Gmail search, pass the mailbox that `imap_list_mailboxes` reports with `role="all"`. The server does not assume the English `[Gmail]/All Mail` name.

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `next_action`: `{ instruction, tool, arguments }`
- `account_id`
- `mailboxes` (searched mailboxes in request order, with canonical names)
- `total?` (integer):
  - omitted when a target mailbox search failed before the snapshot was complete;
  - `0` for a successful empty search;
  - the stored snapshot total on later pages, including pages with fetch failures.
- `thread_method?` (`gmail|headers`; present when `thread_of_message_id` was used)
- `thread_possibly_incomplete` (boolean; `true` for header-based discovery)
- `attempted` (integer; UIDs in the claimed page)
- `returned` (integer)
- `failed` (integer; UIDs whose summaries could not be fetched)
- `messages`: array (max 100) of:
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
  - `preview?` (string; whitespace-normalized body text, bounded by `preview_max_chars`)
  - `gmail_labels?` (string[]; exactly the labels the server returned)
  - `gmail_message_id?` (decimal string; `X-GM-MSGID`)
  - `gmail_thread_id?` (decimal string; `X-GM-THRID`)
- `next_cursor?` (string)
- `has_more` (boolean)

Summary rules:
- The server sends one Summary metadata fetch per participating mailbox group. It does not send one body fetch per hit.
- A missing or oversized body keeps the header summary and adds a preview issue.
- A zero configured fetch budget sends no zero-length body query and returns a preview-budget issue.
- An empty meaningful body returns no `preview`.
- Gmail fields are absent or null on non-Gmail servers. A missing requested Gmail attribute produces a partial-result issue and is never replaced by an invented empty value.
- Gmail message and thread IDs are decimal strings. Values above 2^53 remain exact in JSON.
- Messages are not deduplicated across mailboxes. One message can appear in several Gmail label folders.

Pagination rules:
- A claimed page is a consumed attempt, including failed fetches. The cursor offset does not rewind.
- A cursor binds the account and the ordered, canonical mailbox list.
- The cursor stores the matching UID snapshot, `preview_max_chars`, and thread method. A continuation uses that stored snapshot and those presentation settings.
- A continuation with a different account or mailbox list returns `invalid_input`. A mismatch does not extend the cursor lifetime.
- If a mailbox's UIDVALIDITY changed, the server deletes the cursor and returns the existing conflict error.

### 4) `imap_get_message`

Purpose: return parsed message details with optional bounded enrichments.

Input:
- `message_id` (required)
- `body_max_chars?` (1..16000, default 2000)
- `body_mode?` (`text|html|both`, default `text`)
- `include_headers?` (boolean, default true)
- `include_all_headers?` (boolean, default false)
- `attachment_mode?` (`none|metadata|extract_text`, default `metadata`)
- `attachment_text_max_chars?` (1..64000, default 10000; only valid when `attachment_mode=extract_text`)

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
  - `gmail_labels?` (string[]; exactly the labels the server returned)
  - `gmail_message_id?` (decimal string; `X-GM-MSGID`)
  - `gmail_thread_id?` (decimal string; `X-GM-THRID`)
  - `headers?` (curated by default; full when requested)
  - `body_text?` (bounded; returned for `body_mode=text|both`; prefers `text/plain`, otherwise derived from sanitized HTML when no meaningful plain-text body exists)
  - `body_html?` (sanitized and bounded; returned for `body_mode=html|both`)
  - `attachments?`: array (max 50) of:
    - `filename?`
    - `content_type`
    - `size_bytes?` (complete decoded payload size when known; `null`/absent when unavailable or incomplete; unknown size is never represented as zero)
    - `part_id`
    - `extracted_text?` (bounded; only when `attachment_mode=extract_text`)

Gmail metadata rules:
- The server checks capabilities once for the request and uses one State metadata fetch for flags and Gmail fields.
- Gmail fields are absent or null on non-Gmail servers.
- A requested Gmail attribute that the server omits produces a partial-result issue. The field stays absent.
- Gmail message and thread IDs are decimal strings. Values above 2^53 remain exact in JSON.

PDF extraction rules:
- only `application/pdf`
- max attachment size for extraction: 5 MB
- extraction failures do not fail the whole tool call
- `attachment_mode=metadata` reports attachment metadata without attempting extraction
- messages with more than 50 attachments return the first 50 plus a truncation issue

Message processing is bounded by the server-wide fetch, decode, MIME-complexity, and attachment-extraction limits listed under [Environment Variables](#environment-variables). A conforming IMAP server is asked for at most the configured raw-message fetch budget. Oversized messages use that bounded raw prefix and return `partial` status with a fetch-budget issue. MIME depth or part-count overflow returns header-only metadata rather than recursively parsing the overflowing structure. Incomplete messages never extract attachment text, and an unknown or incomplete attachment `size_bytes` remains `null`/absent rather than zero.

### 5) `imap_get_message_raw`

Purpose: return a bounded RFC822 byte range for diagnostics.

Input:
- `message_id` (required)
- `max_bytes?` (1..64000, default 16000)
- `offset_bytes?` (integer, default 0)

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `message_id`
- `message_uri`
- `message_raw_uri`
- `total_size_bytes`
- `returned_bytes`
- `offset_bytes`
- `truncated`
- `raw_source_base64` (byte-faithful RFC822 source, base64 encoded)
- `raw_source_encoding` (`"base64"` on success)

### 6) `imap_get_attachment`

Purpose: read one decoded attachment as bounded text or bounded bytes.

Input:
- `message_id` (required)
- `part_id` (required; 1..64 characters)
- `mode?` (`text|bytes`, default `text`)
- `max_chars?` (integer, 1..64000, default 10000; valid only when `mode=text`)
- `offset_bytes?` (integer, default 0; valid only when `mode=bytes`)
- `max_bytes?` (integer, 1..64000, default 16000; valid only when `mode=bytes`)

Part IDs use the existing MIME numbering contract:
- The root part is `1`.
- Each child index is positive and one-based, separated by dots (for example `1.2`).
- The server resolves the part by walking the parsed MIME tree. It does not send synthetic part IDs as IMAP sections.
- The target must be a leaf part with attachment disposition or filename, using the same predicate as `imap_get_message`.
- A missing part returns `not_found`. A malformed ID or a multipart/body target returns `invalid_input`.

Validation:
- Reject mode-inapplicable fields instead of ignoring them.
- Text mode accepts only `max_chars`.
- Bytes mode accepts only `offset_bytes` and `max_bytes`.
- Offset equal to the decoded size returns an empty final page. A greater offset is invalid.

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `message_id`
- `part_id`
- `attachment?` (present only when the complete attachment was read; the status is then `ok` or `partial`):
  - `filename?`
  - `content_type` (lowercase MIME type)
  - `size_bytes` (integer; complete decoded payload size)
  - `content` (tagged by `format`):
    - text: `{ "format": "text", "text": string, "truncated": boolean }`
    - bytes: `{ "format": "base64", "content_base64": string, "offset_bytes": integer, "returned_bytes": integer, "has_more": boolean }`
- `next_action?`: `{ instruction, tool, arguments }`

Content rules:
- Byte offsets refer to decoded attachment bytes.
- The bytes end position is `min(saturating_add(offset_bytes, max_bytes), size_bytes)`.
- A continuation copies `mode`, `max_bytes`, `message_id`, `part_id`, and the next offset.
- Only a complete decoded attachment can populate `size_bytes`, byte pagination, and presentation-only text truncation.
- The bounded raw prefix length is not the attachment size and cannot define EOF or continuation.
- Incomplete or zero-budget content returns `limit_exceeded` with no payload in either mode.
- If bounded transfer decoding reports an incomplete payload, the tool returns `limit_exceeded` with no payload in either mode.
- Charset conversion or extraction-budget failures return no text payload.

Text mode:
- Supports `text/*`, `application/json`, `application/xml`, `application/ics`, and PDF.
- Sanitizes HTML before text conversion.
- Reuses PDF extraction and its configured extraction budget.
- Transfer-decodes the selected part once, then converts text without a second transfer decode.
- Truncates presentation output to `max_chars` and sets `truncated=true` when text remains.
- An unsupported type or extraction failure returns a structured issue and an `imap_get_attachment` next action with `mode="bytes"`. The tool never switches payload modes silently.

Bytes mode:
- Returns base64 over decoded attachment bytes.
- Never runs text or PDF extraction.

### 7) `imap_apply_to_messages`

Purpose: apply one mutation action to explicit messages.

Write gate: requires `MAIL_IMAP_WRITE_ENABLED=true`.

Input:
- `message_ids` (required): string[] (`1..250`)
- `action` (required): `move|copy|delete`
- `destination_mailbox?` (required for `move` and `copy`, rejected for `delete`)

Validation:
- message discovery happens in read tools such as `imap_search_messages`
- duplicate `message_ids` are deduplicated before execution
- all message ids must belong to the same account inferred from their IDs
- destination mailbox must already exist for `move` and `copy`
- live mailbox `uidvalidity` is revalidated before the operation is accepted
- `delete` requires `UIDPLUS`
- `move` requires native `MOVE` or `UIDPLUS` (copy-based move)
- `copy` requires neither capability
- The server checks these capabilities in preflight and again on the execution session before mutation.
- A missing required capability returns `conflict` with `server lacks UIDPLUS; delete and copy-based move are unavailable without safe UID EXPUNGE`.
- The server never falls back to plain `EXPUNGE`, changes delete into a Trash move, or manipulates unrelated deleted flags.

Output `data`:
- `status`: `accepted|running|ok|partial|failed|canceled`
- `issues`: array of diagnostic issues
- `operation`: `{ operation_id, kind, state, done, cancel_supported, created_at, started_at?, finished_at?, progress }`
- `result?`: final completed payload when `done=true`
- `next_action?`: polling instruction for `imap_get_operation` when `done=false`

Completed bulk result payload:
- `status`
- `issues`
- `account_id`
- `action?`
- `operation?`
- `matched`
- `attempted`
- `succeeded`
- `partial` (messages applied but not fully verified or identified)
- `failed`
- `results`: array of:
  - `message_id`
  - `status` (`ok|partial|failed`)
  - `issues`
  - `source_mailbox`
  - `destination_mailbox?`
  - `flags?`
  - `labels?`
  - `new_message_id?`

Destination identity rules:
- `new_message_id` is assembled from the destination mailbox, the destination UIDVALIDITY reported by `COPYUID`, and the mapped destination UID.
- `new_message_id` is absent when the server supplies no mapping. Mutation success does not imply identity discovery.
- The server does not examine the destination mailbox to reconstruct this identity.
- A malformed or incomplete mapping clears the mapping and adds a non-retryable issue with stage `copyuid_mapping`. Do not guess missing entries.
- A valid mapping is kept if a later copy-based move phase fails. The copied destination exists even if source deletion did not finish.
- When a command error follows a known mapping, mapped messages are `partial` with their destination ID. Unmapped messages carry the command issue and no destination ID.

Write outcome rules:
- A tagged rejection before any earlier applied phase is `failed`.
- A successful COPY or STORE followed by a failed later phase, such as a rejected `UID EXPUNGE` or missing property verification, is `partial`, never `ok`.
- A timeout or disconnection after a mutation was sent is `partial` with a non-retryable issue. The application outcome is unknown and must be checked before another write.
- The server discards the connection and never replays the mutation.
- The aggregate result is `ok` without issues, `partial` when any message was applied or partially applied, and `failed` otherwise.

### 8) `imap_update_message_flags`

Purpose: add, remove, or replace flags on explicit messages.

Write gate: requires `MAIL_IMAP_WRITE_ENABLED=true`.

Input:
- `message_ids` (required): string[] (`1..250`)
- `operation` (required): `add|remove|replace`
- `flags` (required): string[] (`1..32`)

Validation:
- duplicate `message_ids` are deduplicated before execution
- all message ids must belong to the same account inferred from their IDs
- valid standard flags are `\Seen`, `\Answered`, `\Flagged`, `\Deleted`, `\Draft`
- custom keywords may also be allowed if they are valid IMAP atoms and the server accepts them
- invalid standard flags return a corrective hint listing the valid standard flags
- live mailbox `uidvalidity` is revalidated before the operation is accepted

Output `data`:
- `status`: `accepted|running|ok|partial|failed|canceled`
- `issues`: array of diagnostic issues
- `operation`: `{ operation_id, kind, state, done, cancel_supported, created_at, started_at?, finished_at?, progress }`
- `result?`: final completed payload when `done=true`
- `next_action?`: polling instruction for `imap_get_operation` when `done=false`

Property result rules:
- The completed result uses the bulk result payload defined for `imap_apply_to_messages`.
- `flags` holds the flags observed by one post-STORE State fetch.
- `labels` is absent for flag updates.
- A successful STORE followed by a missing or failed flag verification is `partial` with a non-retryable issue that states the STORE completed.
- The `partial` counter counts those messages. Any partial message keeps the aggregate and the operation state `partial`.

### 9) `imap_update_message_labels`

Purpose: add, remove, or replace Gmail labels on explicit messages.

Write gate: requires `MAIL_IMAP_WRITE_ENABLED=true`.
Server requirement: the server must advertise `X-GM-EXT-1`.

Input:
- `message_ids` (required): string[] (`1..250`)
- `operation` (required): `add|remove|replace`
- `labels` (required): string[] (`0..32`; empty is allowed only for `replace`)

Validation:
- add and remove require 1..32 labels.
- replace permits 0..32 labels so the model can clear the label set deliberately.
- each label is 1..256 Unicode characters without ASCII control characters.
- duplicates are removed while preserving input order.
- The server validates every label before it chooses the wire representation.
- duplicate `message_ids` are deduplicated before execution.
- all message ids must belong to the same account inferred from their IDs.
- live mailbox `uidvalidity` is revalidated before the operation is accepted.
- Gmail support is checked in preflight and again on the execution session before any STORE.
- A server without `X-GM-EXT-1` returns `invalid_input` with `Gmail labels require a server that advertises X-GM-EXT-1`.

Wire rules:
- System labels that match `\[A-Za-z]+` are sent as atoms.
- Custom labels use modified UTF-7 and quoted-string escaping.
- Commands use exactly `+X-GM-LABELS (...)`, `-X-GM-LABELS (...)`, or `X-GM-LABELS (...)`.
- Empty replace sends `X-GM-LABELS ()`.
- Replace means the complete label set accepted by the server, including writable system labels. The server does not silently preserve a subset.
- The STORE is non-silent so the server returns observed labels.
- Returned labels are accepted only after a tagged OK.
- The server does not send a post-STORE label FETCH. Removing the selected folder's label can invalidate the original folder UID.

Output `data`:
- `status`: `accepted|running|ok|partial|failed|canceled`
- `issues`: array of diagnostic issues
- `operation`: `{ operation_id, kind, state, done, cancel_supported, created_at, started_at?, finished_at?, progress }`
- `result?`: final completed payload when `done=true`
- `next_action?`: polling instruction for `imap_get_operation` when `done=false`

Property result rules:
- The completed result uses the bulk result payload defined for `imap_apply_to_messages`.
- `labels` holds the labels returned by the STORE response.
- `flags` is absent for label updates.
- A tagged STORE rejection is `failed`.
- A successful STORE with missing returned labels is `partial` with a non-retryable issue. The server does not guess labels with a cross-folder lookup.
- An observed empty label set is a known value and is returned as an empty array.

### 10) `imap_manage_mailbox`

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
- `status`: `accepted|running|ok|partial|failed|canceled`
- `issues`: array of diagnostic issues
- `operation`: `{ operation_id, kind, state, done, cancel_supported, created_at, started_at?, finished_at?, progress }`
- `result?`: final completed payload when `done=true`
- `next_action?`: polling instruction for `imap_get_operation` when `done=false`

### 11) `imap_get_operation`

Purpose: poll a previously accepted write operation.

Input:
- `operation_id` (required)
- `include_result?` (optional boolean, default `false`)

Output `data`:
- `status`: `accepted|running|ok|partial|failed|canceled`
- `issues`: array of currently accumulated diagnostic issues
- `operation`: `{ operation_id, kind, state, done, cancel_supported, created_at, started_at?, finished_at?, progress }`
- `result?`: final completed payload when `done=true` and `include_result=true`
- `next_action?`: polling instruction for `imap_get_operation` when `done=false`; if the operation is already complete and a result exists but `include_result=false`, `next_action` points to `imap_get_operation` with `include_result=true`

### 12) `imap_cancel_operation`

Purpose: request cancellation for a running write operation.

Input:
- `operation_id` (required)

Output `data`:
- `status`: `accepted|running|ok|partial|failed|canceled`
- `issues`: array of currently accumulated diagnostic issues
- `operation`: `{ operation_id, kind, state, done, cancel_supported, created_at, started_at?, finished_at?, progress }`
- `result?`: final completed payload when `done=true`
- `next_action?`: polling instruction for `imap_get_operation` when `done=false`

## Security and Guardrails

- Never return secrets (`*_PASS`, tokens, cookies, auth headers).
- Redact secret-like keys in logs.
- Enforce all bounds before IMAP fetch/download when possible.
- Limit attachment bytes and text extraction output.
- Use TLS certificate and hostname verification by default.
- Reject ambiguous or conflicting inputs with explicit `invalid_input` errors.
- Never send another IMAP command on a connection after a command error.
- Require `UIDPLUS` before delete and before a copy-based move.
- Never replay a mutation after an uncertain outcome.

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
- `MAIL_IMAP_READ_SESSION_CACHE_TTL_SECONDS` (default `120`)
- `MAIL_IMAP_READ_SESSION_CACHE_MAX_PER_ACCOUNT` (default `4`; set `0` to disable read-session caching)
- `MAIL_IMAP_OPERATION_MAX_ENTRIES` (default `256`; completed write operations retained in memory)
- `MAIL_IMAP_MESSAGE_FETCH_BUDGET_BYTES` (default `8388608`; maximum bytes fetched while assembling one message or attachment; exhaustion returns bounded partial content or `limit_exceeded` and an issue)
- `MAIL_IMAP_MESSAGE_DECODE_BUDGET_BYTES` (default `16777216`; maximum decoded message payload bytes; exhaustion returns bounded partial content and an issue)
- `MAIL_IMAP_MIME_MAX_DEPTH` (default `32`; exceeding the MIME depth ceiling returns header-only metadata with a diagnostic issue)
- `MAIL_IMAP_MIME_MAX_PARTS` (default `250`; exceeding the MIME part-count ceiling returns header-only metadata with a diagnostic issue)
- `MAIL_IMAP_ATTACHMENT_EXTRACT_BUDGET_BYTES` (default `10485760`; maximum complete decoded attachment payload bytes processed for text extraction per message; attachments that do not fit remain metadata-only and are reported with an issue)
