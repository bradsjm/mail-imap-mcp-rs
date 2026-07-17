//! Cursor-based pagination storage
//!
//! Manages search cursors with TTL and LRU-ish eviction. Cursors encode
//! search state (UIDs, offset, filters) for efficient pagination
//! across large result sets.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

#[derive(Debug)]
enum CursorClock {
    System,
    #[cfg(test)]
    Manual {
        base: Instant,
        offset_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    },
}

impl CursorClock {
    fn now(&self) -> Instant {
        match self {
            Self::System => Instant::now(),
            #[cfg(test)]
            Self::Manual { base, offset_ms } => {
                *base + Duration::from_millis(offset_ms.load(std::sync::atomic::Ordering::Relaxed))
            }
        }
    }
}

/// Single cursor entry
///
/// Captures the full state of a search result page, allowing
/// subsequent pages to be fetched efficiently.
#[derive(Debug, Clone)]
pub struct CursorEntry {
    /// Account identifier
    pub account_id: String,
    /// Mailbox name
    pub mailbox: String,
    /// Mailbox UIDVALIDITY at time of search
    pub uidvalidity: u32,
    /// All matching UIDs in descending order (newest first)
    pub uids_desc: Arc<[u32]>,
    /// Current offset into `uids_desc` (next page starts here)
    pub offset: usize,
    /// Snippet character limit from original search. `None` means snippets are disabled.
    pub snippet_max_chars: Option<usize>,
    /// Expiration timestamp (refreshed on read/write access)
    pub expires_at: Instant,
}

/// Cursor store with TTL and LRU-ish eviction
///
/// Manages search cursors with automatic cleanup of expired entries
/// and eviction when `max_entries` is exceeded.
#[derive(Debug)]
pub struct CursorStore {
    /// Time-to-live for cursors (after which they expire)
    ttl: Duration,
    /// Maximum number of cursors to retain
    max_entries: usize,
    /// Active cursors keyed by UUID
    entries: HashMap<String, CursorEntry>,
    /// Time source for expiry calculations
    clock: CursorClock,
}

impl CursorStore {
    /// Create new cursor store
    ///
    /// # Parameters
    ///
    /// - `ttl_seconds`: Cursor lifetime in seconds (default 600)
    /// - `max_entries`: Maximum cursors to retain (default 512)
    pub fn new(ttl_seconds: u64, max_entries: usize) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_seconds),
            max_entries,
            entries: HashMap::new(),
            clock: CursorClock::System,
        }
    }

    #[cfg(test)]
    fn new_with_manual_clock(
        ttl_seconds: u64,
        max_entries: usize,
    ) -> (Self, std::sync::Arc<std::sync::atomic::AtomicU64>) {
        let offset_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        (
            Self {
                ttl: Duration::from_secs(ttl_seconds),
                max_entries,
                entries: HashMap::new(),
                clock: CursorClock::Manual {
                    base: Instant::now(),
                    offset_ms: offset_ms.clone(),
                },
            },
            offset_ms,
        )
    }

    /// Create and store a new cursor
    ///
    /// Generates UUID for cursor, stores it with current expiration,
    /// and evicts old entries if necessary. Returns the cursor ID.
    pub fn create(&mut self, mut entry: CursorEntry) -> String {
        self.cleanup();
        entry.expires_at = self.clock.now() + self.ttl;
        let id = Uuid::new_v4().to_string();
        self.entries.insert(id.clone(), entry);
        self.evict_if_needed();
        id
    }

    /// Retrieve cursor by ID
    ///
    /// Returns cloned entry if cursor exists and is not expired.
    /// Automatically cleans up expired entries before lookup.
    ///
    /// Refreshes cursor expiration on successful access.
    pub fn get(&mut self, cursor: &str) -> Option<CursorEntry> {
        self.cleanup();
        let entry = self.entries.get_mut(cursor)?;
        entry.expires_at = self.clock.now() + self.ttl;
        Some(entry.clone())
    }

    /// Update cursor offset (for next page)
    ///
    /// Moves cursor forward in search results. Refreshes expiration.
    /// Silently ignores missing cursors.
    pub fn update_offset(&mut self, cursor: &str, offset: usize) {
        if let Some(entry) = self.entries.get_mut(cursor) {
            entry.offset = offset;
            entry.expires_at = self.clock.now() + self.ttl;
        }
    }

    /// Delete cursor
    ///
    /// Removes cursor from store. Silently ignores missing cursors.
    pub fn delete(&mut self, cursor: &str) {
        self.entries.remove(cursor);
    }

    /// Remove expired cursors
    ///
    /// Called internally before get/create/update operations to
    /// ensure stale cursors don't accumulate.
    fn cleanup(&mut self) {
        let now = self.clock.now();
        self.entries.retain(|_, entry| entry.expires_at > now);
    }

    /// Evict cursors if exceeding max_entries
    ///
    /// Removes oldest entries (by expiration time) until under limit.
    /// Uses LRU-ish behavior since expiration is refreshed on get/update.
    fn evict_if_needed(&mut self) {
        if self.entries.len() <= self.max_entries {
            return;
        }

        let overflow = self.entries.len() - self.max_entries;
        let mut ids_by_expiry: Vec<(String, Instant)> = self
            .entries
            .iter()
            .map(|(id, entry)| (id.clone(), entry.expires_at))
            .collect();
        ids_by_expiry.sort_by_key(|(_, expires_at)| *expires_at);

        for (id, _) in ids_by_expiry.into_iter().take(overflow) {
            self.entries.remove(&id);
        }
    }
}

