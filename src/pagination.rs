//! Cursor-based pagination storage
//!
//! Manages grouped search cursors with TTL and LRU-ish eviction. A grouped
//! cursor stores the ordered mailbox list and its descending UID slices from
//! one search, so combined pagination can span several mailboxes without
//! losing per-mailbox UIDVALIDITY.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::mailbox_codec::normalize_mailbox_name;

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

/// One mailbox group stored in a grouped search cursor.
#[derive(Debug, Clone)]
pub struct SearchCursorGroup {
    /// Mailbox name as stored by the caller; bindings compare normalized forms.
    pub mailbox: String,
    /// Mailbox UIDVALIDITY observed when the search ran.
    pub uidvalidity: u32,
    /// Matching UIDs for this mailbox in descending order (newest first).
    pub uids_desc: Arc<[u32]>,
}

/// Grouped search cursor entry.
///
/// Captures the ordered mailbox groups, one combined offset, and the
/// presentation settings needed to serve subsequent pages.
#[derive(Debug, Clone)]
pub struct CursorEntry {
    /// Account identifier.
    pub account_id: String,
    /// Mailbox groups in requested order.
    pub groups: Arc<[SearchCursorGroup]>,
    /// Next unread position across all groups in group order.
    pub offset: usize,
    /// Preview character limit from the original search.
    pub preview_max_chars: Option<usize>,
    /// Conversation discovery method from the original search.
    pub thread_method: Option<crate::models::ThreadMethod>,
    /// Expiration timestamp (refreshed on successful claim).
    pub expires_at: Instant,
}

/// Failure to validate or atomically claim a search continuation page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchCursorClaimError {
    /// The cursor does not exist or its lifetime elapsed.
    MissingOrExpired,
    /// The cursor belongs to a different account.
    AccountMismatch,
    /// The requested mailbox list does not match the stored ordered list.
    MailboxesMismatch,
    /// The stored combined offset exceeds the stored UID count.
    OffsetOutOfRange,
}

/// One non-empty mailbox slice of a claimed search page.
#[derive(Debug, Clone)]
pub struct SearchPageGroup {
    /// Mailbox name for this slice.
    pub mailbox: String,
    /// Mailbox UIDVALIDITY observed when the search ran.
    pub uidvalidity: u32,
    /// Claimed UIDs in stored descending order.
    pub uids: Vec<u32>,
}

/// One claimed page of grouped search results.
#[derive(Debug, Clone)]
pub struct SearchCursorPage {
    /// Non-empty mailbox slices in requested order.
    pub groups: Vec<SearchPageGroup>,
    /// Total number of UIDs across every stored group.
    pub total: usize,
    /// Whether any UID remains unclaimed after this page.
    pub has_more: bool,
    /// Preview character limit stored with the cursor.
    pub preview_max_chars: Option<usize>,
    /// Conversation discovery method stored with the cursor.
    pub thread_method: Option<crate::models::ThreadMethod>,
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

    /// Validate that a cursor belongs to the account and ordered mailbox list.
    ///
    /// This does not claim a page, move the offset, or refresh expiry, so a
    /// continuation can be checked before network work begins.
    pub fn validate_binding(
        &mut self,
        id: &str,
        account_id: &str,
        mailboxes: &[String],
    ) -> Result<(), SearchCursorClaimError> {
        self.cleanup();
        // Stored entry selected by the opaque cursor identifier.
        let entry = self
            .entries
            .get(id)
            .ok_or(SearchCursorClaimError::MissingOrExpired)?;
        check_binding(entry, account_id, mailboxes)?;
        if entry.offset > total_group_uids(&entry.groups) {
            return Err(SearchCursorClaimError::OffsetOutOfRange);
        }
        Ok(())
    }

