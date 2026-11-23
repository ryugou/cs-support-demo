use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Result;

struct Entry<T> {
    stored_at: Instant,
    value: Arc<T>,
}

pub(crate) struct SchemaCache<T> {
    label: &'static str,
    ttl: Duration,
    entries: Mutex<HashMap<String, Entry<T>>>,
}

impl<T> SchemaCache<T> {
    pub(crate) fn new(label: &'static str, ttl: Duration) -> Self {
        Self {
            label,
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn get_or_fetch<F, Fut>(&self, schema: &str, fetch: F) -> Result<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        if let Some(fresh) = self.entry(schema, |age| age < self.ttl) {
            return Ok(fresh);
        }
        match fetch().await {
            Ok(value) => {
                let value = Arc::new(value);
                self.store(schema, Arc::clone(&value));
                Ok(value)
            }
            Err(err) => {
                let Some(stale) = self.entry(schema, |_| true) else {
                    return Err(err);
                };
                tracing::warn!(
                    schema,
                    cache = self.label,
                    error = ?err,
                    "{} refresh failed; serving the stale cached value instead of failing the request",
                    self.label
                );
                Ok(stale)
            }
        }
    }

    pub(crate) fn store(&self, schema: &str, value: Arc<T>) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                schema.to_owned(),
                Entry {
                    stored_at: Instant::now(),
                    value,
                },
            );
    }

    fn entry(&self, schema: &str, usable: impl Fn(Duration) -> bool) -> Option<Arc<T>> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .get(schema)
            .filter(|entry| usable(entry.stored_at.elapsed()))
            .map(|entry| Arc::clone(&entry.value))
    }
}

#[cfg(test)]
mod tests;
