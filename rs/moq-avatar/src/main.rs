//! moq-avatar: Multi-track bidirectional avatar simulation for MoQ metaverse research
//!
//! This tool simulates metaverse avatar communication with:
//! - Multiple tracks (position, state) per avatar
//! - Bidirectional communication (each client publishes AND subscribes)
//! - Latency measurement for research purposes

use anyhow::Context;
use clap::Parser;
use moq_lite::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use url::Url;

mod avatar;
use avatar::{tracks, AvatarState, LatencyStats, Position};

#[derive(Parser, Clone)]
#[command(name = "moq-avatar")]
#[command(about = "Multi-track bidirectional avatar simulation for MoQ metaverse research")]
pub struct Config {
    /// Connect to the given URL starting with https://
    #[arg(long)]
    pub url: Url,

    /// Unique client ID (e.g., "alice", "bob")
    #[arg(long)]
    pub client_id: String,

    /// Client IDs to subscribe to (comma-separated, e.g., "bob,charlie")
    #[arg(long, value_delimiter = ',')]
    pub subscribe_to: Vec<String>,

    /// The MoQ client configuration
    #[command(flatten)]
    pub client: moq_native::ClientConfig,

    /// The log configuration
    #[command(flatten)]
    pub log: moq_native::Log,

    /// Position update interval in milliseconds (default: 100ms = 10Hz)
    #[arg(long, default_value = "100")]
    pub position_interval_ms: u64,

    /// State update interval in milliseconds (default: 1000ms = 1Hz)
    #[arg(long, default_value = "1000")]
    pub state_interval_ms: u64,

    /// Duration to run in seconds (0 = infinite)
    #[arg(long, default_value = "0")]
    pub duration_secs: u64,
}

/// Configuration for publisher (extracted from main config)
#[derive(Clone)]
struct PublisherConfig {
    client_id: String,
    position_interval_ms: u64,
    state_interval_ms: u64,
}

/// Shared statistics across all tracks
struct Stats {
    position: LatencyStats,
    state: LatencyStats,
}

impl Stats {
    fn new() -> Self {
        Self {
            position: LatencyStats::default(),
            state: LatencyStats::default(),
        }
    }

    fn print_all(&self) {
        self.position.print_summary("Position");
        self.state.print_summary("State");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    config.log.init();

    // Clone config before consuming client
    let pub_config = PublisherConfig {
        client_id: config.client_id.clone(),
        position_interval_ms: config.position_interval_ms,
        state_interval_ms: config.state_interval_ms,
    };
    let subscribe_to = config.subscribe_to.clone();
    let duration_secs = config.duration_secs;
    let url = config.url.clone();

    let client = config.client.init()?;

    tracing::info!(
        url = ?url,
        client_id = %pub_config.client_id,
        subscribe_to = ?subscribe_to,
        "starting avatar client"
    );

    let session = client.connect(url).await?;

    // Create origin for bidirectional communication
    let origin = moq_lite::Origin::produce();

    // Connect with BOTH producer (for publishing) and consumer (for subscribing)
    let session = moq_lite::Session::connect(session, origin.consumer.clone(), Some(origin.producer.clone())).await?;

    let stats = Arc::new(RwLock::new(Stats::new()));

    // Spawn publisher task (publishes our avatar data)
    let _publisher_handle = tokio::spawn({
        let pub_config = pub_config.clone();
        let origin_producer = origin.producer.clone();
        async move {
            if let Err(e) = run_publisher(pub_config, origin_producer).await {
                tracing::error!("Publisher error: {:?}", e);
            }
        }
    });

    // Spawn subscriber tasks for each remote client
    let _subscriber_handles: Vec<_> = subscribe_to
        .iter()
        .map(|remote_id| {
            let remote_id = remote_id.clone();
            let origin_consumer = origin.consumer.clone();
            let stats = stats.clone();
            tokio::spawn(async move {
                if let Err(e) = run_subscriber(remote_id.clone(), origin_consumer, stats).await {
                    tracing::error!(remote_id = %remote_id, "Subscriber error: {:?}", e);
                }
            })
        })
        .collect();

    // Handle duration limit or run forever
    let run_future = async {
        if duration_secs > 0 {
            tokio::time::sleep(Duration::from_secs(duration_secs)).await;
            tracing::info!("Duration limit reached, shutting down...");
        } else {
            // Wait for session to close
            let _ = session.closed().await;
        }
    };

    tokio::select! {
        _ = run_future => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Ctrl+C received, shutting down...");
        }
    }

    // Print statistics
    let stats = stats.read().await;
    stats.print_all();

    Ok(())
}

/// Publishes avatar data with multiple tracks
async fn run_publisher(config: PublisherConfig, origin_producer: OriginProducer) -> anyhow::Result<()> {
    let broadcast_path = format!("avatar/{}", config.client_id);

    // Create broadcast with multiple tracks
    let mut broadcast = moq_lite::Broadcast::produce();

    // Create position track (high priority, high frequency)
    let position_track = broadcast.producer.create_track(Track {
        name: tracks::POSITION.to_string(),
        priority: 0, // High priority
    });

    // Create state track (lower priority, lower frequency)
    let state_track = broadcast.producer.create_track(Track {
        name: tracks::STATE.to_string(),
        priority: 1, // Lower priority
    });

    // Publish broadcast to origin
    origin_producer.publish_broadcast(&broadcast_path, broadcast.consumer);

    tracing::info!(
        broadcast = %broadcast_path,
        "publishing avatar with tracks: [{}, {}]",
        tracks::POSITION,
        tracks::STATE
    );

    // Run track publishers concurrently
    tokio::select! {
        res = run_position_publisher(position_track, config.position_interval_ms) => {
            res.context("position publisher failed")?;
        }
        res = run_state_publisher(state_track, config.state_interval_ms) => {
            res.context("state publisher failed")?;
        }
    }

    Ok(())
}

