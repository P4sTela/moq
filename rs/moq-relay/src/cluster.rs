use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Context;
use moq_lite::{Broadcast, BroadcastConsumer, BroadcastProducer, Origin, OriginConsumer, OriginProducer, Path};
use tracing::Instrument;
use url::Url;

use crate::{AuthToken, PredictiveCache};

#[serde_with::serde_as]
#[derive(clap::Args, Clone, Debug, serde::Serialize, serde::Deserialize, Default)]
#[serde_with::skip_serializing_none]
#[serde(default, deny_unknown_fields)]
pub struct ClusterConfig {
	/// Connect to this hostname in order to discover other nodes.
	#[serde(alias = "connect")]
	#[arg(
		id = "cluster-root",
		long = "cluster-root",
		env = "MOQ_CLUSTER_ROOT",
		alias = "cluster-connect"
	)]
	pub root: Option<String>,

	/// Use the token in this file when connecting to other nodes.
	#[arg(id = "cluster-token", long = "cluster-token", env = "MOQ_CLUSTER_TOKEN")]
	pub token: Option<PathBuf>,

	/// Our hostname which we advertise to other nodes.
	///
	// TODO Remove alias once we've migrated to the new name.
	#[serde(alias = "advertise")]
	#[arg(
		id = "cluster-node",
		long = "cluster-node",
		env = "MOQ_CLUSTER_NODE",
		alias = "cluster-advertise"
	)]
	pub node: Option<String>,

	/// The prefix to use for cluster announcements.
	/// Defaults to "internal/origins".
	///
	/// WARNING: This should not be accessible by users unless authentication is disabled (YOLO).
	#[arg(
		id = "cluster-prefix",
		long = "cluster-prefix",
		default_value = "internal/origins",
		env = "MOQ_CLUSTER_PREFIX"
	)]
	pub prefix: String,

	/// Predictive cache configuration for edge relay (TOML only)
	#[arg(skip)]
	#[serde(default)]
	pub predictive_cache: PredictiveCacheConfig,
}

/// Predictive cache configuration for edge relay (TOML only, no CLI args)
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PredictiveCacheConfig {
	/// Enable predictive caching (prefetch content based on ANNOUNCE)
	pub enabled: bool,

	/// Glob patterns for broadcasts to prefetch (e.g., "live/**", "sports/*")
	pub prefetch_patterns: Vec<String>,

	/// How many seconds to look ahead for prefetching (0 = unlimited)
	pub lookahead_seconds: u64,

	/// Maximum cache size in bytes (0 = unlimited)
	pub max_cache_bytes: u64,

	/// Time-to-live for cached content in seconds (0 = unlimited)
	pub ttl_seconds: u64,
}

impl Default for PredictiveCacheConfig {
	fn default() -> Self {
		Self {
			enabled: false,
			prefetch_patterns: Vec::new(),
			lookahead_seconds: 0,
			max_cache_bytes: 0,
			ttl_seconds: 3600,
		}
	}
}

#[derive(Clone)]
pub struct Cluster {
	config: ClusterConfig,
	client: moq_native::Client,

	// Advertises ourselves as an origin to other nodes.
	noop: moq_lite::Produce<BroadcastProducer, BroadcastConsumer>,

	// Broadcasts announced by local clients (users).
	pub primary: Arc<moq_lite::Produce<OriginProducer, OriginConsumer>>,

	// Broadcasts announced by remote servers (cluster).
	pub secondary: Arc<moq_lite::Produce<OriginProducer, OriginConsumer>>,

	// Broadcasts announced by local clients and remote servers.
	pub combined: Arc<moq_lite::Produce<OriginProducer, OriginConsumer>>,

	// Predictive cache for edge relay (optional)
	pub predictive_cache: Option<Arc<PredictiveCache>>,
}

impl Cluster {
	/// Get cluster configuration
	pub fn config(&self) -> &ClusterConfig {
		&self.config
	}

	pub fn new(config: ClusterConfig, client: moq_native::Client) -> Self {
		// Log full cluster configuration for debugging
		tracing::info!(
			config = ?config,
			"cluster configuration loaded"
		);

		// Initialize predictive cache if enabled
		let predictive_cache = if config.predictive_cache.enabled {
			tracing::info!("predictive cache is enabled, initializing...");
			match PredictiveCache::new(config.predictive_cache.clone()) {
				Ok(cache) => {
					let cache = Arc::new(cache);
					// Spawn background cleanup task
					cache.clone().spawn_cleanup_task();
					tracing::info!("predictive cache successfully initialized");
					Some(cache)
				}
				Err(e) => {
					tracing::warn!("Failed to initialize predictive cache: {}", e);
					None
				}
			}
		} else {
			tracing::info!("predictive cache is disabled");
			None
		};

		Cluster {
			config,
			client,
			noop: Broadcast::produce(),
			primary: Arc::new(Origin::produce()),
			secondary: Arc::new(Origin::produce()),
			combined: Arc::new(Origin::produce()),
			predictive_cache,
		}
	}

