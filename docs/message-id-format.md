# Message ID Format

The `message_id` is a stable, opaque string that encodes a message's location within an IMAP mailbox. It is designed to be stable across sessions as long as the mailbox's `UIDVALIDITY` remains unchanged.

## Format

```
imap:{account_id}:{mailbox}:{uidvalidity}:{uid}
```

## Components

| Component | Description | Example |
|-----------|-------------|---------|
| `imap` | Fixed prefix identifying the format | `imap` |
| `account_id` | Account identifier from configuration | `default`, `work`, `personal` |
| `mailbox` | IMAP mailbox/folder name | `INBOX`, `Archive`, `Projects:2026` |
| `uidvalidity` | IMAP UIDVALIDITY value (session identifier) | `12345` |
| `uid` | IMAP UID (message identifier) | `42` |

## Examples

```
imap:default:INBOX:12345:42                    → Account "default", INBOX, UIDVALIDITY 12345, UID 42
imap:work:Sent:67890:999                       → Account "work", Sent mailbox, UIDVALIDITY 67890, UID 999
imap:personal:Projects:2026:Q1:999:7           → Account "personal", mailbox "Projects:2026:Q1", UIDVALIDITY 999, UID 7
```

## Mailbox Names with Colons

Mailbox names containing colons are fully preserved in the format. The encoding is designed to handle such cases correctly.

For a mailbox named `Projects:2026:Q1`:
- The `message_id` would be: `imap:personal:Projects:2026:Q1:999:7`
- Parsing correctly extracts: account_id=`personal`, mailbox=`Projects:2026:Q1`, uidvalidity=`999`, uid=`7`

## Stability and UIDVALIDITY

The `message_id` is only stable while the mailbox's `UIDVALIDITY` remains unchanged. IMAP servers may change `UIDVALIDITY` when:
- Mailbox is deleted and recreated
- Server-side mailbox migration occurs
- Certain mailbox operations on the server

If `UIDVALIDITY` changes, existing `message_id`s for that mailbox become invalid. The server will return a conflict error:

```
Error: conflict: mailbox snapshot changed; rerun search
```

Solution: Rerun `imap_search_messages` to obtain fresh `message_id`s.

## Usage in Tools

The `message_id` is required by:
- `imap_get_message` - Fetch message details
- `imap_get_message_raw` - Fetch RFC822 source
- `imap_update_message_flags` - Modify flags
- `imap_copy_message` - Copy to another mailbox
- `imap_move_message` - Move to another mailbox
- `imap_delete_message` - Delete message

Always obtain `message_id`s from `imap_search_messages` output rather than constructing them manually.
