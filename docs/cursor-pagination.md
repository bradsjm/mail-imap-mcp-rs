# Cursor Pagination

Search results from `imap_search_messages` use cursor-based pagination for efficient navigation through large result sets.

A search covers 1 to 10 mailboxes. Results are grouped in request order. One message can appear in several mailboxes; the server does not merge those entries.

## Pagination Metadata

Search responses include pagination metadata in the `data` field:

```json
{
  "summary": "Found 150 messages",
  "data": {
    "account_id": "default",
    "mailboxes": ["INBOX"],
    "total": 150,
    "attempted": 10,
    "returned": 10,
    "failed": 0,
    "messages": [
      {
        "message_id": "imap:default:INBOX:12345:42",
        "mailbox": "INBOX",
        "date": "2024-02-26T10:30:00Z",
        "from": "sender@example.com",
        "subject": "Project update",
        "preview": "The release is ready for review."
      }
      // ... up to `limit` messages
    ],
    "next_cursor": "550e8400-e29b-41d4-a716-446655440000",
    "has_more": true
  },
  "meta": {
    "now_utc": "2024-02-26T10:30:45.123Z",
    "duration_ms": 245
  }
}
```

## Fields

| Field | Type | Description |
|-------|------|-------------|
| `mailboxes` | string[] | Searched mailboxes in request order, with canonical names |
| `total` | integer? | Combined match count. Absent when a target mailbox search failed. `0` for a successful empty search. |
| `thread_method` | string? | `gmail` or `headers` when `thread_of_message_id` was used |
| `thread_possibly_incomplete` | boolean | `true` for header-based conversation discovery |
| `attempted` | integer | UIDs in the claimed page |
| `returned` | integer | Messages in this page |
| `failed` | integer | UIDs whose summaries could not be fetched |
| `messages` | array | Current page of messages (max `limit`, default 10, max 100) |
| `next_cursor` | string? | Opaque cursor string for fetching the next page; absent if no more results |
| `has_more` | boolean | `true` if additional pages are available |

Messages are returned group by group in the requested mailbox order. Within a group, UIDs are newest first.

## Fetching Next Pages

To fetch the next page, pass the same `account_id`, the same ordered `mailboxes` list, and the `cursor` parameter. Clients may resend the original search payload. The server resumes the stored snapshot and ignores replayed search filters and presentation options.

```json
{
  "account_id": "default",
  "mailboxes": ["INBOX"],
  "cursor": "550e8400-e29b-41d4-a716-446655440000",
  "limit": 10
}
```

**Important rules:**
- Always pass the same `account_id` and the same ordered `mailboxes` list that started the search.
- `INBOX` is case-insensitive. Other mailbox names keep their case.
- A continuation with a different account or mailbox list returns `invalid_input`. A mismatch does not extend the cursor lifetime.
- When `cursor` is present, `query`, `from`, `to`, `subject`, `unread_only`, `flagged`, `answered`, `keyword`, `message_id_header`, `gmail_query`, `thread_of_message_id`, `last_days`, `start_date`, `end_date`, and `preview_max_chars` are ignored. The stored UID snapshot already reflects the original filters and retains the presentation options.
- Replayed filter values still receive syntax validation. Control characters and injection-shaped input are rejected.
- `limit` applies to the page size for the resumed request.
- A claimed page is a consumed attempt. A failed fetch does not rewind the cursor offset.
- Cursors are opaque strings. Do not parse or construct them.

## Cursor Expiration

Cursors expire after 10 minutes by default. This is configurable via:

```bash
MAIL_IMAP_CURSOR_TTL_SECONDS=600
```

When a cursor expires, you receive:

```
Error: invalid input: cursor is invalid or expired
```

Solution: Rerun the original search without a cursor to obtain a fresh result set.

## Cursor Storage Limits

The server stores cursor data in memory with configurable limits:

```bash
# Maximum number of cursor entries to store
MAIL_IMAP_CURSOR_MAX_ENTRIES=512
```

When the limit is reached, the oldest unused cursors are evicted first (LRU policy).

A search snapshot stores at most 1,000 combined UIDs across all requested mailboxes.

## Error Handling

| Error | Cause | Resolution |
|-------|-------|------------|
| `invalid input: cursor is invalid or expired` | Cursor expired, malformed, or evicted | Rerun original search |
| `invalid input: cursor does not match account/mailboxes` | Different account or mailbox list | Use the original account and ordered list |
| `invalid input: cursor offset is out of range` | Stored snapshot is inconsistent | Rerun original search |
| `conflict: mailbox snapshot changed; rerun search` | UIDVALIDITY changed between pages | Rerun original search |

## Best Practices

1. **Process pages promptly**: Cursors expire after 10 minutes by default
2. **Don't reuse cursors**: Each cursor is for a specific page; always use the `next_cursor` from the previous response
3. **Handle `has_more`**: When `has_more` is `false`, there are no more pages
4. **Keep the mailbox order**: The cursor binds the ordered mailbox list
5. **Parallel processing**: Different searches generate independent cursors that can be processed in parallel
6. **Error recovery**: On any cursor-related error, rerun the original search from scratch
