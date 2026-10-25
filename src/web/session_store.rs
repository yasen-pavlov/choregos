//! Bounded in-memory session store.
//!
//! `tower-sessions`' `MemoryStore` never removes expired records and has no size limit, so
//! anonymous login-form sessions could grow without bound. This store purges expired records,
//! keeps anonymous sessions short-lived, caps the number of sessions, and when full evicts the
//! least recently saved *anonymous* session; authenticated sessions are never evicted to make
//! room (new ones are rejected instead).

use std::{
    collections::HashMap,
    future::{Future, ready},
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
};

use tower_sessions::{
    SessionStore,
    cookie::time::{Duration, OffsetDateTime},
    session::{Id, Record},
    session_store::{Error, Result},
};

type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Clone, Debug)]
pub(crate) struct BoundedStore {
    inner: Arc<Mutex<Inner>>,
    /// Session data key whose presence marks an authenticated session.
    authenticated_key: &'static str,
    /// Expiry cap for anonymous sessions, applied on every save (so it acts as idle timeout).
    anonymous_ttl: Duration,
}

#[derive(Debug)]
struct Inner {
    capacity: usize,
    /// Monotonic save counter, used to find the least recently saved record.
    clock: u64,
    records: HashMap<Id, Entry>,
}

#[derive(Debug)]
struct Entry {
    record: Record,
    saved: u64,
}

impl BoundedStore {
    pub(crate) fn new(
        capacity: usize,
        authenticated_key: &'static str,
        anonymous_ttl: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                capacity,
                clock: 0,
                records: HashMap::new(),
            })),
            authenticated_key,
            anonymous_ttl,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().records.len()
    }

    fn insert(&self, record: &mut Record, new: bool) -> Result<()> {
        let mut inner = self.lock();
        let now = OffsetDateTime::now_utc();
        if new {
            while inner.records.contains_key(&record.id) {
                record.id = Id::default();
            }
        }
        if !inner.records.contains_key(&record.id) && inner.records.len() >= inner.capacity {
            inner
                .records
                .retain(|_, entry| entry.record.expiry_date > now);
            if inner.records.len() >= inner.capacity {
                let oldest_anonymous = inner
                    .records
                    .iter()
                    .filter(|(_, entry)| !entry.record.data.contains_key(self.authenticated_key))
                    .min_by_key(|(_, entry)| entry.saved)
                    .map(|(id, _)| *id);
                match oldest_anonymous {
                    Some(id) => {
                        inner.records.remove(&id);
                    }
                    None => return Err(Error::Backend("session store is full".to_owned())),
                }
            }
        }
        let mut stored = record.clone();
        if !stored.data.contains_key(self.authenticated_key) {
            stored.expiry_date = stored.expiry_date.min(now + self.anonymous_ttl);
        }
        inner.clock += 1;
        let saved = inner.clock;
        inner.records.insert(
            record.id,
            Entry {
                record: stored,
                saved,
            },
        );
        Ok(())
    }
}

// Implemented by hand (the trait is declared with `async_trait`) because every operation is
// synchronous; this avoids a direct `async-trait` dependency.
impl SessionStore for BoundedStore {
    fn create<'a, 'b, 'c>(&'a self, record: &'b mut Record) -> StoreFuture<'c, ()>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(ready(self.insert(record, true)))
    }

    fn save<'a, 'b, 'c>(&'a self, record: &'b Record) -> StoreFuture<'c, ()>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        let mut record = record.clone();
        Box::pin(ready(self.insert(&mut record, false)))
    }

    fn load<'a, 'b, 'c>(&'a self, id: &'b Id) -> StoreFuture<'c, Option<Record>>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        let mut inner = self.lock();
        let record = match inner.records.get(id) {
            Some(entry) if entry.record.expiry_date > OffsetDateTime::now_utc() => {
                Some(entry.record.clone())
            }
            Some(_) => {
                inner.records.remove(id);
                None
            }
            None => None,
        };
        Box::pin(ready(Ok(record)))
    }

    fn delete<'a, 'b, 'c>(&'a self, id: &'b Id) -> StoreFuture<'c, ()>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        self.lock().records.remove(id);
        Box::pin(ready(Ok(())))
    }
}

#[cfg(test)]
mod tests {
    use tower_sessions::cookie::time::Duration;

    use super::*;

    fn record(authenticated: bool, ttl: Duration) -> Record {
        let mut data = HashMap::new();
        if authenticated {
            data.insert("user".to_owned(), serde_json::json!("admin"));
        }
        Record {
            id: Id::default(),
            data,
            expiry_date: OffsetDateTime::now_utc() + ttl,
        }
    }

    fn bounded(capacity: usize) -> BoundedStore {
        BoundedStore::new(capacity, "user", Duration::minutes(15))
    }

    #[tokio::test]
    async fn full_store_evicts_oldest_anonymous_and_never_authenticated() {
        let store = bounded(3);
        let mut user = record(true, Duration::hours(1));
        store.create(&mut user).await.unwrap();
        let mut first = record(false, Duration::hours(1));
        store.create(&mut first).await.unwrap();
        let mut second = record(false, Duration::hours(1));
        store.create(&mut second).await.unwrap();
        // Touching `first` makes `second` the least recently saved anonymous session.
        store.save(&first).await.unwrap();
        let mut third = record(false, Duration::hours(1));
        store.create(&mut third).await.unwrap();
        assert_eq!(store.len(), 3);
        assert!(store.load(&second.id).await.unwrap().is_none());
        for kept in [&user, &first, &third] {
            assert!(store.load(&kept.id).await.unwrap().is_some());
        }

        let store = bounded(1);
        store
            .create(&mut record(true, Duration::hours(1)))
            .await
            .unwrap();
        assert!(
            store
                .create(&mut record(false, Duration::hours(1)))
                .await
                .is_err()
        );
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn anonymous_sessions_are_capped_to_the_short_ttl() {
        let store = bounded(4);
        let anonymous = record(false, Duration::days(7));
        let user = record(true, Duration::days(7));
        store.save(&anonymous).await.unwrap();
        store.save(&user).await.unwrap();
        let soon = OffsetDateTime::now_utc() + Duration::minutes(16);
        let loaded = store.load(&anonymous.id).await.unwrap().unwrap();
        assert!(loaded.expiry_date < soon);
        let loaded = store.load(&user.id).await.unwrap().unwrap();
        assert!(loaded.expiry_date > soon);
    }

    #[tokio::test]
    async fn expired_records_are_reclaimed() {
        let store = bounded(2);
        store
            .create(&mut record(true, Duration::seconds(-1)))
            .await
            .unwrap();
        store
            .create(&mut record(false, Duration::seconds(-1)))
            .await
            .unwrap();
        // Full of expired records: both are purged instead of evicting or rejecting.
        store
            .create(&mut record(true, Duration::hours(1)))
            .await
            .unwrap();
        assert_eq!(store.len(), 1);
        // An expired record is also dropped when looked up.
        let expiring = record(false, Duration::seconds(-1));
        store.save(&expiring).await.unwrap();
        assert!(store.load(&expiring.id).await.unwrap().is_none());
        assert_eq!(store.len(), 1);
    }
}
