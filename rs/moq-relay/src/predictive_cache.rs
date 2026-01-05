use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
	time::{Duration, SystemTime},
};

use moq_lite::{BroadcastConsumer, Path, PathOwned};
use tracing::debug;

use crate::cluster::PredictiveCacheConfig;

/// Cached broadcast entry with metadata
#[derive(Clone)]
struct CachedBroadcast {
	/// The broadcast consumer for this cached content
	consumer: BroadcastConsumer,
	/// When this entry was cached
	cached_at: SystemTime,
	/// Estimated size in bytes (if known)
	size_bytes: Option<u64>,
	/// Number of times this entry was accessed
	access_count: u64,
	/// Last access time for LRU eviction
	last_access: SystemTime,
}

/// Metrics for monitoring cache performance
#[derive(Default, Clone, Debug)]
pub struct CacheMetrics {
	/// Total cache hits (content served from cache)
	pub hits: u64,
	/// Total cache misses (content not in cache)
	pub misses: u64,
	/// Total prefetch operations performed
	pub prefetches: u64,
	/// Total evictions due to TTL or size limits
	pub evictions: u64,
	/// Current cache size in bytes
	pub current_bytes: u64,
	/// Number of entries currently cached
	pub entry_count: usize,
}

/// Predictive cache for edge relay
///
/// Monitors ANNOUNCE messages from Root and prefetches matching broadcasts
/// based on configured patterns. Uses LRU eviction when cache size limits
/// are reached, and TTL-based cleanup for expired entries.
pub struct PredictiveCache {
	config: PredictiveCacheConfig,
	/// Storage: broadcast path -> cached entry
	storage: Arc<Mutex<HashMap<PathOwned, CachedBroadcast>>>,
	/// Compiled glob patterns for matching
	patterns: Vec<glob::Pattern>,
	/// Metrics for monitoring
	metrics: Arc<Mutex<CacheMetrics>>,
}

impl PredictiveCache {
	/// Create a new predictive cache with the given configuration
	pub fn new(config: PredictiveCacheConfig) -> anyhow::Result<Self> {
		tracing::info!(
			patterns = ?config.prefetch_patterns,
			max_bytes = config.max_cache_bytes,
			ttl_seconds = config.ttl_seconds,
			"initializing predictive cache"
		);

		// Compile glob patterns
		let patterns = config
			.prefetch_patterns
			.iter()
			.map(|p| glob::Pattern::new(p))
			.collect::<Result<Vec<_>, _>>()?;

		tracing::info!(pattern_count = patterns.len(), "predictive cache initialized");

		Ok(Self {
			config,
			storage: Arc::new(Mutex::new(HashMap::new())),
			patterns,
			metrics: Arc::new(Mutex::new(CacheMetrics::default())),
		})
	}

	/// Check if a broadcast path matches any prefetch patterns
	pub fn should_prefetch(&self, path: &Path) -> bool {
		if self.patterns.is_empty() {
			return false;
		}

		let path_str = path.as_str();
		self.patterns.iter().any(|pattern| pattern.matches(path_str))
	}

	/// Prefetch a broadcast and store it in cache
	pub async fn prefetch(&self, path: PathOwned, consumer: BroadcastConsumer) -> anyhow::Result<()> {
		debug!(path = %path.as_str(), "prefetching broadcast");

		// Check cache size limits before adding
		if self.config.max_cache_bytes > 0 {
			let current_size = self.metrics.lock().unwrap().current_bytes;
			if current_size >= self.config.max_cache_bytes {
				// Evict LRU entry to make room
				self.evict_lru();
			}
		}

		let now = SystemTime::now();
		let entry = CachedBroadcast {
			consumer,
			cached_at: now,
			size_bytes: None, // Will be updated as we consume frames
			access_count: 0,
			last_access: now,
		};

		self.storage.lock().unwrap().insert(path.clone(), entry);

		// Update metrics
		let mut metrics = self.metrics.lock().unwrap();
		metrics.prefetches += 1;
		metrics.entry_count = self.storage.lock().unwrap().len();

		debug!(
			path = %path.as_str(),
			prefetches = metrics.prefetches,
			entry_count = metrics.entry_count,
			"prefetch completed successfully"
		);

		Ok(())
	}

	/// Try to get a broadcast from cache (cache hit)
	pub fn get(&self, path: &Path) -> Option<BroadcastConsumer> {
		let mut storage = self.storage.lock().unwrap();
		let mut metrics = self.metrics.lock().unwrap();

		if let Some(entry) = storage.get_mut(&path.to_owned()) {
			// Update access metadata
			entry.access_count += 1;
			entry.last_access = SystemTime::now();

			// Record hit
			metrics.hits += 1;
			debug!(
				path = %path.as_str(),
				access_count = entry.access_count,
				"cache hit"
			);

			Some(entry.consumer.clone())
		} else {
			// Record miss
			metrics.misses += 1;
			debug!(path = %path.as_str(), "cache miss");
			None
		}
	}

