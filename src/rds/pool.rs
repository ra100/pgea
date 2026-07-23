//! Per-(target, profile) pool of RDS Data API clients.
//!
//! Building an `AwsRdsClient` means resolving AWS credentials (SSO/env/IMDS)
//! and constructing an `aws_sdk_rdsdata::Client` — real work worth avoiding
//! on every new pg connection to the same target. Entries are keyed by
//! everything that determines a distinct client's identity: the target's
//! ARNs/database/region plus the resolved profile.
//!
//! Entries expire after `ttl` so a profile whose SSO session goes stale
//! after first use isn't served forever from a cached client — the next
//! connection past the TTL window re-resolves credentials from scratch,
//! same as the pre-pool per-connection behavior.
//!
//! This module has no AWS-specific knowledge on purpose: the caller supplies
//! a `build` closure that does the actual credential resolution + SDK
//! client construction, so the cache itself stays trivially unit-testable
//! with a fake builder instead of needing real AWS credentials in CI.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::config::Target;
use crate::rds::RdsClient;

const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PoolKey {
    cluster_arn: String,
    secret_arn: String,
    database: String,
    region: String,
    profile: Option<String>,
}

struct PoolEntry {
    client: Arc<dyn RdsClient>,
    created_at: Instant,
}

/// Caches one `Arc<dyn RdsClient>` per distinct (target, profile) pair.
pub struct RdsClientPool {
    entries: Mutex<HashMap<PoolKey, PoolEntry>>,
    ttl: Duration,
}

impl Default for RdsClientPool {
    fn default() -> Self {
        Self::new(DEFAULT_TTL)
    }
}

