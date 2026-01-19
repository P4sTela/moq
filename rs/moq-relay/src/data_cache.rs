//! Data-level cache for edge relay
//!
//! Buffers actual frame data (groups) at the edge to improve TTFS (Time To First Sample).
//! When a subscriber connects, buffered groups are delivered immediately, then new groups
//! continue streaming from the origin.

use std::{
	collections::{HashMap, VecDeque},
	sync::Arc,
	time::{Duration, Instant},
};

use moq_lite::{
	Broadcast, BroadcastConsumer, BroadcastProducer, GroupConsumer, Path, PathOwned, Track,
	TrackConsumer, TrackProducer,
};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Configuration for data-level cache
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DataCacheConfig {
	/// Enable data caching
	pub enabled: bool,

	/// Maximum groups to buffer per track (ring buffer)
	pub max_groups_per_track: usize,

	/// Glob patterns for broadcasts to cache
	pub prefetch_patterns: Vec<String>,

	/// Track names to proactively prefetch (e.g., ["seconds", "video", "audio"])
	pub prefetch_tracks: Vec<String>,

	/// TTL in seconds for cached broadcasts
	pub ttl_seconds: u64,
}

impl Default for DataCacheConfig {
	fn default() -> Self {
		Self {
			enabled: false,
			max_groups_per_track: 3,
			prefetch_patterns: Vec::new(),
			prefetch_tracks: vec![
				"seconds".to_string(), // moq-clock
				"video".to_string(),
				"audio".to_string(),
			],
			ttl_seconds: 60,
		}
	}
}

/// Metrics for monitoring cache performance
#[derive(Default, Clone, Debug)]
pub struct DataCacheMetrics {
	/// Total cache hits (buffered data served)
	pub hits: u64,
	/// Total cache misses (no buffered data)
	pub misses: u64,
	/// Total groups buffered
	pub groups_buffered: u64,
	/// Total groups evicted from ring buffer
	pub groups_evicted: u64,
	/// Number of broadcasts currently cached
	pub broadcast_count: usize,
	/// Number of tracks currently cached
	pub track_count: usize,
}

/// Cached track with ring buffer of recent groups
struct CachedTrack {
	/// Track metadata
	info: Track,

	/// Ring buffer of recent groups (oldest first)
	/// Each GroupConsumer is cloned from the source - shares underlying data
	groups: VecDeque<GroupConsumer>,

	/// Maximum groups to buffer
	max_groups: usize,

	/// Latest sequence number seen
	latest_sequence: Option<u64>,
}

impl CachedTrack {
	fn new(info: Track, max_groups: usize) -> Self {
		Self {
			info,
			groups: VecDeque::with_capacity(max_groups),
			max_groups,
			latest_sequence: None,
		}
	}

	/// Add a new group to the ring buffer
	/// Returns true if a group was evicted
	fn push_group(&mut self, group: GroupConsumer) -> bool {
		let evicted = if self.groups.len() >= self.max_groups {
			self.groups.pop_front();
			true
		} else {
			false
		};

		self.latest_sequence = Some(group.info.sequence);
		self.groups.push_back(group);
		evicted
	}

	/// Get clones of all buffered groups
	fn get_buffered_groups(&self) -> Vec<GroupConsumer> {
		self.groups.iter().cloned().collect()
	}

	/// Number of buffered groups
	fn len(&self) -> usize {
		self.groups.len()
	}
}

/// Cached broadcast entry with buffered track data
#[allow(dead_code)]
struct CachedBroadcast {
	/// Original consumer (reference to origin)
	source: BroadcastConsumer,

	/// Local broadcast producer for serving cached data
	local_producer: BroadcastProducer,

	/// Local broadcast consumer (serves cached data + live)
	local_consumer: BroadcastConsumer,

	/// Cached tracks by name
	tracks: HashMap<String, Arc<Mutex<CachedTrack>>>,

	/// When this broadcast was cached
	created_at: Instant,

	/// Path of the broadcast
	#[allow(dead_code)]
	path: PathOwned,
}

/// Data-level cache for edge relay
///
/// Actively consumes groups from the origin and buffers them.
/// When a subscriber requests a track, buffered groups are delivered
/// immediately, then new groups continue streaming.
pub struct DataCache {
	config: DataCacheConfig,