	/// Evict least recently used entry
	fn evict_lru(&self) {
		let mut storage = self.storage.lock().unwrap();

		// Find LRU entry
		let lru_path = storage
			.iter()
			.min_by_key(|(_, entry)| entry.last_access)
			.map(|(path, _)| path.clone());

		if let Some(path) = lru_path {
			debug!(path = %path.as_str(), "evicting LRU entry");
			storage.remove(&path);

			let mut metrics = self.metrics.lock().unwrap();
			metrics.evictions += 1;
			metrics.entry_count = storage.len();
		}
	}

	/// Clean up expired entries based on TTL
	pub fn cleanup_expired(&self) {
		if self.config.ttl_seconds == 0 {
			return; // No TTL configured
		}

		let ttl = Duration::from_secs(self.config.ttl_seconds);
		let now = SystemTime::now();
		let mut storage = self.storage.lock().unwrap();

		let expired_paths: Vec<PathOwned> = storage
			.iter()
			.filter(|(_, entry)| {
				now.duration_since(entry.cached_at)
					.map(|age| age > ttl)
					.unwrap_or(false)
			})
			.map(|(path, _)| path.clone())
			.collect();

		let eviction_count = expired_paths.len();

		for path in expired_paths {
			debug!(path = %path.as_str(), "evicting expired entry");
			storage.remove(&path);
		}

		if eviction_count > 0 {
			let mut metrics = self.metrics.lock().unwrap();
			metrics.evictions += eviction_count as u64;
			metrics.entry_count = storage.len();
		}
	}

	/// Get current cache metrics
	pub fn metrics(&self) -> CacheMetrics {
		self.metrics.lock().unwrap().clone()
	}

	/// Start background cleanup task
	pub fn spawn_cleanup_task(self: Arc<Self>) {
		if self.config.ttl_seconds == 0 {
			return; // No cleanup needed
		}

		tokio::spawn(async move {
			let cleanup_interval = Duration::from_secs(self.config.ttl_seconds.max(60) / 2);
			let mut interval = tokio::time::interval(cleanup_interval);

			loop {
				interval.tick().await;
				self.cleanup_expired();

				// Log metrics periodically
				let metrics = self.metrics();
				debug!(
					hits = metrics.hits,
					misses = metrics.misses,
					prefetches = metrics.prefetches,
					evictions = metrics.evictions,
					entries = metrics.entry_count,
					"cache metrics"
				);
			}
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use moq_lite::Broadcast;

	#[test]
	fn test_pattern_matching() {
		let config = PredictiveCacheConfig {
			enabled: true,
			prefetch_patterns: vec!["live/**".to_string(), "sports/*".to_string()],
			lookahead_seconds: 0,
			max_cache_bytes: 0,
			ttl_seconds: 3600,
		};

		let cache = PredictiveCache::new(config).unwrap();

		assert!(cache.should_prefetch(&Path::new("live/stream1")));
		assert!(cache.should_prefetch(&Path::new("live/events/match")));
		assert!(cache.should_prefetch(&Path::new("sports/football")));
		assert!(!cache.should_prefetch(&Path::new("archive/old")));
	}

	#[tokio::test]
	async fn test_prefetch_and_get() {
		let config = PredictiveCacheConfig {
			enabled: true,
			prefetch_patterns: vec!["test/**".to_string()],
			lookahead_seconds: 0,
			max_cache_bytes: 0,
			ttl_seconds: 3600,
		};

		let cache = PredictiveCache::new(config).unwrap();
		let produce = Broadcast::produce();
		let _producer = produce.producer;
		let consumer = produce.consumer;

		let path = PathOwned::new("test/stream");
		cache.prefetch(path.clone(), consumer).await.unwrap();

		// Should get cache hit
		let cached = cache.get(&path);
		assert!(cached.is_some());

		// Check metrics
		let metrics = cache.metrics();
		assert_eq!(metrics.prefetches, 1);
		assert_eq!(metrics.hits, 1);
		assert_eq!(metrics.misses, 0);
	}

	#[tokio::test]
	async fn test_cache_miss() {
		let config = PredictiveCacheConfig::default();
		let cache = PredictiveCache::new(config).unwrap();

		let path = Path::new("not/cached");
		let result = cache.get(&path);
		assert!(result.is_none());

		let metrics = cache.metrics();
		assert_eq!(metrics.misses, 1);
	}

	#[test]
	fn test_ttl_cleanup() {
		let config = PredictiveCacheConfig {
			enabled: true,
			prefetch_patterns: vec!["**".to_string()],
			lookahead_seconds: 0,
			max_cache_bytes: 0,
			ttl_seconds: 1, // 1 second TTL
		};

		let cache = PredictiveCache::new(config).unwrap();

		// Add entry with past timestamp
		let path = PathOwned::new("test/old");
		let produce = Broadcast::produce();
		let consumer = produce.consumer;
		let entry = CachedBroadcast {
			consumer,
			cached_at: SystemTime::now() - Duration::from_secs(10),
			size_bytes: None,
			access_count: 0,
			last_access: SystemTime::now(),
		};

		cache.storage.lock().unwrap().insert(path.clone(), entry);

		// Run cleanup
		cache.cleanup_expired();

		// Entry should be evicted
		assert!(cache.storage.lock().unwrap().is_empty());
	}
}