/// Snapshot-backed mailbox-list cursor.
///
/// A cursor binds a mailbox snapshot to an account and retains the response
/// metadata required to distinguish a complete list from a bounded one.
#[derive(Debug, Clone)]
pub struct MailboxCursorEntry {
    /// Account whose mailbox list produced this snapshot.
    pub account_id: String,
    /// Bounded mailbox snapshot in server LIST order.
    pub mailboxes: Arc<[crate::models::MailboxInfo]>,
    /// Next unread index in `mailboxes`.
    pub offset: usize,
    /// Exact source count when known; absent for a truncated LIST.
    pub total: Option<usize>,
    /// Whether the LIST observation cap hid additional mailboxes.
    pub truncated: bool,
    /// Expiration timestamp, refreshed on access.
    pub expires_at: Instant,
}

/// Failure to atomically claim a mailbox-list continuation page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxCursorClaimError {
    /// The cursor does not exist or its lifetime elapsed.
    MissingOrExpired,
    /// The cursor belongs to a different account.
    AccountMismatch,
    /// The stored continuation offset exceeds the snapshot length.
    OffsetOutOfRange,
}

/// One page claimed from a mailbox-list snapshot.
#[derive(Debug, Clone)]
pub struct MailboxCursorPage {
    /// Mailboxes in stable snapshot order.
    pub mailboxes: Vec<crate::models::MailboxInfo>,
    /// Exact source count when known.
    pub total: Option<usize>,
    /// Whether the source LIST observation was truncated.
    pub truncated: bool,
    /// Whether another page remains available under the same cursor.
    pub has_more: bool,
}

/// Cursor store for mailbox-list snapshots.
///
/// This is intentionally distinct from message-search cursors so snapshot
/// lifetimes and eviction cannot cross-contaminate unrelated operations.
#[derive(Debug)]
pub struct MailboxCursorStore {
    ttl: Duration,
    max_entries: usize,
    entries: HashMap<String, MailboxCursorEntry>,
    clock: CursorClock,
}