    /// Atomically validate, claim, and advance one search continuation page.
    ///
    /// The claimed page is a consumed attempt: the stored offset moves forward
    /// immediately, and a terminal page deletes the cursor. There is no rollback.
    pub fn claim_page(
        &mut self,
        id: &str,
        account_id: &str,
        mailboxes: &[String],
        limit: usize,
    ) -> Result<SearchCursorPage, SearchCursorClaimError> {
        self.cleanup();

        // Build the page, settings, and final offset while the entry is immutably borrowed.
        let (groups, total, end, preview_max_chars, thread_method) = {
            let entry = self
                .entries
                .get(id)
                .ok_or(SearchCursorClaimError::MissingOrExpired)?;
            check_binding(entry, account_id, mailboxes)?;
            // Combined UID count across every stored group.
            let total = total_group_uids(&entry.groups);
            if entry.offset > total {
                return Err(SearchCursorClaimError::OffsetOutOfRange);
            }

            // Number of UIDs claimed by this page, bounded by the remaining count.
            let page_size = limit.min(total - entry.offset);
            // Combined offset after this page; never exceeds the total UID count.
            let end = entry.offset + page_size;
            // Non-empty mailbox slices in requested group order.
            let mut groups = Vec::new();
            // Combined position of the current group in the overall UID sequence.
            let mut position = 0usize;

            for group in entry.groups.iter() {
                // Slice bounds translated from the combined offset into this group.
                let group_len = group.uids_desc.len();
                let start = entry.offset.saturating_sub(position);
                let finish = end.saturating_sub(position).min(group_len);
                if start < finish {
                    groups.push(SearchPageGroup {
                        mailbox: group.mailbox.clone(),
                        uidvalidity: group.uidvalidity,
                        uids: group.uids_desc[start..finish].to_vec(),
                    });
                }
                position = position.saturating_add(group_len);
            }

            (
                groups,
                total,
                end,
                entry.preview_max_chars,
                entry.thread_method,
            )
        };

        // Whether unclaimed UIDs remain after this page.
        let has_more = end < total;
        if has_more {
            if let Some(entry) = self.entries.get_mut(id) {
                entry.offset = end;
                entry.expires_at = self.clock.now() + self.ttl;
            }
        } else {
            self.entries.remove(id);
        }

        Ok(SearchCursorPage {
            groups,
            total,
            has_more,
            preview_max_chars,
            thread_method,
        })
    }

    /// Delete cursor
    ///
    /// Removes cursor from store. Silently ignores missing cursors.
    pub fn delete(&mut self, cursor: &str) {
        self.entries.remove(cursor);
    }

    /// Remove expired cursors
    ///
    /// Called internally before create, validate, and claim operations to
    /// ensure stale cursors don't accumulate.
    fn cleanup(&mut self) {
        let now = self.clock.now();
        self.entries.retain(|_, entry| entry.expires_at > now);
    }

    /// Evict cursors if exceeding max_entries
    ///
    /// Removes oldest entries (by expiration time) until under limit.
    /// Uses LRU-ish behavior since expiration is refreshed on a successful claim.
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

/// Total number of UIDs across every group in a cursor.
fn total_group_uids(groups: &[SearchCursorGroup]) -> usize {
    groups.iter().fold(0usize, |total, group| {
        total.saturating_add(group.uids_desc.len())
    })
}

/// Check the account and ordered mailbox identity of one stored cursor.
fn check_binding(
    entry: &CursorEntry,
    account_id: &str,
    mailboxes: &[String],
) -> Result<(), SearchCursorClaimError> {
    if entry.account_id != account_id {
        return Err(SearchCursorClaimError::AccountMismatch);
    }
    // Ordered mailbox identity after decoding and INBOX canonicalization.
    let matches = entry.groups.len() == mailboxes.len()
        && entry.groups.iter().zip(mailboxes).all(|(group, mailbox)| {
            normalize_mailbox_name(&group.mailbox) == normalize_mailbox_name(mailbox)
        });
    if !matches {
        return Err(SearchCursorClaimError::MailboxesMismatch);
    }
    Ok(())
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
        SearchCursorClaimError, SearchCursorGroup, SearchCursorPage,
    };
    use crate::models::{MailboxInfo, ThreadMethod};

    /// Build one stored mailbox group for tests.
    fn search_group(mailbox: &str, uidvalidity: u32, uids: &[u32]) -> SearchCursorGroup {
        SearchCursorGroup {
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uids_desc: Arc::<[u32]>::from(uids),
        }
    }

    /// Build a grouped cursor entry with the default account and no settings.
    fn cursor_entry(groups: Vec<SearchCursorGroup>) -> CursorEntry {
        CursorEntry {
            account_id: "default".to_owned(),
            groups: Arc::<[SearchCursorGroup]>::from(groups),
            offset: 0,
            preview_max_chars: None,
            thread_method: None,
            expires_at: Instant::now(),
        }
    }

