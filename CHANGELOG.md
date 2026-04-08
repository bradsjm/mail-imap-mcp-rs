# Changelog

## [0.3.2]

### Added

- Added client-safe MCP input schema enforcement tests that validate every published tool schema and reject schema combinators that common MCP clients do not handle reliably.
- Expanded the GreenMail MCP inspector integration script into a contract-level test that checks tool discovery, parameter schema completeness, shared response envelope fields, and happy-path output shapes for all 7 MCP tools.

### Changed

- Flattened the public input contracts for `imap_apply_to_messages` and `imap_manage_mailbox` so they publish client-safe schemas while preserving the existing tool names and runtime validation rules.
- Synced the documented MCP contract with the current server payloads, including URI fields on message-bearing tools and the flattened write-tool argument shapes.

## [0.3.0]

### Added

- Added `imap_apply_to_messages` to apply `move`, `copy`, `delete`, or `update_flags` to explicit message ids or search-selected messages.
- Added `imap_manage_mailbox` to create, rename, and delete mailboxes, including automatic parent mailbox creation for create and rename operations.

### Changed

- Reduced the MCP surface to 7 tools by consolidating single-message write operations into `imap_apply_to_messages` and replacing mailbox lifecycle gaps with `imap_manage_mailbox`.
- Removed `imap_verify_account` from the public surface and dropped redundant `confirm` parameters from destructive operations.

## [0.2.2]

### Added

- Added HTML-to-text fallback for `imap_get_message` so HTML-only messages still return `body_text`.
- Added MIME parsing regression tests covering HTML-only bodies and whitespace-only `text/plain` alternatives.
- Added search validation regression tests covering cursor pagination requests that replay the original filter payload.

### Changed

- `body_text` now prefers meaningful `text/plain` content and otherwise derives text from sanitized HTML.
- `imap_search_messages` now treats `cursor` as authoritative and ignores replayed search criteria on paginated requests while still honoring `limit`, `account_id`, and `mailbox`.

## [0.2.1]

### Added

- Added IMAP modified UTF-7 mailbox decoding for `imap_list_mailboxes`.
- Added compatibility-aware mailbox command encoding so legacy encoded mailbox names in older `message_id` values and cursors continue to work after upgrade.
- Added regression tests covering mailbox codec behavior, encoded/decoded cursor compatibility, and raw message fetch read-state preservation.

### Changed

- Switched full-message fetches from `RFC822` to `BODY.PEEK[]` for better iCloud IMAP compatibility and to avoid implicitly setting `\\Seen`.

### Fixed

- Fixed cursor resume to treat encoded and decoded mailbox forms as the same mailbox identity.
- Fixed mailbox operations on non-ASCII folders by re-encoding outbound mailbox arguments for IMAP commands.
- Preserved raw message fidelity by returning byte-faithful base64-encoded message source in tool responses.
