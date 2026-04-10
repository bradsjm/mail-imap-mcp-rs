use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::imap;

#[derive(Debug)]
struct CachedSession<T> {
    session: T,
    last_used_at: Instant,
}

#[derive(Debug)]
pub(super) struct IdleSessionCache<T> {
    ttl: Duration,
    max_per_account: usize,
    sessions_by_account: BTreeMap<String, Vec<CachedSession<T>>>,
}

impl<T> IdleSessionCache<T> {
    pub(super) fn new(ttl: Duration, max_per_account: usize) -> Self {
        Self {
            ttl,
            max_per_account,
            sessions_by_account: BTreeMap::new(),
        }
    }

    pub(super) fn checkout(&mut self, account_id: &str, now: Instant) -> Option<T> {
        if self.max_per_account == 0 {
            return None;
        }
        self.prune_account(account_id, now);
        let session = self
            .sessions_by_account
            .get_mut(account_id)
            .and_then(|sessions| sessions.pop())
            .map(|cached| cached.session);
        self.remove_empty_bucket(account_id);
        session
    }

    pub(super) fn put_back(&mut self, account_id: &str, session: T, now: Instant) -> Option<T> {
        if self.max_per_account == 0 || self.ttl.is_zero() {
            return Some(session);
        }

        self.prune_account(account_id, now);
        let sessions = self
            .sessions_by_account
            .entry(account_id.to_owned())
            .or_default();
        if sessions.len() >= self.max_per_account {
            return Some(session);
        }

        sessions.push(CachedSession {
            session,
            last_used_at: now,
        });
        None
    }

    fn prune_account(&mut self, account_id: &str, now: Instant) {
        let ttl = self.ttl;
        let Some(sessions) = self.sessions_by_account.get_mut(account_id) else {
            return;
        };
        sessions.retain(|cached| now.duration_since(cached.last_used_at) < ttl);
    }

    fn remove_empty_bucket(&mut self, account_id: &str) {
        if self
            .sessions_by_account
            .get(account_id)
            .is_some_and(Vec::is_empty)
        {
            self.sessions_by_account.remove(account_id);
        }
    }
}

pub(super) type ReadSessionCache = Mutex<IdleSessionCache<imap::ImapSession>>;

pub(super) struct ReadSessionLease {
    account_id: String,
    session: Option<imap::ImapSession>,
}

impl ReadSessionLease {
    pub(super) fn new(account_id: String, session: imap::ImapSession) -> Self {
        Self {
            account_id,
            session: Some(session),
        }
    }

    pub(super) fn session(&mut self) -> &mut imap::ImapSession {
        self.session
            .as_mut()
            .expect("read session lease must contain a session")
    }

    pub(super) async fn finish(
        mut self,
        config: &crate::config::ServerConfig,
        cache: &ReadSessionCache,
        reusable: bool,
    ) -> Option<crate::errors::AppError> {
        let session = self.session.take()?;
        if !reusable {
            return imap::logout_session_best_effort(config, session)
                .await
                .err();
        }

        let evicted = {
            let mut cache = cache.lock().await;
            cache.put_back(&self.account_id, session, Instant::now())
        };
        match evicted {
            Some(session) => imap::logout_session_best_effort(config, session)
                .await
                .err(),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::IdleSessionCache;

    #[test]
    fn checkout_evicts_expired_entries() {
        let now = Instant::now();
        let mut cache = IdleSessionCache::new(Duration::from_secs(10), 2);
        assert!(cache.put_back("default", 1usize, now).is_none());

        let expired_at = now + Duration::from_secs(11);
        assert_eq!(cache.checkout("default", expired_at), None);
    }

    #[test]
    fn put_back_respects_capacity() {
        let now = Instant::now();
        let mut cache = IdleSessionCache::new(Duration::from_secs(10), 1);
        assert!(cache.put_back("default", 1usize, now).is_none());
        assert_eq!(cache.put_back("default", 2usize, now), Some(2));
    }

    #[test]
    fn zero_capacity_disables_caching() {
        let now = Instant::now();
        let mut cache = IdleSessionCache::new(Duration::from_secs(10), 0);
        assert_eq!(cache.put_back("default", 1usize, now), Some(1));
        assert_eq!(cache.checkout("default", now), None);
    }
}