	// For a given auth token, return the origin that should be used for the session.
	pub fn subscriber(&self, token: &AuthToken) -> Option<OriginConsumer> {
		// 🔑 Leaf Pull Implementation: Always return combined origin
		//
		// This allows Leaf nodes to receive ALL broadcasts from the Root node,
		// enabling Root → Leaf ANNOUNCE propagation.
		//
		// Flow:
		// 1. Leaf connects to Root with cluster=true JWT
		// 2. Root's subscriber() returns combined origin (not primary)
		// 3. Leaf receives all broadcasts: primary (local clients) + secondary (other Leafs)
		// 4. ANNOUNCE propagation: Leaf1 → Root → Leaf2 ✅
		//
		// Previous behavior (token.cluster check) caused Leaf to only see primary,
		// preventing Root → Leaf propagation.
		let subscribe_origin = &self.combined;

		// Scope the origin to our root.
		let subscribe_origin = subscribe_origin.producer.with_root(&token.root)?;
		subscribe_origin.consume_only(&token.subscribe)
	}

	pub fn publisher(&self, token: &AuthToken) -> Option<OriginProducer> {
		// If this is a cluster node, then add its broadcasts to the secondary origin.
		// That way we won't publish them to other cluster nodes.
		let publish_origin = match token.cluster {
			true => &self.secondary,
			false => &self.primary,
		};

		let publish_origin = publish_origin.producer.with_root(&token.root)?;
		publish_origin.publish_only(&token.publish)
	}

	pub fn get(&self, broadcast: &str) -> Option<BroadcastConsumer> {
		// 🚀 Predictive Cache: キャッシュから取得を試みる
		if let Some(cache) = &self.predictive_cache {
			let path = Path::from(broadcast);
			if let Some(cached) = cache.get(&path) {
				tracing::debug!(
					broadcast = %broadcast,
					"predictive cache: serving from cache (HIT)"
				);
				return Some(cached);
			} else {
				tracing::debug!(
					broadcast = %broadcast,
					"predictive cache: not in cache (MISS)"
				);
			}
		}

		// キャッシュにない場合は通常のフォールバック
		self.primary
			.consumer
			.consume_broadcast(broadcast)
			.or_else(|| self.secondary.consumer.consume_broadcast(broadcast))
	}

	pub async fn run(self) -> anyhow::Result<()> {
		let root = match self.config.root.clone() {
			// If we're using a root node, then we have to connect to it.
			Some(connect) if Some(&connect) != self.config.node.as_ref() => connect,
			// Otherwise, we're the root node so we wait for other nodes to connect to us.
			_ => {
				tracing::info!("running as root, accepting leaf nodes");
				self.run_combined().await?;
				anyhow::bail!("combined connection closed");
			}
		};

		// Subscribe to available origins.
		// Use with_root to automatically strip the prefix from announced paths.
		let origins = self
			.secondary
			.producer
			.with_root(&self.config.prefix)
			.context("no authorized origins")?;

		// Announce ourselves as an origin to the root node.
		if let Some(myself) = self.config.node.as_ref() {
			tracing::info!(%myself, "announcing as leaf");
			origins.publish_broadcast(myself, self.noop.consumer.clone());
		}

		// If the token is provided, read it from the disk and use it in the query parameter.
		// TODO put this in an AUTH header once WebTransport supports it.
		let token = match &self.config.token {
			Some(path) => std::fs::read_to_string(path).context("failed to read token")?,
			None => "".to_string(),
		};

		let noop = self.noop.consumer.clone();

		// Despite returning a Result, we should NEVER return an Ok
		tokio::select! {
			res = self.clone().run_remote(&root, token.clone(), noop) => {
				res.context("failed to connect to root")?;
				anyhow::bail!("connection to root closed");
			}
			res = self.clone().run_remotes(origins.consume(), token) => {
				res.context("failed to connect to remotes")?;
				anyhow::bail!("connection to remotes closed");
			}
			res = self.run_combined() => {
				res.context("failed to run combined")?;
				anyhow::bail!("combined connection closed");
			}
		}
	}