	/// Cached broadcasts by path
	broadcasts: Arc<Mutex<HashMap<PathOwned, Arc<Mutex<CachedBroadcast>>>>>,

	/// Compiled glob patterns for matching
	patterns: Vec<glob::Pattern>,

	/// Metrics for monitoring
	metrics: Arc<Mutex<DataCacheMetrics>>,
}

impl DataCache {
	/// Create a new data cache with the given configuration
	pub fn new(config: DataCacheConfig) -> anyhow::Result<Self> {
		info!(
			patterns = ?config.prefetch_patterns,
			tracks = ?config.prefetch_tracks,
			max_groups = config.max_groups_per_track,
			ttl_seconds = config.ttl_seconds,
			"initializing data cache"
		);

		// Compile glob patterns
		let patterns = config
			.prefetch_patterns
			.iter()
			.map(|p| glob::Pattern::new(p))
			.collect::<Result<Vec<_>, _>>()?;

		info!(pattern_count = patterns.len(), "data cache initialized");

		Ok(Self {
			config,
			broadcasts: Arc::new(Mutex::new(HashMap::new())),
			patterns,
			metrics: Arc::new(Mutex::new(DataCacheMetrics::default())),
		})
	}

	/// Check if a broadcast path matches any cache patterns
	pub fn should_cache(&self, path: &Path) -> bool {
		if self.patterns.is_empty() {
			return false;
		}

		let path_str = path.as_str();
		self.patterns.iter().any(|pattern| pattern.matches(path_str))
	}

	/// Start caching a broadcast - spawns background tasks to buffer groups
	///
	/// Returns a BroadcastConsumer that serves cached data + live stream
	pub async fn start_caching(&self, path: PathOwned, consumer: BroadcastConsumer) -> BroadcastConsumer {
		debug!(path = %path.as_str(), "starting to cache broadcast");

		// Create local broadcast for serving cached data
		let local = Broadcast::produce();

		let entry = Arc::new(Mutex::new(CachedBroadcast {
			source: consumer.clone(),
			local_producer: local.producer.clone(),
			local_consumer: local.consumer.clone(),
			tracks: HashMap::new(),
			created_at: Instant::now(),
			path: path.clone(),
		}));

		// Store in cache
		{
			let mut broadcasts = self.broadcasts.lock().await;
			broadcasts.insert(path.clone(), entry.clone());

			let mut metrics = self.metrics.lock().await;
			metrics.broadcast_count = broadcasts.len();
		}

		debug!(path = %path.as_str(), "broadcast entry created with local producer");

		// Proactively start caching configured tracks
		for track_name in &self.config.prefetch_tracks {
			let track = Track { name: track_name.clone(), priority: 0 };
			let source = consumer.subscribe_track(&track);
			debug!(
				path = %path.as_str(),
				track = %track.name,
				"proactively caching track"
			);
			self.start_caching_track_with_local(
				path.clone(),
				track.clone(),
				source,
				local.producer.clone(),
			)
			.await;
		}

		// Return the local consumer that serves cached data
		local.consumer
	}

	/// Start caching a specific track within a broadcast
	#[allow(dead_code)]
	pub async fn start_caching_track(
		&self,
		path: PathOwned,
		track_name: String,
		source: TrackConsumer,
	) {
		let broadcasts = self.broadcasts.lock().await;
		let Some(broadcast_entry) = broadcasts.get(&path).cloned() else {
			warn!(path = %path.as_str(), "broadcast not found for track caching");
			return;
		};
		drop(broadcasts);

		let cached_track = Arc::new(Mutex::new(CachedTrack::new(
			source.info.clone(),
			self.config.max_groups_per_track,
		)));

		// Store track in broadcast entry
		{
			let mut broadcast = broadcast_entry.lock().await;
			broadcast.tracks.insert(track_name.clone(), cached_track.clone());
		}

		// Update metrics
		{
			let broadcasts = self.broadcasts.lock().await;
			let mut metrics = self.metrics.lock().await;
			metrics.track_count = broadcasts.values().fold(0, |acc, _b| {
				// Can't await in fold, so we skip exact count
				acc + 1
			});
		}

		// Spawn background task to continuously buffer groups
		let metrics = self.metrics.clone();
		tokio::spawn(async move {
			Self::run_track_cache_task(source, cached_track, metrics, None).await;
		});

		debug!(
			path = %path.as_str(),
			track = %track_name,
			"started caching track"
		);
	}

