use glob::Pattern;

use crate::{CacheDecision, CachePolicy, Path};

/// Pattern-based cache policy with configurable rules
#[derive(Debug, Clone)]
pub struct PatternBasedCachePolicy {
	/// Glob patterns for broadcasts to cache
	pub cache_patterns: Vec<Pattern>,
	/// Glob patterns for broadcasts to exclude from caching
	pub exclude_patterns: Vec<Pattern>,
	/// Minimum priority for caching tracks (0-255)
	pub min_track_priority: u8,
	/// Maximum age for backup broadcasts in seconds (0 = unlimited)
	pub backup_max_age_seconds: u64,
	/// Maximum number of backup broadcasts to keep (0 = unlimited)
	pub backup_max_count: usize,
	/// Maximum groups to cache per track (0 = unlimited)
	pub max_groups_per_track: usize,
	/// Maximum frames per group (0 = unlimited)
	pub max_frames_per_group: usize,
	/// Maximum frame size in bytes (0 = unlimited)
	pub max_frame_size_bytes: u64,
}

impl Default for PatternBasedCachePolicy {
	fn default() -> Self {
		Self {
			cache_patterns: vec![Pattern::new("**").expect("valid pattern")],
			exclude_patterns: Vec::new(),
			min_track_priority: 0,
			backup_max_age_seconds: 0,
			backup_max_count: 0,
			max_groups_per_track: 1, // Only latest group by default
			max_frames_per_group: 0,
			max_frame_size_bytes: 0,
		}
	}
}

impl PatternBasedCachePolicy {
	/// Create a new pattern-based cache policy
	pub fn new() -> Self {
		Self::default()
	}

	/// Set cache patterns (glob patterns)
	pub fn with_cache_patterns(mut self, patterns: Vec<String>) -> Result<Self, glob::PatternError> {
		self.cache_patterns = patterns
			.into_iter()
			.map(|p| Pattern::new(&p))
			.collect::<Result<Vec<_>, _>>()?;
		Ok(self)
	}

	/// Set exclude patterns (glob patterns)
	pub fn with_exclude_patterns(mut self, patterns: Vec<String>) -> Result<Self, glob::PatternError> {
		self.exclude_patterns = patterns
			.into_iter()
			.map(|p| Pattern::new(&p))
			.collect::<Result<Vec<_>, _>>()?;
		Ok(self)
	}

	/// Set minimum track priority
	pub fn with_min_track_priority(mut self, priority: u8) -> Self {
		self.min_track_priority = priority;
		self
	}

	/// Set backup max age in seconds
	pub fn with_backup_max_age(mut self, seconds: u64) -> Self {
		self.backup_max_age_seconds = seconds;
		self
	}

	/// Set backup max count
	pub fn with_backup_max_count(mut self, count: usize) -> Self {
		self.backup_max_count = count;
		self
	}

	/// Set max groups per track
	pub fn with_max_groups_per_track(mut self, max: usize) -> Self {
		self.max_groups_per_track = max;
		self
	}

	/// Set max frames per group
	pub fn with_max_frames_per_group(mut self, max: usize) -> Self {
		self.max_frames_per_group = max;
		self
	}

	/// Set max frame size
	pub fn with_max_frame_size(mut self, bytes: u64) -> Self {
		self.max_frame_size_bytes = bytes;
		self
	}

	/// Check if a path matches any of the patterns
	fn matches_patterns(path: &str, patterns: &[Pattern]) -> bool {
		patterns.iter().any(|p| p.matches(path))
	}
}

impl CachePolicy for PatternBasedCachePolicy {
	fn should_cache_broadcast(&self, path: &Path) -> CacheDecision {
		let path_str = path.as_str();

		// Check exclude patterns first (they take precedence)
		if !self.exclude_patterns.is_empty() && Self::matches_patterns(path_str, &self.exclude_patterns) {
			return CacheDecision::NoCache;
		}

		// Check cache patterns
		if Self::matches_patterns(path_str, &self.cache_patterns) {
			CacheDecision::Cache
		} else {
			CacheDecision::NoCache
		}
	}