impl RdsClientPool {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    #[cfg(test)]
    async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    /// Return a cached client for this (target, profile) if one exists and
    /// hasn't expired; otherwise run `build` and cache the result.
    ///
    /// `build` runs *outside* the map lock: holding the mutex across the
    /// AWS SDK's credential-resolution await would serialize every
    /// concurrent connection — including ones targeting a different
    /// cluster entirely — behind whichever one happens to be building.
    /// Two connections racing past an expired/missing entry can both build
    /// and the second insert simply wins; that thundering-herd window is
    /// narrow (only right after TTL expiry or cold start) and cheaper than
    /// the coordination needed to avoid it.
    pub async fn get_or_build<F, Fut>(
        &self,
        target: &Target,
        profile: Option<&str>,
        build: F,
    ) -> Arc<dyn RdsClient>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Arc<dyn RdsClient>>,
    {
        let key = PoolKey {
            cluster_arn: target.cluster_arn.clone(),
            secret_arn: target.secret_arn.clone(),
            database: target.database.clone(),
            region: target.region.clone(),
            profile: profile.map(str::to_owned),
        };

        {
            let entries = self.entries.lock().await;
            if let Some(entry) = entries.get(&key) {
                if entry.created_at.elapsed() < self.ttl {
                    return entry.client.clone();
                }
            }
        }

        let client = build().await;

        let mut entries = self.entries.lock().await;
        // Sweep other expired entries while we already hold the lock, so a
        // client that reconnects with a different profile override each
        // time (the pg `password` field is a free-form profile override,
        // see module docs) can't grow this map unbounded for the life of
        // the listener — only keys that are actively reused stay cached.
        entries.retain(|_, e| e.created_at.elapsed() < self.ttl);
        entries.insert(
            key,
            PoolEntry {
                client: client.clone(),
                created_at: Instant::now(),
            },
        );
        client
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rds::client::mock::MockRdsClient;
    use crate::rds::{ExecuteOutput, RdsError};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn target(cluster: &str) -> Target {
        Target {
            cluster_arn: format!("arn:aws:rds:us-east-1:123456789012:cluster:{cluster}"),
            secret_arn: "arn:aws:secretsmanager:us-east-1:123456789012:secret:s".into(),
            database: "appdb".into(),
            region: "us-east-1".into(),
            profile: None,
            read_only: false,
        }
    }

    /// A `RdsClient` that does nothing; only used so pool tests can hand out
    /// distinguishable `Arc` instances without touching AWS.
    struct StubClient;

    #[async_trait]
    impl RdsClient for StubClient {
        async fn execute_statement(
            &self,
            _sql: &str,
            _parameters: Vec<aws_sdk_rdsdata::types::SqlParameter>,
            _transaction_id: Option<&str>,
        ) -> Result<ExecuteOutput, RdsError> {
            Ok(ExecuteOutput::default())
        }
        async fn begin_transaction(&self) -> Result<String, RdsError> {
            Ok("tx".into())
        }
        async fn commit_transaction(&self, _transaction_id: &str) -> Result<(), RdsError> {
            Ok(())
        }
        async fn rollback_transaction(&self, _transaction_id: &str) -> Result<(), RdsError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn cache_hit_skips_build_and_returns_same_arc() {
        let pool = RdsClientPool::default();
        let t = target("c");
        let build_calls = AtomicU32::new(0);

        let build = || async {
            build_calls.fetch_add(1, Ordering::SeqCst);
            Arc::new(StubClient) as Arc<dyn RdsClient>
        };
        let first = pool.get_or_build(&t, Some("dev"), build).await;

        let build = || async {
            build_calls.fetch_add(1, Ordering::SeqCst);
            Arc::new(StubClient) as Arc<dyn RdsClient>
        };
        let second = pool.get_or_build(&t, Some("dev"), build).await;

        assert!(Arc::ptr_eq(&first, &second), "cache hit must reuse the Arc");
        assert_eq!(build_calls.load(Ordering::SeqCst), 1, "build must run once");
    }

    #[tokio::test]
    async fn different_profile_builds_separately() {
        let pool = RdsClientPool::default();
        let t = target("c");

        let a = pool
            .get_or_build(&t, Some("dev"), || async {
                Arc::new(StubClient) as Arc<dyn RdsClient>
            })
            .await;
        let b = pool
            .get_or_build(&t, Some("prod"), || async {
                Arc::new(StubClient) as Arc<dyn RdsClient>
            })
            .await;

        assert!(
            !Arc::ptr_eq(&a, &b),
            "distinct profiles must not share a cached client"
        );
    }

    #[tokio::test]
    async fn expired_entry_is_rebuilt() {
        let pool = RdsClientPool::new(Duration::from_millis(10));
        let t = target("c");

        let first = pool
            .get_or_build(&t, None, || async {
                Arc::new(StubClient) as Arc<dyn RdsClient>
            })
            .await;

        tokio::time::sleep(Duration::from_millis(30)).await;

        let second = pool
            .get_or_build(&t, None, || async {
                Arc::new(StubClient) as Arc<dyn RdsClient>
            })
            .await;

        assert!(
            !Arc::ptr_eq(&first, &second),
            "expired entry must be rebuilt, not reused"
        );
    }

    #[tokio::test]
    async fn expired_entries_are_swept_not_left_forever() {
        // A client that reconnects with a different profile override each
        // time (the pg `password` field is a free-form override) must not
        // grow the map unbounded — expired entries for keys nobody reuses
        // get dropped, not just left in place until overwritten.
        let pool = RdsClientPool::new(Duration::from_millis(10));
        let t = target("c");

        for profile in ["a", "b", "c"] {
            pool.get_or_build(&t, Some(profile), || async {
                Arc::new(StubClient) as Arc<dyn RdsClient>
            })
            .await;
        }
        assert_eq!(pool.len().await, 3);

        tokio::time::sleep(Duration::from_millis(30)).await;

        // Triggers a rebuild for "a", which sweeps the now-expired "b"/"c"
        // entries as a side effect of the insert.
        pool.get_or_build(&t, Some("a"), || async {
            Arc::new(StubClient) as Arc<dyn RdsClient>
        })
        .await;

        assert_eq!(
            pool.len().await,
            1,
            "expired, unreused entries must be swept on insert"
        );
    }

    #[tokio::test]
    async fn concurrent_racers_on_a_cold_key_both_succeed() {
        // Exercises the race the module doc comment describes in prose:
        // two callers racing past a missing entry for the same key both
        // run build() and both get back a valid, usable client -- neither
        // deadlocks or blocks on the other, and the loser's Arc is still
        // fully functional even though its insert gets overwritten.
        let pool = Arc::new(RdsClientPool::default());
        let t = target("c");
        let build_calls = Arc::new(AtomicU32::new(0));

        let racer = |pool: Arc<RdsClientPool>, t: Target, build_calls: Arc<AtomicU32>| async move {
            pool.get_or_build(&t, Some("dev"), || async {
                // Widen the race window so both racers are past the
                // cache-hit check before either finishes building.
                tokio::time::sleep(Duration::from_millis(20)).await;
                build_calls.fetch_add(1, Ordering::SeqCst);
                Arc::new(StubClient) as Arc<dyn RdsClient>
            })
            .await
        };

        let (first, second) = tokio::join!(
            racer(pool.clone(), t.clone(), build_calls.clone()),
            racer(pool.clone(), t.clone(), build_calls.clone())
        );

        assert_eq!(
            build_calls.load(Ordering::SeqCst),
            2,
            "both racers must run build() independently, neither waits on the other"
        );
        // Both racers get back a valid client regardless of which insert
        // "won" the map slot -- neither Arc is invalidated by the other.
        first
            .begin_transaction()
            .await
            .expect("first racer's client still usable");
        second
            .begin_transaction()
            .await
            .expect("second racer's client still usable");

        // A subsequent lookup returns whichever insert landed last --
        // matching the doc comment's "second insert simply wins".
        let subsequent = pool
            .get_or_build(&t, Some("dev"), || async {
                panic!("must be a cache hit, build should not run again")
            })
            .await;
        assert!(
            Arc::ptr_eq(&subsequent, &first) || Arc::ptr_eq(&subsequent, &second),
            "cache must hold whichever racer's client was inserted last"
        );
    }

    #[tokio::test]
    async fn mock_rds_client_can_be_pooled() {
        // Sanity check against the project's own existing test double, not
        // just the local StubClient above.
        let pool = RdsClientPool::default();
        let t = target("c");

        let client = pool
            .get_or_build(&t, None, || async {
                Arc::new(MockRdsClient::default()) as Arc<dyn RdsClient>
            })
            .await;
        client.begin_transaction().await.expect("stub begin");
    }
}
