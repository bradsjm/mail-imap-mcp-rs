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

## Mailbox Names and INBOX

The server canonicalizes the reserved name `INBOX` case-insensitively to `INBOX`. It preserves the case of every other mailbox name.

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

The `message_id` is required by the ID-based message tools, and the encoded account component is used to infer scope:
- `imap_get_message` - Fetch message details
- `imap_get_message_raw` - Fetch RFC822 source
- `imap_get_attachment` - Read one decoded attachment
- `imap_apply_to_messages` - Apply `move`, `copy`, or `delete` to explicit messages
- `imap_update_message_flags` - Add, remove, or replace flags on explicit messages
- `imap_update_message_labels` - Add, remove, or replace Gmail labels on explicit messages

## Destination IDs

A successful `imap_apply_to_messages` copy or move can return `new_message_id` for each message. This ID uses the destination mailbox, the destination UIDVALIDITY reported by the server's `COPYUID` response, and the mapped destination UID.

`new_message_id` is absent when the server supplies no mapping. The server does not examine the destination mailbox to reconstruct the ID. A copy-based move can return a destination ID even when source deletion did not finish.

Gmail message and thread identifiers are separate decimal strings (`gmail_message_id`, `gmail_thread_id`). They are not `message_id` values.

Always obtain `message_id`s from `imap_search_messages` output rather than constructing them manually.
