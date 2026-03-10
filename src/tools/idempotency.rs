//! Tool execution idempotency cache.
//!
//! Caches results of idempotent tool calls (same tool + same args = same result)
//! to avoid redundant re-execution during LLM retry loops, self-repair recovery,
//! and stuck job retries.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lru::LruCache;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

/// Cached tool result with expiration.
#[derive(Clone, Debug)]
struct CachedResult {
    /// The serialized tool output string.
    output: String,
    /// When this entry was inserted.
    inserted_at: Instant,
}

/// Configuration for the idempotency cache.
#[derive(Debug, Clone)]
pub struct IdempotencyCacheConfig {
    /// Maximum total entries across all jobs. Default: 2000.
    pub max_entries: usize,
    /// Time-to-live for cached entries. Default: 30 minutes.
    pub ttl: Duration,
}

impl Default for IdempotencyCacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 2000,
            ttl: Duration::from_secs(30 * 60),
        }
    }
}

/// Global idempotency cache for tool executions.
///
/// Uses a single LRU cache with composite keys (job_id + tool_name + args_hash).
/// Entries expire after `ttl` and the total size is bounded by `max_entries`.
#[derive(Clone)]
pub struct ToolIdempotencyCache {
    inner: Arc<Mutex<LruCache<String, CachedResult>>>,
    config: IdempotencyCacheConfig,
}