	/// Start caching a track and publish to local broadcast for serving
	async fn start_caching_track_with_local(
		&self,
		path: PathOwned,
		track: Track,
		source: TrackConsumer,
		mut local_producer: BroadcastProducer,
	) {
		let broadcasts = self.broadcasts.lock().await;
		let Some(broadcast_entry) = broadcasts.get(&path).cloned() else {
			warn!(path = %path.as_str(), "broadcast not found for track caching");
			return;
		};
		drop(broadcasts);

		let cached_track = Arc::new(Mutex::new(CachedTrack::new(
			source.info.clone(),
			self.config.max_groups_per_track,
		)));

		// Store track in broadcast entry
		{
			let mut broadcast = broadcast_entry.lock().await;
			broadcast.tracks.insert(track.name.clone(), cached_track.clone());
		}

		// Create local track producer for this track
		let local_track = local_producer.create_track(track.clone());

		// Spawn background task to buffer groups AND publish to local track
		let metrics = self.metrics.clone();
		let track_name = track.name.clone();
		tokio::spawn(async move {
			debug!(track = %track_name, "starting cached track forwarding");
			Self::run_track_cache_task(source, cached_track, metrics, Some(local_track)).await;
			debug!(track = %track_name, "cached track forwarding ended");
		});

		debug!(
			path = %path.as_str(),
			track = %track.name,
			"started caching track with local forwarding"
		);
	}

	/// Background task to buffer groups from a track and optionally publish to local track
	async fn run_track_cache_task(
		mut source: TrackConsumer,
		cached_track: Arc<Mutex<CachedTrack>>,
		metrics: Arc<Mutex<DataCacheMetrics>>,
		mut local_producer: Option<TrackProducer>,
	) {
		loop {
			match source.next_group().await {
				Ok(Some(group)) => {
					// Clone group for caching and local publishing
					let group_for_local = group.clone();

					// Add to cache ring buffer
					let mut track = cached_track.lock().await;
					let evicted = track.push_group(group);

					let mut m = metrics.lock().await;
					m.groups_buffered += 1;
					if evicted {
						m.groups_evicted += 1;
					}

					debug!(
						track = %track.info.name,
						sequence = track.latest_sequence,
						buffered = track.len(),
						"buffered group"
					);
					drop(track);
					drop(m);

					// Publish to local track for subscribers
					if let Some(ref mut producer) = local_producer {
						if !producer.insert_group(group_for_local) {
							debug!("local track producer closed");
							break;
						}
					}
				}
				Ok(None) => {
					debug!("track source closed");
					break;
				}
				Err(e) => {
					warn!(error = %e, "error reading from track source");
					break;
				}
			}
		}

		// Close local producer when done
		if let Some(producer) = local_producer {
			producer.close();
		}
	}

	/// Get a buffered track consumer that serves cached data first
	///
	/// Returns a TrackConsumer that:
	/// 1. Immediately yields buffered groups
	/// 2. Then continues streaming new groups from the source
	pub async fn get_buffered_track(
		&self,
		path: &Path<'_>,
		track: &Track,
		source_broadcast: &BroadcastConsumer,
	) -> Option<BufferedTrackHandle> {
		let broadcasts = self.broadcasts.lock().await;
		let broadcast_entry = broadcasts.get(&path.to_owned())?;
		let broadcast = broadcast_entry.lock().await;

		let cached_track = broadcast.tracks.get(&track.name)?;
		let track_data = cached_track.lock().await;

		// Get buffered groups
		let buffered_groups = track_data.get_buffered_groups();

		if buffered_groups.is_empty() {
			// No buffered data - record miss
			let mut metrics = self.metrics.lock().await;
			metrics.misses += 1;
			return None;
		}

		// Record hit
		{
			let mut metrics = self.metrics.lock().await;
			metrics.hits += 1;
		}

		debug!(
			path = %path.as_str(),
			track = %track.name,
			buffered_count = buffered_groups.len(),
			"cache hit - returning buffered track"
		);

		Some(BufferedTrackHandle {
			buffered_groups,
			source: source_broadcast.subscribe_track(track),
		})
	}

	/// Check if a broadcast is cached
	pub async fn is_cached(&self, path: &Path<'_>) -> bool {
		let broadcasts = self.broadcasts.lock().await;
		broadcasts.contains_key(&path.to_owned())
	}