/// Publishes position data at high frequency
async fn run_position_publisher(mut track: TrackProducer, interval_ms: u64) -> anyhow::Result<()> {
    let mut sequence = 0u64;
    let mut rng = StdRng::from_entropy();

    // Simulate starting position
    let mut x: f32 = 0.0;
    let mut y: f32 = 0.0;
    let mut z: f32 = 0.0;

    let interval = Duration::from_millis(interval_ms);

    loop {
        // Simulate movement (random walk)
        x += rng.gen_range(-0.5..0.5);
        y += rng.gen_range(-0.1..0.1); // Less vertical movement
        z += rng.gen_range(-0.5..0.5);

        let position = Position::new(x, y, z);
        let data = position.to_bytes();

        let mut group = track.create_group(sequence.into()).context("failed to create group")?;
        group.write_frame(data);
        group.close();

        tracing::debug!(
            seq = sequence,
            x = x,
            y = y,
            z = z,
            "sent position"
        );

        sequence += 1;
        tokio::time::sleep(interval).await;
    }
}

/// Publishes state data at lower frequency
async fn run_state_publisher(mut track: TrackProducer, interval_ms: u64) -> anyhow::Result<()> {
    let mut sequence = 0u64;
    let mut rng = StdRng::from_entropy();

    let animations = ["idle", "walking", "running", "jumping", "waving"];
    let interval = Duration::from_millis(interval_ms);

    loop {
        // Randomly select animation
        let animation = animations[rng.gen_range(0..animations.len())];
        let status = rng.gen_range(50..100);

        let state = AvatarState::new(animation, status);
        let data = state.to_bytes();

        let mut group = track.create_group(sequence.into()).context("failed to create group")?;
        group.write_frame(data);
        group.close();

        tracing::debug!(
            seq = sequence,
            animation = animation,
            status = status,
            "sent state"
        );

        sequence += 1;
        tokio::time::sleep(interval).await;
    }
}

/// Subscribes to a remote client's avatar data
async fn run_subscriber(
    remote_id: String,
    mut origin_consumer: OriginConsumer,
    stats: Arc<RwLock<Stats>>,
) -> anyhow::Result<()> {
    let broadcast_path = format!("avatar/{}", remote_id);

    tracing::info!(broadcast = %broadcast_path, "waiting for remote avatar to come online");

    // Wait for the broadcast to be announced
    loop {
        if let Some((path, maybe_broadcast)) = origin_consumer.announced().await {
            let path_str = path.to_string();
            if path_str == broadcast_path {
                if let Some(broadcast) = maybe_broadcast {
                    tracing::info!(broadcast = %path_str, "remote avatar is online, subscribing to tracks");

                    // Subscribe to both tracks
                    let position_track = broadcast.subscribe_track(&Track {
                        name: tracks::POSITION.to_string(),
                        priority: 0,
                    });

                    let state_track = broadcast.subscribe_track(&Track {
                        name: tracks::STATE.to_string(),
                        priority: 1,
                    });

                    // Run track subscribers concurrently
                    let stats_pos = stats.clone();
                    let stats_state = stats.clone();
                    let remote_id_pos = remote_id.clone();
                    let remote_id_state = remote_id.clone();

                    tokio::select! {
                        res = run_position_subscriber(position_track, remote_id_pos, stats_pos) => {
                            tracing::warn!("position subscriber ended: {:?}", res);
                        }
                        res = run_state_subscriber(state_track, remote_id_state, stats_state) => {
                            tracing::warn!("state subscriber ended: {:?}", res);
                        }
                    }
                } else {
                    tracing::warn!(broadcast = %path_str, "remote avatar went offline");
                }
            }
        }
    }
}

/// Receives position data from remote avatar
async fn run_position_subscriber(
    mut track: TrackConsumer,
    remote_id: String,
    stats: Arc<RwLock<Stats>>,
) -> anyhow::Result<()> {
    while let Some(mut group) = track.next_group().await? {
        while let Some(frame) = group.read_frame().await? {
            match Position::from_bytes(&frame) {
                Ok(position) => {
                    let latency = position.latency_ms();

                    // Record stats
                    {
                        let mut stats = stats.write().await;
                        stats.position.record(latency);
                    }

                    tracing::info!(
                        remote = %remote_id,
                        track = tracks::POSITION,
                        x = position.x,
                        y = position.y,
                        z = position.z,
                        latency_ms = format!("{:.2}", latency),
                        "received position"
                    );
                }
                Err(e) => {
                    tracing::warn!("failed to parse position: {:?}", e);
                }
            }
        }
    }

    Ok(())
}

/// Receives state data from remote avatar
async fn run_state_subscriber(
    mut track: TrackConsumer,
    remote_id: String,
    stats: Arc<RwLock<Stats>>,
) -> anyhow::Result<()> {
    while let Some(mut group) = track.next_group().await? {
        while let Some(frame) = group.read_frame().await? {
            match AvatarState::from_bytes(&frame) {
                Ok(state) => {
                    let latency = state.latency_ms();

                    // Record stats
                    {
                        let mut stats = stats.write().await;
                        stats.state.record(latency);
                    }

                    tracing::info!(
                        remote = %remote_id,
                        track = tracks::STATE,
                        animation = %state.animation,
                        status = state.status,
                        latency_ms = format!("{:.2}", latency),
                        "received state"
                    );
                }
                Err(e) => {
                    tracing::warn!("failed to parse state: {:?}", e);
                }
            }
        }
    }

    Ok(())
}