    /// Build the ordered mailbox request list used by binding checks.
    fn requested_mailboxes(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Flatten a claimed page into combined UID order.
    fn page_uids(page: &SearchCursorPage) -> Vec<u32> {
        page.groups
            .iter()
            .flat_map(|group| group.uids.iter().copied())
            .collect()
    }

    /// Build the two-group fixture shared by the pagination tests.
    fn two_group_entry() -> CursorEntry {
        cursor_entry(vec![
            search_group("INBOX", 1, &[5, 4, 3]),
            search_group("Archive", 2, &[9, 8]),
        ])
    }

    /// Tests that a created cursor can claim its first page.
    #[test]
    fn create_and_claim_cursor() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let id = store.create(two_group_entry());
        let page = store
            .claim_page(
                &id,
                "default",
                &requested_mailboxes(&["INBOX", "Archive"]),
                2,
            )
            .expect("first page must be claimable");
        assert_eq!(page_uids(&page), [5, 4]);
        assert_eq!(page.total, 5);
        assert!(page.has_more);

        store.delete(&id);
        assert!(matches!(
            store.validate_binding(&id, "default", &requested_mailboxes(&["INBOX", "Archive"])),
            Err(SearchCursorClaimError::MissingOrExpired)
        ));
    }

    /// Tests that one page can span groups while omitting empty slices.
    #[test]
    fn page_slices_across_groups_in_order() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let mut entry = two_group_entry();
        entry.offset = 2;
        let id = store.create(entry);

        let page = store
            .claim_page(
                &id,
                "default",
                &requested_mailboxes(&["INBOX", "Archive"]),
                2,
            )
            .expect("spanning page must be claimable");

