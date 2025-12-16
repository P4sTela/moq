mod pattern_based;

pub use pattern_based::PatternBasedCachePolicy;

use crate::Path;

/// Decision on whether to cache an item
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDecision {
	/// Cache the item
	Cache,
	/// Do not cache the item
	NoCache,
}

impl CacheDecision {
	pub fn should_cache(&self) -> bool {
		matches!(self, CacheDecision::Cache)
	}
}

/// Trait for cache policy decision making
pub trait CachePolicy: Send + Sync {
	/// Check if a broadcast should be cached
	fn should_cache_broadcast(&self, path: &Path) -> CacheDecision;

	/// Check if a track should be cached
	fn should_cache_track(&self, broadcast_path: &Path, track_name: &str, priority: u8) -> CacheDecision;

	/// Check if a group should be cached
	fn should_cache_group(&self, sequence: u64, estimated_size: Option<u64>) -> CacheDecision;

	/// Check if a frame should be cached
	fn should_cache_frame(&self, frame_size: u64) -> CacheDecision;

	/// Check if backup broadcasts should be kept
	fn should_keep_backup(&self, age_seconds: u64, backup_count: usize) -> bool;
}

/// Always cache everything (default behavior, backward compatible)
#[derive(Debug, Clone, Default)]
pub struct AlwaysCachePolicy;

impl CachePolicy for AlwaysCachePolicy {
	fn should_cache_broadcast(&self, _path: &Path) -> CacheDecision {
		CacheDecision::Cache
	}

	fn should_cache_track(&self, _broadcast_path: &Path, _track_name: &str, _priority: u8) -> CacheDecision {
		CacheDecision::Cache
	}

	fn should_cache_group(&self, _sequence: u64, _estimated_size: Option<u64>) -> CacheDecision {
		CacheDecision::Cache
	}

	fn should_cache_frame(&self, _frame_size: u64) -> CacheDecision {
		CacheDecision::Cache
	}

	fn should_keep_backup(&self, _age_seconds: u64, _backup_count: usize) -> bool {
		true // Keep all backups
	}
}

/// Never cache anything (memory saving mode)
#[derive(Debug, Clone, Default)]
pub struct NeverCachePolicy;

impl CachePolicy for NeverCachePolicy {
	fn should_cache_broadcast(&self, _path: &Path) -> CacheDecision {
		CacheDecision::NoCache
	}

	fn should_cache_track(&self, _broadcast_path: &Path, _track_name: &str, _priority: u8) -> CacheDecision {
		CacheDecision::NoCache
	}

	fn should_cache_group(&self, _sequence: u64, _estimated_size: Option<u64>) -> CacheDecision {
		CacheDecision::NoCache
	}

	fn should_cache_frame(&self, _frame_size: u64) -> CacheDecision {
		CacheDecision::NoCache
	}

	fn should_keep_backup(&self, _age_seconds: u64, _backup_count: usize) -> bool {
		false // Don't keep any backups
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_cache_decision() {
		assert!(CacheDecision::Cache.should_cache());
		assert!(!CacheDecision::NoCache.should_cache());
	}

	#[test]
	fn test_always_cache_policy() {
		let policy = AlwaysCachePolicy;
		assert_eq!(
			policy.should_cache_broadcast(&Path::new("test")),
			CacheDecision::Cache
		);
		assert_eq!(
			policy.should_cache_track(&Path::new("test"), "video", 128),
			CacheDecision::Cache
		);
		assert_eq!(policy.should_cache_group(1, Some(1024)), CacheDecision::Cache);
		assert_eq!(policy.should_cache_frame(512), CacheDecision::Cache);
		assert!(policy.should_keep_backup(3600, 10));
	}

	#[test]
	fn test_never_cache_policy() {
		let policy = NeverCachePolicy;
		assert_eq!(
			policy.should_cache_broadcast(&Path::new("test")),
			CacheDecision::NoCache
		);
		assert_eq!(
			policy.should_cache_track(&Path::new("test"), "video", 128),
			CacheDecision::NoCache
		);
		assert_eq!(policy.should_cache_group(1, Some(1024)), CacheDecision::NoCache);
		assert_eq!(policy.should_cache_frame(512), CacheDecision::NoCache);
		assert!(!policy.should_keep_backup(3600, 10));
	}
}
