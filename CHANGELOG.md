# Changelog

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
