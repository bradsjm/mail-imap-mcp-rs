//! Cursor-based pagination storage
//!
//! Manages search cursors with TTL and LRU eviction. Cursors encode
//! search state (UIDs, offset, filters) for efficient pagination
//! across large result sets.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use uuid::Uuid;

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
    pub uids_desc: Vec<u32>,
    /// Current offset into `uids_desc` (next page starts here)
    pub offset: usize,
    /// Whether snippet was included in original search
    pub include_snippet: bool,
    /// Snippet character limit from original search
    pub snippet_max_chars: usize,
    /// Expiration timestamp (refreshed on access)
    pub expires_at: Instant,
}

/// Cursor store with TTL and LRU eviction
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
        }
    }

    /// Create and store a new cursor
    ///
    /// Generates UUID for cursor, stores it with current expiration,
    /// and evicts old entries if necessary. Returns the cursor ID.
    pub fn create(&mut self, mut entry: CursorEntry) -> String {
        self.cleanup();
        entry.expires_at = Instant::now() + self.ttl;
        let id = Uuid::new_v4().to_string();
        self.entries.insert(id.clone(), entry);
        self.evict_if_needed();
        id
    }

    /// Retrieve cursor by ID
    ///
    /// Returns cloned entry if cursor exists and is not expired.
    /// Automatically cleans up expired entries before lookup.
    pub fn get(&mut self, cursor: &str) -> Option<CursorEntry> {
        self.cleanup();
        self.entries.get(cursor).cloned()
    }

    /// Update cursor offset (for next page)
    ///
    /// Moves cursor forward in search results. Refreshes expiration.
    /// Silently ignores missing cursors.
    pub fn update_offset(&mut self, cursor: &str, offset: usize) {
        if let Some(entry) = self.entries.get_mut(cursor) {
            entry.offset = offset;
            entry.expires_at = Instant::now() + self.ttl;
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
        let now = Instant::now();
        self.entries.retain(|_, entry| entry.expires_at > now);
    }

    /// Evict cursors if exceeding max_entries
    ///
    /// Removes oldest entries (by expiration time) until under limit.
    /// Uses LRU-ish behavior since expiration is refreshed on access.
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
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{CursorEntry, CursorStore};

    /// Create test cursor entry
    fn cursor_entry(expires_at: Instant) -> CursorEntry {
        CursorEntry {
            account_id: "default".to_owned(),
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uids_desc: vec![5, 4, 3, 2, 1],
            offset: 0,
            include_snippet: false,
            snippet_max_chars: 200,
            expires_at,
        }
    }

    #[test]
    fn create_and_get_cursor() {
        let mut store = CursorStore::new(60, 10);
        let id = store.create(cursor_entry(Instant::now()));
        let loaded = store.get(&id).expect("cursor must be present");
        assert_eq!(loaded.mailbox, "INBOX");
        assert_eq!(loaded.uids_desc.len(), 5);
    }

    #[test]
    fn update_offset_and_delete_cursor() {
        let mut store = CursorStore::new(60, 10);
        let id = store.create(cursor_entry(Instant::now()));
        store.update_offset(&id, 3);
        let loaded = store.get(&id).expect("cursor must exist after update");
        assert_eq!(loaded.offset, 3);

        store.delete(&id);
        assert!(store.get(&id).is_none());
    }

    #[test]
    fn expires_old_entries() {
        let mut store = CursorStore::new(1, 10);
        let id = store.create(cursor_entry(Instant::now()));
        thread::sleep(Duration::from_millis(1100));
        assert!(store.get(&id).is_none());
    }

    #[test]
    fn evicts_to_max_entries() {
        let mut store = CursorStore::new(60, 2);
        let id1 = store.create(cursor_entry(Instant::now()));
        let id2 = store.create(cursor_entry(Instant::now()));
        let id3 = store.create(cursor_entry(Instant::now()));

        let remaining = [id1, id2, id3]
            .into_iter()
            .filter(|id| store.get(id).is_some())
            .count();
        assert_eq!(remaining, 2);
    }
}