	// Shovel broadcasts from the primary and secondary origins into the combined origin.
	async fn run_combined(self) -> anyhow::Result<()> {
		tracing::debug!("run_combined: starting");
		let mut primary = self.primary.consumer.consume();
		let mut secondary = self.secondary.consumer.consume();

		loop {
			let (name, broadcast, from_secondary) = tokio::select! {
				biased;
				Some((name, broadcast)) = primary.announced() => {
					tracing::debug!(source = "primary", broadcast = %name.as_str(), "run_combined: received announcement");
					(name, broadcast, false)
				},
				Some((name, broadcast)) = secondary.announced() => {
					tracing::debug!(source = "secondary", broadcast = %name.as_str(), "run_combined: received announcement");
					(name, broadcast, true)
				},
				else => {
					tracing::debug!("run_combined: all sources closed");
					return Ok(());
				}
			};

			if let Some(broadcast) = broadcast {
				tracing::debug!(broadcast = %name.as_str(), "run_combined: publishing to combined origin");

				// 🚀 Predictive Cache: Check cache first, then prefetch if needed
				let broadcast_to_publish = if from_secondary {
					if let Some(cache) = &self.predictive_cache {
						let path = Path::from(name.as_str());

						// Try to get from cache first (cache hit)
						if let Some(cached) = cache.get(&path) {
							tracing::debug!(
								broadcast = %name.as_str(),
								"predictive cache: serving from cache (HIT)"
							);
							cached
						} else {
							// Cache miss - prefetch for future use
							if cache.should_prefetch(&name) {
								tracing::debug!(
									broadcast = %name.as_str(),
									"predictive cache: prefetching broadcast from Root (MISS)"
								);

								// Clone broadcast consumer for caching
								let cached_consumer = broadcast.clone();
								let cache_clone = cache.clone();
								let name_clone = name.clone();

								// Spawn prefetch task asynchronously
								tokio::spawn(async move {
									if let Err(e) = cache_clone.prefetch(name_clone.clone(), cached_consumer).await {
										tracing::warn!(
											broadcast = %name_clone.as_str(),
											error = %e,
											"predictive cache: prefetch failed"
										);
									}
								});
							}
							broadcast
						}
					} else {
						broadcast
					}
				} else {
					broadcast
				};

				self.combined.producer.publish_broadcast(&name, broadcast_to_publish);
			}
		}
	}

	async fn run_remotes(self, mut origins: OriginConsumer, token: String) -> anyhow::Result<()> {
		// Cancel tasks when the origin is closed.
		let mut active: HashMap<String, tokio::task::AbortHandle> = HashMap::new();

		// Discover other origins.
		// NOTE: The root node will connect to all other nodes as a client, ignoring the existing (server) connection.
		// This ensures that nodes are advertising a valid hostname before any tracks get announced.
		while let Some((node, origin)) = origins.announced().await {
			if Some(node.as_str()) == self.config.node.as_deref() {
				// Skip ourselves.
				continue;
			}

			let origin = match origin {
				Some(origin) => origin,
				None => {
					tracing::info!(%node, "origin cancelled");
					active.remove(node.as_str()).unwrap().abort();
					continue;
				}
			};

			tracing::info!(%node, "discovered origin");

			let this = self.clone();
			let token = token.clone();
			let node2 = node.clone();

			let handle = tokio::spawn(
				async move {
					match this.run_remote(node2.as_str(), token, origin).await {
						Ok(()) => tracing::info!(%node2, "origin closed"),
						Err(err) => tracing::warn!(%err, %node2, "origin error"),
					}
				}
				.in_current_span(),
			);

			active.insert(node.to_string(), handle.abort_handle());
		}

		Ok(())
	}

	#[tracing::instrument("remote", skip_all, err, fields(%node))]
	async fn run_remote(mut self, node: &str, token: String, origin: BroadcastConsumer) -> anyhow::Result<()> {
		let url = Url::parse(&format!("https://{node}/?jwt={token}"))?;
		let mut backoff = 1;

		loop {
			let res = tokio::select! {
				biased;
				_ = origin.closed() => break,
				res = self.run_remote_once(&url) => res,
			};

			if let Err(err) = res {
				backoff *= 2;
				tracing::error!(%err, "remote error");
			}

			let timeout = tokio::time::Duration::from_secs(backoff);
			if timeout > tokio::time::Duration::from_secs(300) {
				// 5 minutes of backoff is enough, just give up.
				// TODO Reset the backoff if the connect is successful for some period of time.
				anyhow::bail!("remote connection keep failing, giving up");
			}

			tokio::time::sleep(timeout).await;
		}

		Ok(())
	}

	async fn run_remote_once(&mut self, url: &Url) -> anyhow::Result<()> {
		tracing::info!(%url, "connecting to remote");

		// Connect to the remote node.
		let conn = self
			.client
			.connect(url.clone())
			.await
			.context("failed to connect to remote")?;

		let publish = Some(self.primary.consumer.consume());
		let subscribe = Some(self.secondary.producer.clone());

		let session = moq_lite::Session::connect(conn, publish, subscribe)
			.await
			.context("failed to establish session")?;

		session.closed().await.map_err(Into::into)
	}
}
