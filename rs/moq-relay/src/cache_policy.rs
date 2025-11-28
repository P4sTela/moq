use clap::Parser;
use moq_lite::{AlwaysCachePolicy, CachePolicy, NeverCachePolicy, PatternBasedCachePolicy};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Cache policy configuration for the relay
#[derive(Parser, Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct CachePolicyConfig {
	/// Enable caching globally
	#[arg(long, default_value_t = true)]
	pub cache_enabled: bool,

	/// Broadcast cache policy
	#[command(flatten)]
	pub broadcast: BroadcastCachePolicy,

	/// Track cache policy
	#[command(flatten)]
	pub track: TrackCachePolicy,

	/// Group cache policy
	#[command(flatten)]
	pub group: GroupCachePolicy,

	/// Global cache limits
	#[command(flatten)]
	pub limits: CacheLimits,
}

impl Default for CachePolicyConfig {
	fn default() -> Self {
		Self {
			cache_enabled: true,
			broadcast: BroadcastCachePolicy::default(),
			track: TrackCachePolicy::default(),
			group: GroupCachePolicy::default(),
			limits: CacheLimits::default(),
		}
	}
}

/// Broadcast-level cache policy
#[derive(Parser, Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct BroadcastCachePolicy {
	/// Cache broadcasts matching these patterns (glob)
	#[arg(long, value_delimiter = ',')]
	#[serde(default)]
	pub cache_patterns: Vec<String>,

	/// Exclude broadcasts matching these patterns (glob)
	#[arg(long, value_delimiter = ',')]
	#[serde(default)]
	pub exclude_patterns: Vec<String>,

	/// Maximum age of backup broadcasts in seconds (0 = unlimited)
	#[arg(long, default_value_t = 0)]
	pub backup_max_age_seconds: u64,

	/// Maximum number of backup broadcasts to keep per path
	#[arg(long, default_value_t = 0)]
	pub backup_max_count: usize,
}

impl Default for BroadcastCachePolicy {
	fn default() -> Self {
		Self {
			cache_patterns: vec!["**".to_string()], // Cache everything by default
			exclude_patterns: Vec::new(),
			backup_max_age_seconds: 0, // No TTL by default
			backup_max_count: 0,       // Unlimited by default
		}
	}
}

/// Track-level cache policy
#[derive(Parser, Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TrackCachePolicy {
	/// Maximum number of tracks to cache per broadcast
	#[arg(long, default_value_t = 0)]
	pub max_tracks_per_broadcast: usize,

	/// Cache high priority tracks (priority >= threshold)
	#[arg(long, default_value_t = 0)]
	pub min_priority: u8,
}

impl Default for TrackCachePolicy {
	fn default() -> Self {
		Self {
			max_tracks_per_broadcast: 0, // Unlimited by default
			min_priority: 0,              // All priorities by default
		}
	}
}

/// Group-level cache policy
#[derive(Parser, Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct GroupCachePolicy {
	/// Maximum number of groups to cache per track
	#[arg(long, default_value_t = 1)]
	pub max_groups_per_track: usize,

	/// Maximum frames per group
	#[arg(long, default_value_t = 0)]
	pub max_frames_per_group: usize,
}

impl Default for GroupCachePolicy {
	fn default() -> Self {
		Self {
			max_groups_per_track: 1, // Only latest group by default
			max_frames_per_group: 0, // Unlimited frames by default
		}
	}
}

/// Global cache size limits
#[derive(Parser, Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct CacheLimits {
	/// Maximum total cache size in bytes (0 = unlimited)
	#[arg(long, default_value_t = 0)]
	pub max_cache_size_bytes: u64,

	/// Maximum cache size per broadcast in bytes (0 = unlimited)
	#[arg(long, default_value_t = 0)]
	pub max_broadcast_size_bytes: u64,

	/// Maximum frame size in bytes (0 = unlimited)
	#[arg(long, default_value_t = 0)]
	pub max_frame_size_bytes: u64,
}

impl Default for CacheLimits {
	fn default() -> Self {
		Self {
			max_cache_size_bytes: 0,
			max_broadcast_size_bytes: 0,
			max_frame_size_bytes: 0,
		}
	}
}

impl CachePolicyConfig {
	/// Create a cache policy implementation from this configuration
	pub fn build(&self) -> anyhow::Result<Arc<dyn CachePolicy>> {
		if !self.cache_enabled {
			return Ok(Arc::new(NeverCachePolicy));
		}

		// If using default patterns and no limits, use AlwaysCachePolicy for backward compatibility
		if self.broadcast.cache_patterns == vec!["**"]
			&& self.broadcast.exclude_patterns.is_empty()
			&& self.track.min_priority == 0
			&& self.broadcast.backup_max_age_seconds == 0
			&& self.broadcast.backup_max_count == 0
			&& self.limits.max_frame_size_bytes == 0
		{
			return Ok(Arc::new(AlwaysCachePolicy));
		}

		// Build pattern-based policy
		let policy = PatternBasedCachePolicy::new()
			.with_cache_patterns(self.broadcast.cache_patterns.clone())?
			.with_exclude_patterns(self.broadcast.exclude_patterns.clone())?
			.with_min_track_priority(self.track.min_priority)
			.with_backup_max_age(self.broadcast.backup_max_age_seconds)
			.with_backup_max_count(self.broadcast.backup_max_count)
			.with_max_groups_per_track(self.group.max_groups_per_track)
			.with_max_frames_per_group(self.group.max_frames_per_group)
			.with_max_frame_size(self.limits.max_frame_size_bytes);

		Ok(Arc::new(policy))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_default_config() {
		let config = CachePolicyConfig::default();
		assert!(config.cache_enabled);
		assert_eq!(config.broadcast.cache_patterns, vec!["**"]);
		assert_eq!(config.group.max_groups_per_track, 1);
	}

	#[test]
	fn test_memory_saving_mode() {
		let config = CachePolicyConfig {
			cache_enabled: true,
			broadcast: BroadcastCachePolicy {
				cache_patterns: vec!["live/**".to_string()],
				exclude_patterns: vec!["*/archive/*".to_string()],
				backup_max_age_seconds: 300,
				backup_max_count: 3,
			},
			track: TrackCachePolicy {
				max_tracks_per_broadcast: 10,
				min_priority: 128,
			},
			group: GroupCachePolicy {
				max_groups_per_track: 1,
				max_frames_per_group: 100,
			},
			limits: CacheLimits {
				max_cache_size_bytes: 100 * 1024 * 1024, // 100MB
				max_broadcast_size_bytes: 10 * 1024 * 1024, // 10MB
				max_frame_size_bytes: 1024 * 1024,        // 1MB
			},
		};

		assert_eq!(config.broadcast.backup_max_age_seconds, 300);
		assert_eq!(config.limits.max_cache_size_bytes, 100 * 1024 * 1024);
	}

	#[test]
	fn test_build_always_cache_policy() {
		let config = CachePolicyConfig::default();
		let _policy = config.build().unwrap();
		// Should use AlwaysCachePolicy for default config
	}

	#[test]
	fn test_build_never_cache_policy() {
		let mut config = CachePolicyConfig::default();
		config.cache_enabled = false;
		let _policy = config.build().unwrap();
		// Should use NeverCachePolicy when disabled
	}

	#[test]
	fn test_build_pattern_based_policy() {
		let mut config = CachePolicyConfig::default();
		config.broadcast.cache_patterns = vec!["live/**".to_string()];
		let _policy = config.build().unwrap();
		// Should use PatternBasedCachePolicy with custom patterns
	}
}
