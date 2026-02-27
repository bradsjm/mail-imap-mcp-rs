# Cursor Pagination

Search results from `imap_search_messages` use cursor-based pagination for efficient navigation through large result sets.

## Pagination Metadata

Search responses include pagination metadata in the `data` field:

```json
{
  "summary": "Found 150 messages",
  "data": {
    "account_id": "default",
    "mailbox": "INBOX",
    "total": 150,
    "messages": [
      {
        "message_id": "imap:default:INBOX:12345:42",
        "date": "2024-02-26T10:30:00Z",
        "from": "sender@example.com",
        "subject": "Project update"
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
| `total` | integer | Total number of messages matching search criteria (up to 20,000) |
| `messages` | array | Current page of messages (max `limit`, default 10, max 50) |
| `next_cursor` | string? | Opaque cursor string for fetching next page; absent if no more results |
| `has_more` | boolean | `true` if additional pages available |

## Fetching Next Pages

To fetch the next page, pass the `cursor` parameter instead of search criteria:

```json
{
  "account_id": "default",
  "mailbox": "INBOX",
  "cursor": "550e8400-e29b-41d4-a716-446655440000"
}
```

**Important rules:**
- `cursor` cannot be combined with search criteria (`query`, `from`, `to`, `subject`, `unread_only`, `last_days`, `start_date`, `end_date`)
- Always pass the same `account_id` and `mailbox` used in the original search
- Cursors are opaque strings; do not attempt to parse or construct them

## Cursor Expiration

Cursors expire after 10 minutes by default. This is configurable via:

```bash
MAIL_IMAP_CURSOR_TTL_SECONDS=600
```

When a cursor expires, you'll receive:

```
Error: invalid input: cursor is invalid or expired
```

Solution: Rerun the original search without a cursor to obtain a fresh result set.

## Cursor Storage Limits

The server stores cursor data in-memory with configurable limits:

```bash
# Maximum number of cursor entries to store
MAIL_IMAP_CURSOR_MAX_ENTRIES=512
```

When the limit is reached, the oldest unused cursors are evicted first (LRU policy).

## Error Handling

| Error | Cause | Resolution |
|-------|-------|------------|
| `invalid input: cursor is invalid or expired` | Cursor expired, malformed, or evicted | Rerun original search |
| `invalid input: cursor cannot be combined with search criteria` | Both `cursor` and search fields provided | Use only `cursor` for pagination |
| `conflict: mailbox snapshot changed; rerun search` | UIDVALIDITY changed between pages | Rerun original search |

## Best Practices

1. **Process pages promptly**: Cursors expire after 10 minutes by default
2. **Don't reuse cursors**: Each cursor is for a specific page; always use the `next_cursor` from the previous response
3. **Handle `has_more`**: When `has_more` is `false`, there are no more pages
4. **Parallel processing**: Different searches generate independent cursors that can be processed in parallel
5. **Error recovery**: On any cursor-related error, rerun the original search from scratch