	fn should_cache_track(&self, broadcast_path: &Path, _track_name: &str, priority: u8) -> CacheDecision {
		// First check if broadcast should be cached
		if !self.should_cache_broadcast(broadcast_path).should_cache() {
			return CacheDecision::NoCache;
		}

		// Then check priority
		if priority >= self.min_track_priority {
			CacheDecision::Cache
		} else {
			CacheDecision::NoCache
		}
	}

	fn should_cache_group(&self, _sequence: u64, estimated_size: Option<u64>) -> CacheDecision {
		// Note: Group count limits are enforced at insertion time, not here
		// This just checks size limits

		if self.max_frames_per_group > 0 {
			// Would need frame count tracking to enforce this properly
			// For now, we just accept groups
		}

		if let Some(size) = estimated_size {
			if self.max_frame_size_bytes > 0 && size > self.max_frame_size_bytes {
				return CacheDecision::NoCache;
			}
		}

		CacheDecision::Cache
	}

	fn should_cache_frame(&self, frame_size: u64) -> CacheDecision {
		if self.max_frame_size_bytes > 0 && frame_size > self.max_frame_size_bytes {
			CacheDecision::NoCache
		} else {
			CacheDecision::Cache
		}
	}

	fn should_keep_backup(&self, age_seconds: u64, backup_count: usize) -> bool {
		// Check age limit (>= to include exact age match for removal)
		if self.backup_max_age_seconds > 0 && age_seconds >= self.backup_max_age_seconds {
			return false;
		}

		// Check count limit
		if self.backup_max_count > 0 && backup_count > self.backup_max_count {
			return false;
		}

		true
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_default_policy() {
		let policy = PatternBasedCachePolicy::default();
		assert_eq!(
			policy.should_cache_broadcast(&Path::new("any/path")),
			CacheDecision::Cache
		);
	}

	#[test]
	fn test_cache_patterns() {
		let policy = PatternBasedCachePolicy::new()
			.with_cache_patterns(vec!["live/**".to_string()])
			.unwrap();

		assert_eq!(
			policy.should_cache_broadcast(&Path::new("live/stream1")),
			CacheDecision::Cache
		);
		assert_eq!(
			policy.should_cache_broadcast(&Path::new("archive/stream1")),
			CacheDecision::NoCache
		);
	}

	#[test]
	fn test_exclude_patterns() {
		let policy = PatternBasedCachePolicy::new()
			.with_cache_patterns(vec!["**".to_string()])
			.unwrap()
			.with_exclude_patterns(vec!["*/private/*".to_string()])
			.unwrap();

		assert_eq!(
			policy.should_cache_broadcast(&Path::new("live/public/stream")),
			CacheDecision::Cache
		);
		assert_eq!(
			policy.should_cache_broadcast(&Path::new("live/private/stream")),
			CacheDecision::NoCache
		);
	}

	#[test]
	fn test_priority_filtering() {
		let policy = PatternBasedCachePolicy::new().with_min_track_priority(128);

		assert_eq!(
			policy.should_cache_track(&Path::new("test"), "video", 255),
			CacheDecision::Cache
		);
		assert_eq!(
			policy.should_cache_track(&Path::new("test"), "audio", 64),
			CacheDecision::NoCache
		);
	}

	#[test]
	fn test_frame_size_limit() {
		let policy = PatternBasedCachePolicy::new().with_max_frame_size(1024);

		assert_eq!(policy.should_cache_frame(512), CacheDecision::Cache);
		assert_eq!(policy.should_cache_frame(2048), CacheDecision::NoCache);
	}

	#[test]
	fn test_backup_limits() {
		let policy = PatternBasedCachePolicy::new()
			.with_backup_max_age(300)
			.with_backup_max_count(5);

		assert!(policy.should_keep_backup(100, 3)); // Within limits
		assert!(!policy.should_keep_backup(400, 3)); // Age exceeded
		assert!(!policy.should_keep_backup(100, 6)); // Count exceeded
	}
}
