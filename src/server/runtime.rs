//! Process-scoped state shared by all MCP server sessions.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};

use crate::config::ServerConfig;
use crate::pagination::{CursorStore, MailboxCursorStore};

use super::session_cache::{IdleSessionCache, ReadSessionCache};
use super::types::StoredOperation;

/// Maximum number of MIME parsing jobs that may run concurrently.
const MIME_PARSE_CONCURRENCY: usize = 4;

/// Shared mutable state that must outlive an individual MCP transport session.
///
/// Each transport creates one runtime for its session factory, so cursors,
/// operations, cached IMAP sessions, account write serialization, and the
/// MIME parsing concurrency budget are process-scoped.
pub(crate) struct MailImapRuntime {
    pub(super) config: Arc<ServerConfig>,
    pub(super) cursors: Arc<Mutex<CursorStore>>,
    pub(super) mailbox_cursors: Arc<Mutex<MailboxCursorStore>>,
    pub(super) read_sessions: Arc<ReadSessionCache>,
    pub(super) operations: Arc<Mutex<BTreeMap<String, StoredOperation>>>,
    pub(super) account_write_locks: Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>,
    pub(super) mime_parse_semaphore: Arc<Semaphore>,
}

impl MailImapRuntime {
    pub(crate) fn new(config: ServerConfig) -> Self {
        let cursors = CursorStore::new(config.cursor_ttl_seconds, config.cursor_max_entries);
        let mailbox_cursors =
            MailboxCursorStore::new(config.cursor_ttl_seconds, config.cursor_max_entries);
        let read_sessions = IdleSessionCache::new(
            Duration::from_secs(config.read_session_cache_ttl_seconds),
            config.read_session_cache_max_per_account,
        );

        Self {
            config: Arc::new(config),
            cursors: Arc::new(Mutex::new(cursors)),
            mailbox_cursors: Arc::new(Mutex::new(mailbox_cursors)),
            read_sessions: Arc::new(Mutex::new(read_sessions)),
            operations: Arc::new(Mutex::new(BTreeMap::new())),
            account_write_locks: Arc::new(Mutex::new(BTreeMap::new())),
            mime_parse_semaphore: Arc::new(Semaphore::new(MIME_PARSE_CONCURRENCY)),
        }
    }
}
