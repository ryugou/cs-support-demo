use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;

use super::SchemaCache;
use crate::test_support::{capture_logs_async, filter_warn_and_error_lines};

const TEN_MINUTES: Duration = Duration::from_secs(600);

#[tokio::test]
async fn a_fresh_entry_is_served_without_fetching_or_warning() {
    let cache = SchemaCache::new("test values", TEN_MINUTES);
    cache.store("urtect", Arc::new(1));
    let fetched = Cell::new(false);
    let (value, logs) = capture_logs_async(cache.get_or_fetch("urtect", || async {
        fetched.set(true);
        Ok(2)
    }))
    .await;
    assert_eq!(*value.expect("a fresh entry is served"), 1);
    assert!(!fetched.get());
    assert!(filter_warn_and_error_lines(&logs).is_empty(), "{logs}");
}

#[tokio::test]
async fn a_fetched_value_is_stored_and_served_while_fresh() {
    let cache = SchemaCache::new("test values", TEN_MINUTES);
    let first = cache
        .get_or_fetch("urtect", || async { Ok(1) })
        .await
        .expect("the first fetch succeeds");
    let second = cache
        .get_or_fetch("urtect", || async { Err(anyhow!("vegapunk unavailable")) })
        .await
        .expect("the stored value is still fresh");
    assert_eq!((*first, *second), (1, 1));
}

#[tokio::test]
async fn an_expired_entry_is_fetched_again() {
    let cache = SchemaCache::new("test values", Duration::ZERO);
    cache.store("urtect", Arc::new(1));
    let value = cache
        .get_or_fetch("urtect", || async { Ok(2) })
        .await
        .expect("the refetch succeeds");
    assert_eq!(*value, 2);
}

#[tokio::test]
async fn a_stale_entry_is_served_with_a_warning_when_the_refresh_fails() {
    let cache = SchemaCache::new("test values", Duration::ZERO);
    cache.store("urtect", Arc::new(1));
    let (value, logs) = capture_logs_async(
        cache.get_or_fetch("urtect", || async { Err(anyhow!("vegapunk unavailable")) }),
    )
    .await;
    assert_eq!(*value.expect("the stale entry is served"), 1);
    let warnings = filter_warn_and_error_lines(&logs);
    assert!(
        warnings.contains("test values refresh failed"),
        "{warnings}"
    );
    assert!(warnings.contains("urtect"), "{warnings}");
    assert!(warnings.contains("vegapunk unavailable"), "{warnings}");
}

#[tokio::test]
async fn the_fetch_error_is_returned_when_the_schema_has_no_entry() {
    let cache = SchemaCache::new("test values", TEN_MINUTES);
    cache.store("other-project", Arc::new(1));
    let error = cache
        .get_or_fetch("urtect", || async { Err(anyhow!("vegapunk unavailable")) })
        .await
        .expect_err("another schema's entry is not served");
    assert_eq!(error.to_string(), "vegapunk unavailable");
}
