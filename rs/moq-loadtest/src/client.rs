use crate::metrics::Metrics;
use anyhow::Context;
use moq_lite::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use url::Url;

#[derive(serde::Serialize, serde::Deserialize)]
struct Payload {
    id: String,
    seq: u64,
    ts: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pad: Option<String>,
}

pub struct VirtualClient {
    pub id: String,
    metrics: Arc<Metrics>,
    publish_rate_hz: u32,
    payload_bytes: usize,
    cancel_tx: watch::Sender<bool>,
    cancel_rx: watch::Receiver<bool>,
    _session: Option<moq_lite::Session<web_transport_quinn::Session>>,
    subscribe_origin: Option<Produce<OriginProducer, OriginConsumer>>,
    // Deferred publish: origin + broadcast + track stored until start_publisher()
    publish_origin: Option<Produce<OriginProducer, OriginConsumer>>,
    // Must keep BroadcastProducer alive — its Drop clears published track state
    _broadcast_producer: Option<BroadcastProducer>,
    broadcast_consumer: Option<BroadcastConsumer>,
    track_producer: Option<TrackProducer>,
}

impl VirtualClient {
    pub fn new(id: String, metrics: Arc<Metrics>, publish_rate_hz: u32, payload_bytes: usize) -> Self {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        Self {
            id,
            metrics,
            publish_rate_hz,
            payload_bytes,
            cancel_tx,
            cancel_rx,
            _session: None,
            subscribe_origin: None,
            publish_origin: None,
            _broadcast_producer: None,
            broadcast_consumer: None,
            track_producer: None,
        }
    }

    pub async fn connect(&mut self, url: Url, client: &moq_native::Client) -> anyhow::Result<()> {
        let start = std::time::Instant::now();

        match self.try_connect(url, client).await {
            Ok(()) => {
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                self.metrics.record_connection(&self.id, true, elapsed);
                Ok(())
            }
            Err(e) => {
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                self.metrics.record_connection(&self.id, false, elapsed);
                Err(e)
            }
        }
    }

    async fn try_connect(&mut self, url: Url, client: &moq_native::Client) -> anyhow::Result<()> {
        let session = client.connect(url).await.context("QUIC connect failed")?;

        // Dual origin: publish and subscribe separated to avoid ANNOUNCE feedback loop
        let publish_origin = moq_lite::Origin::produce();
        let subscribe_origin = moq_lite::Origin::produce();

        let session = moq_lite::Session::connect(
            session,
            publish_origin.consumer.clone(),
            Some(subscribe_origin.producer.clone()),
        )
        .await
        .context("MoQ session connect failed")?;

        // Create broadcast + track but do NOT publish to origin yet.
        // This defers the ANNOUNCE until start_publisher() is called,
        // so subscribers can set up announced() listeners first.
        let mut broadcast = moq_lite::Broadcast::produce();
        let track = broadcast.producer.create_track(Track {
            name: "data".to_string(),
            priority: 0,
        });

        self._session = Some(session);
        self.subscribe_origin = Some(subscribe_origin);
        self.publish_origin = Some(publish_origin);
        self._broadcast_producer = Some(broadcast.producer);
        self.broadcast_consumer = Some(broadcast.consumer);
        self.track_producer = Some(track);

        Ok(())
    }

    pub fn start_publisher(&mut self) -> tokio::task::JoinHandle<()> {
        let track = self
            .track_producer
            .take()
            .expect("connect() must be called first");

        // NOW publish the broadcast to origin (triggers ANNOUNCE to relay)
        let publish_origin = self.publish_origin.as_ref().expect("connect() must be called first");
        let broadcast_consumer = self.broadcast_consumer.take().expect("connect() must be called first");
        let broadcast_path = format!("loadtest/{}", self.id);
        publish_origin
            .producer
            .publish_broadcast(&broadcast_path, broadcast_consumer);

        let id = self.id.clone();
        let metrics = self.metrics.clone();
        let rate_hz = self.publish_rate_hz;
        let payload_bytes = self.payload_bytes;
        let cancel_rx = self.cancel_rx.clone();

        tokio::spawn(async move {
            if let Err(e) = run_publisher(id, track, metrics, rate_hz, payload_bytes, cancel_rx).await {
                tracing::warn!("publisher error: {:?}", e);
            }
        })
    }

    /// Start subscriber tasks for each peer
    pub fn start_subscribers(&self, peer_ids: &[String]) -> Vec<tokio::task::JoinHandle<()>> {
        let origin_consumer = self
            .subscribe_origin
            .as_ref()
            .expect("connect() must be called first")
            .consumer
            .clone();

        peer_ids
            .iter()
            .map(|peer_id| {
                let peer_id = peer_id.clone();
                let my_id = self.id.clone();
                let metrics = self.metrics.clone();
                let origin = origin_consumer.clone();
                let cancel_rx = self.cancel_rx.clone();

                tokio::spawn(async move {
                    if let Err(e) =
                        run_subscriber(peer_id.clone(), my_id, origin, metrics, cancel_rx).await
                    {
                        tracing::debug!(peer = %peer_id, "subscriber ended: {:?}", e);
                    }
                })
            })
            .collect()
    }

    pub fn stop(&self) {
        let _ = self.cancel_tx.send(true);
    }
}