impl MailboxCursorStore {
    /// Create a mailbox cursor store with the supplied TTL and entry ceiling.
    pub fn new(ttl_seconds: u64, max_entries: usize) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_seconds),
            max_entries,
            entries: HashMap::new(),
            clock: CursorClock::System,
        }
    }

    #[cfg(test)]
    fn new_with_manual_clock(
        ttl_seconds: u64,
        max_entries: usize,
    ) -> (Self, std::sync::Arc<std::sync::atomic::AtomicU64>) {
        let offset_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        (
            Self {
                ttl: Duration::from_secs(ttl_seconds),
                max_entries,
                entries: HashMap::new(),
                clock: CursorClock::Manual {
                    base: Instant::now(),
                    offset_ms: offset_ms.clone(),
                },
            },
            offset_ms,
        )
    }

    /// Store a snapshot and return its opaque cursor identifier.
    pub fn create(&mut self, mut entry: MailboxCursorEntry) -> String {
        self.cleanup();
        entry.expires_at = self.clock.now() + self.ttl;
        let id = Uuid::new_v4().to_string();
        self.entries.insert(id.clone(), entry);
        self.evict_if_needed();
        id
    }

    /// Atomically validate, claim, and advance one continuation page.
    pub fn claim_page(
        &mut self,
        cursor: &str,
        account_id: &str,
        limit: usize,
    ) -> Result<MailboxCursorPage, MailboxCursorClaimError> {
        self.cleanup();

        let entry = self
            .entries
            .get(cursor)
            .ok_or(MailboxCursorClaimError::MissingOrExpired)?;
        if entry.account_id != account_id {
            return Err(MailboxCursorClaimError::AccountMismatch);
        }
        if entry.offset > entry.mailboxes.len() {
            return Err(MailboxCursorClaimError::OffsetOutOfRange);
        }

        let end = entry
            .offset
            .saturating_add(limit)
            .min(entry.mailboxes.len());
        let page = MailboxCursorPage {
            mailboxes: entry.mailboxes[entry.offset..end].to_vec(),
            total: entry.total,
            truncated: entry.truncated,
            has_more: end < entry.mailboxes.len(),
        };

        if page.has_more {
            let entry = self
                .entries
                .get_mut(cursor)
                .expect("validated mailbox cursor must remain present");
            entry.offset = end;
            entry.expires_at = self.clock.now() + self.ttl;
        } else {
            self.entries.remove(cursor);
        }

        Ok(page)
    }

    fn cleanup(&mut self) {
        let now = self.clock.now();
        self.entries.retain(|_, entry| entry.expires_at > now);
    }

    fn evict_if_needed(&mut self) {
        if self.entries.len() <= self.max_entries {
            return;
        }

        let overflow = self.entries.len() - self.max_entries;
        let mut ids_by_expiry: Vec<(String, Instant)> = self
            .entries
            .iter()
            .map(|(id, entry)| (id.clone(), entry.expires_at))
            .collect();
        ids_by_expiry.sort_by_key(|(_, expires_at)| *expires_at);
        for (id, _) in ids_by_expiry.into_iter().take(overflow) {
            self.entries.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use super::{
        CursorEntry, CursorStore, MailboxCursorClaimError, MailboxCursorEntry, MailboxCursorStore,
    };
    use crate::models::MailboxInfo;
    /// Creates a test cursor entry with the given expiration time.
    ///
    /// This helper is used to generate consistent test data for cursor store tests.
    fn cursor_entry(expires_at: Instant) -> CursorEntry {
        CursorEntry {
            account_id: "default".to_owned(),
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uids_desc: vec![5, 4, 3, 2, 1].into(),
            offset: 0,
            snippet_max_chars: None,
            expires_at,
        }
    }

    /// Tests that a cursor can be created and then retrieved from the store.
    #[test]
    fn create_and_get_cursor() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let id = store.create(cursor_entry(Instant::now()));
        let loaded = store.get(&id).expect("cursor must be present");
        assert_eq!(loaded.mailbox, "INBOX");
        assert_eq!(loaded.uids_desc.len(), 5);
    }

    /// Tests updating the offset of a cursor and then deleting it.
    ///
    /// Verifies that the offset is updated and that deletion removes the cursor.
    #[test]
    fn update_offset_and_delete_cursor() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let id = store.create(cursor_entry(Instant::now()));
        store.update_offset(&id, 3);
        let loaded = store.get(&id).expect("cursor must exist after update");
        assert_eq!(loaded.offset, 3);

        store.delete(&id);
        assert!(store.get(&id).is_none());
    }

    /// Tests that cursors expire after their TTL has elapsed.
    #[test]
    fn expires_old_entries() {
        let (mut store, offset_ms) = CursorStore::new_with_manual_clock(1, 10);
        let id = store.create(cursor_entry(Instant::now()));
        advance_ms(&offset_ms, 1_100);
        assert!(store.get(&id).is_none());
    }

    /// Tests that accessing a cursor refreshes its TTL, preventing expiration.
    #[test]
    fn get_refreshes_cursor_ttl() {
        let (mut store, offset_ms) = CursorStore::new_with_manual_clock(1, 10);
        let id = store.create(cursor_entry(Instant::now()));

        advance_ms(&offset_ms, 700);
        assert!(store.get(&id).is_some());

        advance_ms(&offset_ms, 700);
        assert!(store.get(&id).is_some());
    }

    /// Tests that the store evicts the oldest cursors when exceeding max_entries.
    #[test]
    fn evicts_to_max_entries() {
        let (mut store, offset_ms) = CursorStore::new_with_manual_clock(60, 2);
        let id1 = store.create(cursor_entry(Instant::now()));
        advance_ms(&offset_ms, 1);
        let id2 = store.create(cursor_entry(Instant::now()));
        advance_ms(&offset_ms, 1);
        let id3 = store.create(cursor_entry(Instant::now()));

        let remaining = [id1, id2, id3]
            .into_iter()
            .filter(|id| store.get(id).is_some())
            .count();
        assert_eq!(remaining, 2);
    }

    fn mailbox(name: &str) -> MailboxInfo {
        MailboxInfo {
            name: name.to_owned(),
            delimiter: Some("/".to_owned()),
            attributes: vec![],
            role: None,
            selectable: true,
            provider_managed: false,
        }
    }

    fn mailbox_cursor_entry(expires_at: Instant) -> MailboxCursorEntry {
        MailboxCursorEntry {
            account_id: "account-a".to_owned(),
            mailboxes: ["INBOX", "Archive", "Sent"]
                .into_iter()
                .map(mailbox)
                .collect::<Vec<_>>()
                .into(),
            offset: 0,
            total: Some(3),
            truncated: false,
            expires_at,
        }
    }

    #[test]
    fn mailbox_cursor_claims_first_and_terminal_pages_then_deletes() {
        let (mut store, _) = MailboxCursorStore::new_with_manual_clock(60, 2);
        let id = store.create(mailbox_cursor_entry(Instant::now()));

        let first = store
            .claim_page(&id, "account-a", 2)
            .expect("first page must be claimable");
        assert_eq!(
            first
                .mailboxes
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["INBOX", "Archive"]
        );
        assert_eq!(first.total, Some(3));
        assert!(!first.truncated);
        assert!(first.has_more);

        let terminal = store
            .claim_page(&id, "account-a", 2)
            .expect("terminal page must be claimable");
        assert_eq!(terminal.mailboxes[0].name, "Sent");
        assert!(!terminal.has_more);
        assert!(matches!(
            store.claim_page(&id, "account-a", 2),
            Err(MailboxCursorClaimError::MissingOrExpired)
        ));
    }

    #[test]
    fn mailbox_cursor_account_mismatch_does_not_consume() {
        let (mut store, _) = MailboxCursorStore::new_with_manual_clock(60, 2);
        let id = store.create(mailbox_cursor_entry(Instant::now()));

        assert!(matches!(
            store.claim_page(&id, "account-b", 1),
            Err(MailboxCursorClaimError::AccountMismatch)
        ));
        let page = store
            .claim_page(&id, "account-a", 1)
            .expect("matching account must retain the first page");
        assert_eq!(page.mailboxes[0].name, "INBOX");
    }

    #[test]
    fn mailbox_cursor_out_of_range_does_not_consume() {
        let (mut store, _) = MailboxCursorStore::new_with_manual_clock(60, 2);
        let mut entry = mailbox_cursor_entry(Instant::now());
        entry.offset = entry.mailboxes.len() + 1;
        let id = store.create(entry);

        assert!(matches!(
            store.claim_page(&id, "account-a", 1),
            Err(MailboxCursorClaimError::OffsetOutOfRange)
        ));
        assert!(matches!(
            store.claim_page(&id, "account-a", 1),
            Err(MailboxCursorClaimError::OffsetOutOfRange)
        ));
    }

    #[test]
    fn mailbox_cursor_expiry_is_missing_or_expired() {
        let (mut store, offset_ms) = MailboxCursorStore::new_with_manual_clock(1, 2);
        let id = store.create(mailbox_cursor_entry(Instant::now()));
        advance_ms(&offset_ms, 1_100);

        assert!(matches!(
            store.claim_page(&id, "account-a", 1),
            Err(MailboxCursorClaimError::MissingOrExpired)
        ));
    }

    #[tokio::test]
    async fn concurrent_mailbox_claims_are_disjoint_and_stable() {
        let (store, _) = MailboxCursorStore::new_with_manual_clock(60, 2);
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let id = store
            .lock()
            .await
            .create(mailbox_cursor_entry(Instant::now()));

        let claim = |store: Arc<tokio::sync::Mutex<MailboxCursorStore>>, id: String| async move {
            store
                .lock()
                .await
                .claim_page(&id, "account-a", 2)
                .expect("concurrent page must be claimable")
        };
        let (left, right) = tokio::join!(
            claim(Arc::clone(&store), id.clone()),
            claim(Arc::clone(&store), id)
        );
        let mut names = left
            .mailboxes
            .into_iter()
            .chain(right.mailboxes)
            .map(|item| item.name)
            .collect::<Vec<_>>();
        names.sort();

        assert_eq!(names, ["Archive", "INBOX", "Sent"]);
    }
    fn advance_ms(offset_ms: &Arc<AtomicU64>, amount: u64) {
        offset_ms.fetch_add(amount, Ordering::Relaxed);
    }
}