        assert_eq!(page.total, 5);
        assert!(page.has_more);
        assert_eq!(page.groups.len(), 2);
        assert_eq!(page.groups[0].mailbox, "INBOX");
        assert_eq!(page.groups[0].uidvalidity, 1);
        assert_eq!(page.groups[0].uids, [3]);
        assert_eq!(page.groups[1].mailbox, "Archive");
        assert_eq!(page.groups[1].uidvalidity, 2);
        assert_eq!(page.groups[1].uids, [9]);
    }

    /// Tests that sequential claims with different limits are disjoint and ordered.
    #[test]
    fn sequential_claims_cover_every_uid_once() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let id = store.create(two_group_entry());
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        let first = store
            .claim_page(&id, "default", &mailboxes, 1)
            .expect("first page must be claimable");
        let second = store
            .claim_page(&id, "default", &mailboxes, 3)
            .expect("second page must be claimable");
        let third = store
            .claim_page(&id, "default", &mailboxes, 2)
            .expect("third page must be claimable");

        assert_eq!(page_uids(&first), [5]);
        assert_eq!(page_uids(&second), [4, 3, 9]);
        assert_eq!(page_uids(&third), [8]);
        assert!(!third.has_more);
        assert!(matches!(
            store.claim_page(&id, "default", &mailboxes, 2),
            Err(SearchCursorClaimError::MissingOrExpired)
        ));
    }

    /// Tests that concurrent claims through a shared store are disjoint.
    #[tokio::test]
    async fn concurrent_search_claims_are_disjoint() {
        let (store, _) = CursorStore::new_with_manual_clock(60, 2);
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let id = store.lock().await.create(two_group_entry());

        let claim = |store: Arc<tokio::sync::Mutex<CursorStore>>, id: String| async move {
            store
                .lock()
                .await
                .claim_page(
                    &id,
                    "default",
                    &requested_mailboxes(&["INBOX", "Archive"]),
                    2,
                )
                .expect("concurrent page must be claimable")
        };
        let (left, right) = tokio::join!(
            claim(Arc::clone(&store), id.clone()),
            claim(Arc::clone(&store), id)
        );
        let mut uids = page_uids(&left)
            .into_iter()
            .chain(page_uids(&right))
            .collect::<Vec<_>>();
        uids.sort_unstable();

        assert_eq!(uids, [3, 4, 5, 9]);
    }

    /// Tests that account and mailbox binding failures do not consume a page.
    #[test]
    fn binding_mismatches_do_not_consume() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let id = store.create(two_group_entry());
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        assert!(matches!(
            store.claim_page(&id, "other", &mailboxes, 1),
            Err(SearchCursorClaimError::AccountMismatch)
        ));
        assert!(matches!(
            store.claim_page(&id, "default", &requested_mailboxes(&["INBOX"]), 1),
            Err(SearchCursorClaimError::MailboxesMismatch)
        ));
        assert!(matches!(
            store.claim_page(
                &id,
                "default",
                &requested_mailboxes(&["Archive", "INBOX"]),
                1
            ),
            Err(SearchCursorClaimError::MailboxesMismatch)
        ));
        assert!(matches!(
            store.validate_binding(
                &id,
                "default",
                &requested_mailboxes(&["INBOX", "Archive", "Sent"])
            ),
            Err(SearchCursorClaimError::MailboxesMismatch)
        ));

        let page = store
            .claim_page(&id, "default", &mailboxes, 1)
            .expect("matching binding must retain the first page");
        assert_eq!(page_uids(&page), [5]);
    }

    /// Tests that unknown and expired cursor identifiers are rejected.
    #[test]
    fn missing_and_expired_cursors_are_rejected() {
        let (mut store, offset_ms) = CursorStore::new_with_manual_clock(1, 10);
        let id = store.create(two_group_entry());
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        assert!(matches!(
            store.claim_page("unknown", "default", &mailboxes, 1),
            Err(SearchCursorClaimError::MissingOrExpired)
        ));

        advance_ms(&offset_ms, 1_100);
        assert!(matches!(
            store.claim_page(&id, "default", &mailboxes, 1),
            Err(SearchCursorClaimError::MissingOrExpired)
        ));
        assert!(matches!(
            store.validate_binding(&id, "default", &mailboxes),
            Err(SearchCursorClaimError::MissingOrExpired)
        ));
    }

    /// Tests that a stored offset beyond the combined UID count is rejected.
    #[test]
    fn offset_beyond_total_is_rejected() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let mut entry = two_group_entry();
        entry.offset = 6;
        let id = store.create(entry);
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        assert!(matches!(
            store.claim_page(&id, "default", &mailboxes, 1),
            Err(SearchCursorClaimError::OffsetOutOfRange)
        ));
        assert!(matches!(
            store.validate_binding(&id, "default", &mailboxes),
            Err(SearchCursorClaimError::OffsetOutOfRange)
        ));
    }

    /// Tests that an offset equal to the total yields an empty terminal page.
    #[test]
    fn offset_equal_to_total_is_terminal() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let mut entry = two_group_entry();
        entry.offset = 5;
        let id = store.create(entry);
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        let page = store
            .claim_page(&id, "default", &mailboxes, 1)
            .expect("terminal empty page must be claimable");
        assert!(page.groups.is_empty());
        assert_eq!(page.total, 5);
        assert!(!page.has_more);
        assert!(matches!(
            store.validate_binding(&id, "default", &mailboxes),
            Err(SearchCursorClaimError::MissingOrExpired)
        ));
    }

    /// Tests that a failed claim does not refresh the stored expiry.
    #[test]
    fn failed_claim_does_not_refresh_expiry() {
        let (mut store, offset_ms) = CursorStore::new_with_manual_clock(60, 10);
        let id = store.create(two_group_entry());
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        advance_ms(&offset_ms, 50_000);
        assert!(matches!(
            store.claim_page(&id, "other", &mailboxes, 1),
            Err(SearchCursorClaimError::AccountMismatch)
        ));
        advance_ms(&offset_ms, 10_001);
        assert!(matches!(
            store.claim_page(&id, "default", &mailboxes, 1),
            Err(SearchCursorClaimError::MissingOrExpired)
        ));
    }

    /// Tests that read-only validation does not refresh the stored expiry.
    #[test]
    fn validate_binding_does_not_refresh_expiry() {
        let (mut store, offset_ms) = CursorStore::new_with_manual_clock(60, 10);
        let id = store.create(two_group_entry());
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        advance_ms(&offset_ms, 50_000);
        store
            .validate_binding(&id, "default", &mailboxes)
            .expect("matching binding must validate");
        advance_ms(&offset_ms, 10_001);
        assert!(matches!(
            store.validate_binding(&id, "default", &mailboxes),
            Err(SearchCursorClaimError::MissingOrExpired)
        ));
    }

    /// Tests that a successful claim refreshes the stored expiry.
    #[test]
    fn successful_claim_refreshes_expiry() {
        let (mut store, offset_ms) = CursorStore::new_with_manual_clock(60, 10);
        let id = store.create(two_group_entry());
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        advance_ms(&offset_ms, 50_000);
        store
            .claim_page(&id, "default", &mailboxes, 1)
            .expect("first claim must succeed");
        advance_ms(&offset_ms, 20_000);
        store
            .claim_page(&id, "default", &mailboxes, 1)
            .expect("claim must succeed after refreshed expiry");
    }

    /// Tests that stored preview and thread settings return with every page.
    #[test]
    fn stored_settings_return_with_the_page() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let mut entry = two_group_entry();
        entry.preview_max_chars = Some(120);
        entry.thread_method = Some(ThreadMethod::Headers);
        let id = store.create(entry);
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        let page = store
            .claim_page(&id, "default", &mailboxes, 5)
            .expect("terminal page must be claimable");
        assert_eq!(page.preview_max_chars, Some(120));
        assert_eq!(page.thread_method, Some(ThreadMethod::Headers));
        assert!(!page.has_more);
    }

    /// Tests INBOX case folding and modified UTF-7 equivalence in bindings.
    #[test]
    fn inbox_and_encoded_mailboxes_match_normalized_bindings() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);

        let id = store.create(cursor_entry(vec![search_group("INBOX", 1, &[1])]));
        store
            .validate_binding(&id, "default", &requested_mailboxes(&["inbox"]))
            .expect("lowercase INBOX request must match");

        let id = store.create(cursor_entry(vec![search_group("&ZeVnLIqe-", 1, &[1])]));
        let page = store
            .claim_page(&id, "default", &requested_mailboxes(&["日本語"]), 1)
            .expect("decoded request must match encoded stored group");
        assert_eq!(page.groups[0].mailbox, "&ZeVnLIqe-");

        let id = store.create(cursor_entry(vec![search_group("日本語", 1, &[1])]));
        store
            .claim_page(&id, "default", &requested_mailboxes(&["&ZeVnLIqe-"]), 1)
            .expect("encoded request must match decoded stored group");
    }

    /// Tests that an empty middle group is skipped in claimed pages.
    #[test]
    fn empty_middle_group_is_skipped() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let entry = cursor_entry(vec![
            search_group("INBOX", 1, &[1, 2]),
            search_group("Empty", 2, &[]),
            search_group("Archive", 3, &[3, 4]),
        ]);
        let id = store.create(entry);
        let mailboxes = requested_mailboxes(&["INBOX", "Empty", "Archive"]);

        let first = store
            .claim_page(&id, "default", &mailboxes, 1)
            .expect("first page must be claimable");
        assert_eq!(first.groups.len(), 1);
        assert_eq!(first.groups[0].mailbox, "INBOX");

        store
            .claim_page(&id, "default", &mailboxes, 1)
            .expect("second page must be claimable");
        let third = store
            .claim_page(&id, "default", &mailboxes, 1)
            .expect("third page must skip the empty group");
        assert_eq!(third.groups.len(), 1);
        assert_eq!(third.groups[0].mailbox, "Archive");
        assert_eq!(third.groups[0].uids, [3]);
    }

    /// Tests that a limit larger than the total returns everything and deletes.
    #[test]
    fn oversized_limit_returns_all_and_deletes() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let id = store.create(two_group_entry());
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        let page = store
            .claim_page(&id, "default", &mailboxes, 100)
            .expect("oversized limit must be bounded by the total");
        assert_eq!(page_uids(&page), [5, 4, 3, 9, 8]);
        assert!(!page.has_more);
        assert!(matches!(
            store.claim_page(&id, "default", &mailboxes, 1),
            Err(SearchCursorClaimError::MissingOrExpired)
        ));
    }

    /// Tests that a maximum usize limit does not overflow.
    #[test]
    fn maximum_limit_does_not_overflow() {
        let (mut store, _) = CursorStore::new_with_manual_clock(60, 10);
        let id = store.create(two_group_entry());
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);

        let page = store
            .claim_page(&id, "default", &mailboxes, usize::MAX)
            .expect("maximum limit must be bounded by the total");
        assert_eq!(page_uids(&page), [5, 4, 3, 9, 8]);
        assert!(!page.has_more);
        assert!(matches!(
            store.validate_binding(&id, "default", &mailboxes),
            Err(SearchCursorClaimError::MissingOrExpired)
        ));
    }

    /// Tests that the store evicts the oldest cursors when exceeding max_entries.
    #[test]
    fn evicts_to_max_entries() {
        let (mut store, offset_ms) = CursorStore::new_with_manual_clock(60, 2);
        let mailboxes = requested_mailboxes(&["INBOX", "Archive"]);
        let id1 = store.create(two_group_entry());
        advance_ms(&offset_ms, 1);
        let id2 = store.create(two_group_entry());
        advance_ms(&offset_ms, 1);
        let id3 = store.create(two_group_entry());

        let remaining = [id1, id2, id3]
            .into_iter()
            .filter(|id| store.validate_binding(id, "default", &mailboxes).is_ok())
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
            total_messages: None,
            unread_messages: None,
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