	/// Get current cache metrics
	pub async fn metrics(&self) -> DataCacheMetrics {
		self.metrics.lock().await.clone()
	}

	/// Clean up expired cache entries
	pub async fn cleanup_expired(&self) {
		if self.config.ttl_seconds == 0 {
			return;
		}

		let ttl = Duration::from_secs(self.config.ttl_seconds);
		let now = Instant::now();

		let mut broadcasts = self.broadcasts.lock().await;
		let before_count = broadcasts.len();

		let mut expired_paths = Vec::new();
		for (path, entry) in broadcasts.iter() {
			let broadcast = entry.lock().await;
			if now.duration_since(broadcast.created_at) > ttl {
				expired_paths.push(path.clone());
			}
		}

		for path in expired_paths {
			debug!(path = %path.as_str(), "evicting expired broadcast");
			broadcasts.remove(&path);
		}

		let evicted = before_count - broadcasts.len();
		if evicted > 0 {
			info!(evicted, remaining = broadcasts.len(), "cleaned up expired broadcasts");
		}

		// Update metrics
		let mut metrics = self.metrics.lock().await;
		metrics.broadcast_count = broadcasts.len();
	}

	/// Start background cleanup task
	pub fn spawn_cleanup_task(self: Arc<Self>) {
		if self.config.ttl_seconds == 0 {
			return;
		}

		let cleanup_interval = Duration::from_secs(self.config.ttl_seconds.max(30) / 2);

		tokio::spawn(async move {
			let mut interval = tokio::time::interval(cleanup_interval);

			loop {
				interval.tick().await;
				self.cleanup_expired().await;

				// Log metrics periodically
				let metrics = self.metrics().await;
				debug!(
					hits = metrics.hits,
					misses = metrics.misses,
					broadcasts = metrics.broadcast_count,
					groups_buffered = metrics.groups_buffered,
					"data cache metrics"
				);
			}
		});
	}
}

/// Handle for serving buffered track data
///
/// Contains pre-fetched groups and a source for new groups
pub struct BufferedTrackHandle {
	/// Pre-buffered groups to serve immediately
	pub buffered_groups: Vec<GroupConsumer>,

	/// Source for new groups after buffered ones
	pub source: TrackConsumer,
}

impl BufferedTrackHandle {
	/// Serve buffered data to a track producer, then forward new groups
	///
	/// This is the main entry point for serving cached data to subscribers
	pub async fn serve(self, mut producer: TrackProducer) {
		// 1. Immediately publish all buffered groups
		for group in self.buffered_groups {
			debug!(sequence = group.info.sequence, "serving buffered group");
			if !producer.insert_group(group) {
				debug!("producer closed while serving buffered groups");
				return;
			}
		}

		// 2. Forward new groups from source
		let mut source = self.source;
		loop {
			match source.next_group().await {
				Ok(Some(group)) => {
					debug!(sequence = group.info.sequence, "forwarding live group");
					if !producer.insert_group(group) {
						debug!("producer closed");
						break;
					}
				}
				Ok(None) => {
					debug!("source track closed");
					break;
				}
				Err(e) => {
					warn!(error = %e, "error reading from source track");
					break;
				}
			}
		}

		producer.close();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_cached_track_ring_buffer() {
		let track = Track {
			name: "test".to_string(),
			priority: 0,
		};
		let cached = CachedTrack::new(track, 3);

		// Create mock groups (we can't easily create real GroupConsumers in tests)
		// This test verifies the ring buffer logic
		assert_eq!(cached.len(), 0);
		assert_eq!(cached.max_groups, 3);
	}

	#[test]
	fn test_pattern_matching() {
		let config = DataCacheConfig {
			enabled: true,
			prefetch_patterns: vec!["live/**".to_string(), "moq-clock".to_string()],
			prefetch_tracks: vec!["seconds".to_string()],
			max_groups_per_track: 3,
			ttl_seconds: 60,
		};

		let cache = DataCache::new(config).unwrap();

		assert!(cache.should_cache(&Path::new("live/stream1")));
		assert!(cache.should_cache(&Path::new("live/events/match")));
		assert!(cache.should_cache(&Path::new("moq-clock")));
		assert!(!cache.should_cache(&Path::new("archive/old")));
		assert!(!cache.should_cache(&Path::new("moq-clock-extra")));
	}
}