async fn run_publisher(
    id: String,
    mut track: TrackProducer,
    metrics: Arc<Metrics>,
    rate_hz: u32,
    payload_bytes: usize,
    mut cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // Build padding to reach target payload size
    let base_size = serde_json::to_vec(&Payload {
        id: id.clone(),
        seq: 0,
        ts: 0,
        pad: None,
    })?
    .len();
    let pad_length = payload_bytes.saturating_sub(base_size + 10);
    let pad = if pad_length > 0 {
        Some("x".repeat(pad_length))
    } else {
        None
    };

    let mut seq: u64 = 0;
    let mut interval = tokio::time::interval(Duration::from_micros(1_000_000 / rate_hz as u64));

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = cancel_rx.changed() => break,
        }

        let payload = Payload {
            id: id.clone(),
            seq,
            ts: chrono::Utc::now().timestamp_millis(),
            pad: pad.clone(),
        };

        let data = serde_json::to_vec(&payload)?;

        if let Some(mut group) = track.create_group(seq.into()) {
            group.write_frame(data);
            group.close();
        }

        metrics.record_publish(&id);
        seq += 1;
    }

    Ok(())
}

/// Subscriber for a single peer.
/// Discovers broadcast, then retries track subscription with backoff.
async fn run_subscriber(
    peer_id: String,
    my_id: String,
    mut origin_consumer: OriginConsumer,
    metrics: Arc<Metrics>,
    mut cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let broadcast_path = format!("loadtest/{}", peer_id);

    // Get the broadcast consumer (waits for announce if needed)
    let broadcast = match get_broadcast(&broadcast_path, &mut origin_consumer, &mut cancel_rx).await {
        Some(bc) => bc,
        None => return Ok(()),
    };

    // Retry with new BroadcastConsumer each time (via consume_broadcast)
    let max_retries = 15u32;
    let base_delay = Duration::from_millis(300);

    for attempt in 0..=max_retries {
        if *cancel_rx.borrow() {
            return Ok(());
        }

        // Use consume_broadcast for retries to get a fresh BroadcastConsumer
        let bc = if attempt == 0 {
            broadcast.clone()
        } else {
            match origin_consumer.consume_broadcast(&broadcast_path) {
                Some(bc) => bc,
                None => {
                    tracing::debug!(peer = %peer_id, "broadcast no longer available");
                    return Ok(());
                }
            }
        };

        let track = bc.subscribe_track(&Track {
            name: "data".to_string(),
            priority: 0,
        });

        match consume_track(track, &peer_id, &my_id, &metrics, &mut cancel_rx).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if *cancel_rx.borrow() {
                    return Ok(());
                }
                let delay = base_delay * 2u32.pow(attempt.min(4));
                tracing::debug!(
                    peer = %peer_id,
                    attempt,
                    "track subscription failed, retrying in {:?}: {:?}",
                    delay, e
                );
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel_rx.changed() => return Ok(()),
                }
            }
        }
    }

    tracing::warn!(peer = %peer_id, "subscriber gave up after {} retries", max_retries);
    Ok(())
}

/// Get a broadcast consumer for the given path.
/// First tries announced(), then polls consume_broadcast with backoff.
async fn get_broadcast(
    broadcast_path: &str,
    origin_consumer: &mut OriginConsumer,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Option<BroadcastConsumer> {
    // First: drain any pending announced events looking for our target
    loop {
        match origin_consumer.try_announced() {
            Some((path, Some(broadcast))) => {
                let path_str = path.to_string();
                if path_str == broadcast_path || path_str.ends_with(broadcast_path) {
                    return Some(broadcast);
                }
            }
            Some((_, None)) => continue,
            None => break, // no more pending events
        }
    }

    // Second: try consume_broadcast directly
    if let Some(bc) = origin_consumer.consume_broadcast(broadcast_path) {
        tracing::debug!(path = broadcast_path, "found broadcast via consume_broadcast");
        return Some(bc);
    }

    // Log allowed paths for debugging
    let allowed: Vec<_> = origin_consumer.allowed().map(|p| p.to_string()).collect();
    tracing::debug!(
        path = broadcast_path,
        ?allowed,
        root = %origin_consumer.root(),
        "broadcast not found, waiting for announce"
    );

    // Third: wait for announced() with polling fallback
    let poll_interval = Duration::from_millis(200);
    loop {
        tokio::select! {
            announce = origin_consumer.announced() => {
                match announce {
                    Some((path, Some(broadcast))) => {
                        let path_str = path.to_string();
                        tracing::debug!(
                            expected = broadcast_path,
                            got = %path_str,
                            "received announce in get_broadcast"
                        );
                        if path_str == broadcast_path || path_str.ends_with(broadcast_path) {
                            return Some(broadcast);
                        }
                    }
                    Some((_, None)) => continue,
                    None => return None,
                }
            }
            _ = tokio::time::sleep(poll_interval) => {
                // Poll consume_broadcast in case we missed the announce
                if let Some(bc) = origin_consumer.consume_broadcast(broadcast_path) {
                    tracing::debug!(path = broadcast_path, "found broadcast via polling");
                    return Some(bc);
                }
            }
            _ = cancel_rx.changed() => return None,
        }
    }
}

async fn consume_track(
    mut track: TrackConsumer,
    peer_id: &str,
    my_id: &str,
    metrics: &Arc<Metrics>,
    cancel_rx: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            group = track.next_group() => {
                match group? {
                    Some(mut group) => {
                        while let Some(frame) = group.read_frame().await? {
                            if let Ok(payload) = serde_json::from_slice::<Payload>(&frame) {
                                let now = chrono::Utc::now().timestamp_millis();
                                let latency = (now - payload.ts) as f64;
                                metrics.record_latency(latency);
                                metrics.record_receive(peer_id, my_id);
                            }
                        }
                    }
                    None => break,
                }
            }
            _ = cancel_rx.changed() => break,
        }
    }

    Ok(())
}
