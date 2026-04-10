# Changelog

## [0.3.3]

### Added

- Added optional streamable HTTP transport via `--transport http`, including configurable bind address and port with the MCP endpoint served at `/mcp`.
- Added tracked write-operation polling and cancellation through `imap_get_operation` and `imap_cancel_operation`.
- Added configurable read-session caching for read tools with `MAIL_IMAP_READ_SESSION_CACHE_TTL_SECONDS` and `MAIL_IMAP_READ_SESSION_CACHE_MAX_PER_ACCOUNT`.
- Added Linux `arm64` (`aarch64-unknown-linux-gnu`) to the npm/native release matrix.
- Published concrete output schemas for every MCP tool so clients can rely on server-declared response shapes.

### Changed

- Refactored write operations to run as tracked backend operations with progress metadata and reusable polling instructions in tool responses.
- Tightened `imap_search_messages` so searches matching more than 1,000 messages are rejected instead of allowing up to 20,000.
- Tightened message fetch bounds by reducing `imap_get_message_raw.max_bytes` to `1..64000` with a default of `16000`, and aligning `imap_get_message` body and attachment extraction limits to the current runtime validation ranges.
- Hardened MCP schema publication and validation so public tool inputs stay client-safe and documented contracts match the current server payloads.

## [0.3.2]

### Added

- Added client-safe MCP input schema enforcement tests that validate every published tool schema and reject schema combinators that common MCP clients do not handle reliably.
- Expanded the GreenMail MCP inspector integration script into a contract-level test that checks tool discovery, parameter schema completeness, shared response envelope fields, and happy-path output shapes for all 8 MCP tools.

### Changed

- Refactored bulk writes so `imap_apply_to_messages` is id-only, added `imap_update_message_flags`, and batched message mutations by mailbox/UID set in the backend.
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