impl ToolIdempotencyCache {
    /// Create a new cache with the given configuration.
    pub fn new(config: IdempotencyCacheConfig) -> Self {
        let cap = std::num::NonZeroUsize::new(config.max_entries)
            .unwrap_or(std::num::NonZeroUsize::new(1).expect("nonzero"));
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(cap))),
            config,
        }
    }

    /// Look up a cached result. Returns `None` if absent or expired.
    pub async fn get(
        &self,
        job_id: &str,
        tool_name: &str,
        params: &serde_json::Value,
    ) -> Option<String> {
        let key = Self::cache_key(job_id, tool_name, params);
        let mut cache = self.inner.lock().await;
        if let Some(entry) = cache.get(&key) {
            if entry.inserted_at.elapsed() < self.config.ttl {
                return Some(entry.output.clone());
            }
            // Expired — remove it
            cache.pop(&key);
        }
        None
    }

    /// Store a successful tool result in the cache.
    pub async fn put(
        &self,
        job_id: &str,
        tool_name: &str,
        params: &serde_json::Value,
        output: String,
    ) {
        let key = Self::cache_key(job_id, tool_name, params);
        let entry = CachedResult {
            output,
            inserted_at: Instant::now(),
        };
        let mut cache = self.inner.lock().await;
        cache.put(key, entry);
    }

    /// Remove all cached entries for a specific job (call on job completion).
    pub async fn invalidate_job(&self, job_id: &str) {
        let prefix = format!("{}:", job_id);
        let mut cache = self.inner.lock().await;
        // Collect keys to remove (can't mutate while iterating)
        let keys_to_remove: Vec<String> = cache
            .iter()
            .filter_map(|(k, _)| {
                if k.starts_with(&prefix) {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        for key in keys_to_remove {
            cache.pop(&key);
        }
    }

    /// Build a deterministic cache key from job_id, tool name, and params.
    fn cache_key(job_id: &str, tool_name: &str, params: &serde_json::Value) -> String {
        let mut hasher = Sha256::new();
        hasher.update(tool_name.as_bytes());
        hasher.update(b":");
        // Canonical JSON via serde_json::to_string (keys are sorted in serde_json maps)
        let params_str = serde_json::to_string(params).unwrap_or_default();
        hasher.update(params_str.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        format!("{}:{}:{}", job_id, tool_name, hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> IdempotencyCacheConfig {
        IdempotencyCacheConfig {
            max_entries: 10,
            ttl: Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn test_cache_hit() {
        let cache = ToolIdempotencyCache::new(config());
        let params = serde_json::json!({"path": "/etc/hosts"});
        cache
            .put("job1", "read_file", &params, "file contents".into())
            .await;
        let result = cache.get("job1", "read_file", &params).await;
        assert_eq!(result, Some("file contents".into()));
    }

    #[tokio::test]
    async fn test_cache_miss() {
        let cache = ToolIdempotencyCache::new(config());
        let params = serde_json::json!({"path": "/etc/hosts"});
        let result = cache.get("job1", "read_file", &params).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_different_params_miss() {
        let cache = ToolIdempotencyCache::new(config());
        let params1 = serde_json::json!({"path": "/etc/hosts"});
        let params2 = serde_json::json!({"path": "/etc/passwd"});
        cache
            .put("job1", "read_file", &params1, "hosts".into())
            .await;
        let result = cache.get("job1", "read_file", &params2).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_different_jobs_isolated() {
        let cache = ToolIdempotencyCache::new(config());
        let params = serde_json::json!({"path": "/etc/hosts"});
        cache
            .put("job1", "read_file", &params, "from job1".into())
            .await;
        let result = cache.get("job2", "read_file", &params).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_invalidate_job() {
        let cache = ToolIdempotencyCache::new(config());
        let params = serde_json::json!({"q": "test"});
        cache.put("job1", "echo", &params, "echo1".into()).await;
        cache
            .put("job1", "time", &serde_json::json!({}), "now".into())
            .await;
        cache.put("job2", "echo", &params, "echo2".into()).await;

        cache.invalidate_job("job1").await;

        assert_eq!(cache.get("job1", "echo", &params).await, None);
        assert_eq!(
            cache.get("job1", "time", &serde_json::json!({})).await,
            None
        );
        // job2 unaffected
        assert_eq!(
            cache.get("job2", "echo", &params).await,
            Some("echo2".into())
        );
    }

    #[tokio::test]
    async fn test_ttl_expiry() {
        let cache = ToolIdempotencyCache::new(IdempotencyCacheConfig {
            max_entries: 10,
            ttl: Duration::from_millis(1),
        });
        let params = serde_json::json!({"x": 1});
        cache.put("job1", "echo", &params, "val".into()).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(cache.get("job1", "echo", &params).await, None);
    }

    #[tokio::test]
    async fn test_lru_eviction() {
        let cache = ToolIdempotencyCache::new(IdempotencyCacheConfig {
            max_entries: 3,
            ttl: Duration::from_secs(60),
        });
        // Fill to capacity
        for i in 0..3 {
            let params = serde_json::json!({"i": i});
            cache.put("job1", "echo", &params, format!("val{i}")).await;
        }
        // Insert one more, evicting the oldest (i=0)
        let params_new = serde_json::json!({"i": 99});
        cache.put("job1", "echo", &params_new, "val99".into()).await;

        assert_eq!(
            cache
                .get("job1", "echo", &serde_json::json!({"i": 0}))
                .await,
            None
        );
        assert_eq!(
            cache
                .get("job1", "echo", &serde_json::json!({"i": 2}))
                .await,
            Some("val2".into())
        );
        assert_eq!(
            cache.get("job1", "echo", &params_new).await,
            Some("val99".into())
        );
    }

    #[tokio::test]
    async fn test_cache_key_determinism() {
        let key1 =
            ToolIdempotencyCache::cache_key("j1", "echo", &serde_json::json!({"a": 1, "b": 2}));
        let key2 =
            ToolIdempotencyCache::cache_key("j1", "echo", &serde_json::json!({"a": 1, "b": 2}));
        assert_eq!(key1, key2);
    }

    #[tokio::test]
    async fn test_overwrite_existing_entry() {
        let cache = ToolIdempotencyCache::new(config());
        let params = serde_json::json!({"x": 1});
        cache.put("job1", "echo", &params, "old".into()).await;
        cache.put("job1", "echo", &params, "new".into()).await;
        assert_eq!(cache.get("job1", "echo", &params).await, Some("new".into()));
    }
}
