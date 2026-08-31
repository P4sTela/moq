//! End-to-end smoke test through a real moq-relay.
//!
//! Stands up the relay's actual axum + auth + cluster stack on a free port,
//! connects a publisher and a subscriber via WebSocket, and confirms that
//! a frame round-trips with the newest moq-lite version on both sides. The
//! version assertion is the regression guard for the
//! "axum-only-advertises-bare-`webtransport`" bug that silently downgraded
//! relay clients to moq-lite-02.

use std::{
	net::TcpListener,
	sync::{Arc, Mutex},
	time::Duration,
};

use moq_native::moq_net::{self, Origin};
use moq_relay::{AuthConfig, CacheConfig, Cluster, ClusterConfig, Connection, PublicConfig, Web, WebConfig};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The newest moq-lite ALPN both sides should converge on. Derived from
/// `moq_net::ALPNS` so a future bump (e.g. lite-05 promoted out of WIP)
/// doesn't break this test independently of the production negotiation.
/// We filter on the `moq-lite-` prefix specifically; the relay smoke test
/// is asserting lite behavior, not IETF moqt drafts.
fn newest_lite_version() -> moq_net::Version {
	moq_net::ALPNS
		.iter()
		.copied()
		.find(|alpn| alpn.starts_with("moq-lite-"))
		.expect("no moq-lite ALPN in moq_net::ALPNS")
		.parse()
		.expect("parse newest lite ALPN as a Version")
}

async fn build_web(port: u16, ws: bool) -> Web {
	build_web_with_cache(port, ws, None).await
}

/// Build the test relay with the production cache wiring when requested.
/// `None` preserves the default unbounded pool used by the existing smoke tests;
/// `Some(duration)` additionally installs the relay's cache-duration ceiling and
/// a finite byte budget for the Phase 5 late-join cases.
async fn build_web_with_cache(port: u16, ws: bool, cache_duration: Option<Duration>) -> Web {
	build_web_with_cache_and_stats(port, ws, cache_duration, None).await.0
}

async fn build_web_with_cache_and_stats(
	port: u16,
	ws: bool,
	cache_duration: Option<Duration>,
	stats: Option<moq_net::stats::Registry>,
) -> (Web, Option<moq_net::stats::Registry>) {
	// Crypto provider is process-global; reinstalls after the first one are
	// no-ops, but the test binary may run before any other moq code does.
	let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

	// AuthConfig with public Simple([""]) lets any path through. Simple is
	// deprecated but matches what `simple_public("")` in moq-relay's auth
	// tests uses, and the relay still honors it.
	#[allow(deprecated)]
	let public = PublicConfig::Simple(vec![String::new()]);
	let mut auth_config = AuthConfig::default();
	auth_config.public = Some(public);
	let auth = auth_config
		.init(&moq_native::tls::Client::default())
		.await
		.expect("auth init");

	let mut cluster_config = ClusterConfig::default();
	// Keep an abruptly disconnected source announced long enough for a
	// late-joiner to exercise the retained relay track.
	cluster_config.linger = Some(Duration::from_secs(5));
	let mut cluster = Cluster::new(cluster_config).expect("cluster init");
	if let Some(stats) = &stats {
		cluster = cluster.with_stats(stats.clone());
	}
	if let Some(duration) = cache_duration {
		let mut cache_config = CacheConfig::default();
		cache_config.capacity = Some("64MiB".into());
		cache_config.duration = Some(duration);
		cluster = cluster.with_cache(cache_config.init().expect("cache init"));
	}

	// moq_native::Server is needed for `certificates`, even though we never
	// expose HTTPS or QUIC in this test. Binding QUIC to `[::]:0` picks an
	// unused UDP port that we ignore.
	let mut server_config = moq_native::ServerConfig::default();
	server_config.bind = Some("[::]:0".to_string());
	server_config.tls.generate = vec!["localhost".into()];
	let server = server_config.init().expect("server init");

	let mut web_config = WebConfig::default();
	web_config.ws = ws;
	web_config.http.listen = Some(format!("127.0.0.1:{port}").parse().expect("parse listen"));

	(Web::new(auth, cluster, server.certificates(), web_config), stats)
}

fn free_tcp_port() -> u16 {
	// Pick a free port for HTTP, then immediately drop the probe listener
	// so axum_server can bind it. There's a tiny race window where the
	// kernel could hand the same port to another process, but on localhost
	// in a single-test process it's safe in practice.
	let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe");
	let port = probe.local_addr().expect("local addr").port();
	drop(probe);
	port
}

async fn wait_for_http(port: u16, server_result: &mut tokio::sync::oneshot::Receiver<anyhow::Result<()>>) {
	// Wait for axum_server to bind. A short poll is more reliable than a
	// fixed sleep when CI is slow.
	let deadline = std::time::Instant::now() + Duration::from_secs(5);
	loop {
		if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
			break;
		}
		match server_result.try_recv() {
			Ok(Ok(())) => panic!("relay web server exited before listening"),
			Ok(Err(err)) => panic!("relay web server failed before listening: {err:#}"),
			Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
			Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
				panic!("relay web server task ended before listening")
			}
		}
		if std::time::Instant::now() >= deadline {
			panic!("relay http listener never became ready on port {port}");
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}
}

/// The shared bootstrap: stand up a relay listening on `127.0.0.1:<free-port>`
/// with fully public auth, and return the port plus an abort handle for the
/// spawned web server.
async fn spawn_relay() -> (u16, tokio::task::JoinHandle<()>) {
	spawn_relay_with_cache(None).await
}

async fn spawn_relay_with_cache(cache_duration: Option<Duration>) -> (u16, tokio::task::JoinHandle<()>) {
	let port = free_tcp_port();
	let web = build_web_with_cache(port, true, cache_duration).await;

	let (server_result_tx, mut server_result_rx) = tokio::sync::oneshot::channel();
	let handle = tokio::spawn(async move {
		// `Web::run` only returns on error; in tests we abort it at teardown.
		let _ = server_result_tx.send(web.run().await);
	});

	wait_for_http(port, &mut server_result_rx).await;

	(port, handle)
}

async fn spawn_relay_with_cache_and_stats(
	cache_duration: Option<Duration>,
) -> (u16, tokio::task::JoinHandle<()>, moq_net::stats::Registry) {
	let port = free_tcp_port();
	let stats = moq_net::stats::Registry::new(moq_net::stats::Config::new());
	let (web, stats) = build_web_with_cache_and_stats(port, true, cache_duration, Some(stats.clone())).await;
	let stats = stats.expect("enabled stats registry");

	let (server_result_tx, mut server_result_rx) = tokio::sync::oneshot::channel();
	let handle = tokio::spawn(async move {
		// `Web::run` only returns on error; in tests we abort it at teardown.
		let _ = server_result_tx.send(web.run().await);
	});

	wait_for_http(port, &mut server_result_rx).await;

	(port, handle, stats)
}

fn client() -> moq_native::Client {
	client_version(None)
}

/// A client pinned to a single MoQ version, or all versions when `None`.
fn client_version(version: Option<moq_net::Version>) -> moq_native::Client {
	let mut config = moq_native::ClientConfig::default();
	config.tls.disable_verify = Some(true);
	// Zero head start so the WebSocket path runs immediately.
	config.websocket.delay = None;
	// Every relay in this file listens on IPv4 loopback, so bind the same family
	// rather than egressing a QUIC dial from a dual-stack IPv6 socket.
	config.bind = "127.0.0.1:0".parse().expect("parse bind");
	if let Some(version) = version {
		config.version = vec![version];
	}
	config.init().expect("client init")
}

/// A canonical cache/late-join canary. The early subscriber forces the relay to
/// materialize both state groups, and the late subscriber starts at the live edge
/// before fetching the older retained group while the publisher remains connected.
/// Publisher-disconnect persistence is intentionally not asserted here: the
/// canonical relay releases an aborted source track when its lingered broadcast
/// eventually closes, which is a separate future-work contract.
#[tokio::test]
async fn relay_websocket_late_join_replays_retained_history_while_source_alive() {
	let peer_count = std::env::var("MOQ_PHASE5_PEER_COUNT")
		.map(|raw| raw.parse::<usize>().expect("MOQ_PHASE5_PEER_COUNT must be an integer"))
		.unwrap_or(1);
	assert!(matches!(peer_count, 1 | 4 | 8), "peer count must be one of 1, 4, or 8");

	let (port, web_handle, stats) = spawn_relay_with_cache_and_stats(Some(Duration::from_secs(5))).await;
	let url: url::Url = format!("ws://127.0.0.1:{port}/phase5").parse().expect("parse url");

	// Publish an overwrite-style current-state track with two complete groups.
	let pub_origin = Origin::random().produce();
	let mut broadcast = pub_origin
		.create_broadcast("late-join", moq_net::broadcast::Route::new().with_announce(true))
		.expect("create broadcast");
	let track_info = moq_net::track::Info::default().with_latency_max(Duration::from_secs(30));
	let mut track = broadcast
		.create_track("state", Some(track_info))
		.expect("create state track");
	let mut old_group = track.append_group().expect("append old group");
	old_group
		.write_frame(moq_net::Timestamp::ZERO, b"state-0".as_ref())
		.expect("write old state");
	old_group.finish().expect("finish old state");

	let pub_session = tokio::time::timeout(TIMEOUT, client().with_publisher(&pub_origin).connect(url.clone()))
		.await
		.expect("publisher connect timeout")
		.expect("publisher connect failed");

	// The early subscriber requests from sequence zero so group 0 is definitely
	// present in the relay cache before the late-join phase begins.
	let early_origin = Origin::random().produce();
	let mut early_announcements = early_origin.consume().announced();
	let early_session = tokio::time::timeout(TIMEOUT, client().with_subscriber(early_origin).connect(url.clone()))
		.await
		.expect("early subscriber connect timeout")
		.expect("early subscriber connect failed");
	let moq_net::announce::Update {
		path: early_path,
		broadcast: early_broadcast,
	} = tokio::time::timeout(TIMEOUT, early_announcements.next())
		.await
		.expect("early announcement timeout")
		.expect("early origin closed");
	assert_eq!(early_path.as_str(), "late-join");
	let early_broadcast = early_broadcast.expect("expected early announce");
	let early_track = early_broadcast.track("state").expect("early state track");
	let mut early_sub = early_track
		.subscribe(Some(
			moq_net::track::Subscription::default()
				.with_group_start(0)
				.with_latency_max(Duration::from_secs(30)),
		))
		.await
		.expect("early state subscribe");
	let mut early_group = tokio::time::timeout(TIMEOUT, early_sub.recv_group())
		.await
		.expect("early group timeout")
		.expect("early group receive failed")
		.expect("early track closed");
	assert_eq!(early_group.sequence, 0);
	let early_frame = tokio::time::timeout(TIMEOUT, early_group.read_frame())
		.await
		.expect("early frame timeout")
		.expect("early frame receive failed")
		.expect("early group closed");
	assert_eq!(&early_frame.payload[..], b"state-0");

	// A second group establishes a live edge while retaining group 0.
	tokio::time::sleep(Duration::from_millis(100)).await;
	let mut current_group = track.append_group().expect("append current group");
	current_group
		.write_frame(moq_net::Timestamp::from_millis(100).unwrap(), b"state-1".as_ref())
		.expect("write current state");
	current_group.finish().expect("finish current state");
	let mut early_current = tokio::time::timeout(TIMEOUT, early_sub.recv_group())
		.await
		.expect("early current group timeout")
		.expect("early current group receive failed")
		.expect("early track closed at current state");
	assert_eq!(early_current.sequence, 1);
	let early_current_frame = tokio::time::timeout(TIMEOUT, early_current.read_frame())
		.await
		.expect("early current frame timeout")
		.expect("early current frame receive failed")
		.expect("early current group closed");
	assert_eq!(&early_current_frame.payload[..], b"state-1");
	drop(early_sub);
	drop(early_session);

	// Late subscribers join the current state concurrently while the source is alive.
	// Each session is a distinct peer; keeping the session inside the future ensures
	// all peer subscriptions overlap instead of becoming a sequential loopback.
	let phase_started = std::time::Instant::now();
	let peer_observations = futures::future::join_all((0..peer_count).map(|peer_id| {
		let phase_started = phase_started.clone();
		let url = url.clone();
		async move {
			let late_origin = Origin::random().produce();
			let mut late_announcements = late_origin.consume().announced();
			let connect_started = std::time::Instant::now();
			let late_session = tokio::time::timeout(TIMEOUT, client().with_subscriber(late_origin).connect(url))
				.await
				.expect("late subscriber connect timeout")
				.expect("late subscriber connect failed");
			let connect_ready_ms = phase_started.elapsed().as_secs_f64() * 1000.0;
			let moq_net::announce::Update {
				path: late_path,
				broadcast: late_broadcast,
			} = tokio::time::timeout(TIMEOUT, late_announcements.next())
				.await
				.expect("late announcement timeout")
				.expect("late origin closed");
			assert_eq!(late_path.as_str(), "late-join");
			let late_broadcast = late_broadcast.expect("expected late announce");
			let late_track = late_broadcast.track("state").expect("late state track");
			let subscribe_started = std::time::Instant::now();
			let subscribe_start_ms = phase_started.elapsed().as_secs_f64() * 1000.0;
			let mut late_sub = late_track.clone().subscribe(None).await.expect("late state subscribe");
			let mut late_group = tokio::time::timeout(TIMEOUT, late_sub.recv_group())
				.await
				.expect("late current group timeout")
				.expect("late current group receive failed")
				.expect("late track closed");
			let first_group_ms = phase_started.elapsed().as_secs_f64() * 1000.0;
			let late_ttfs_ms = subscribe_started.elapsed().as_secs_f64() * 1000.0;
			assert_eq!(late_group.sequence, 1);
			let late_frame = tokio::time::timeout(TIMEOUT, late_group.read_frame())
				.await
				.expect("late current frame timeout")
				.expect("late current frame receive failed")
				.expect("late current group closed");
			let first_frame_ms = phase_started.elapsed().as_secs_f64() * 1000.0;
			assert_eq!(&late_frame.payload[..], b"state-1");

			// Fetch the older group while the upstream source is still connected.
			// This is retained-history evidence, not a source-loss persistence claim.
			let fetch_started = std::time::Instant::now();
			let fetch_start_ms = phase_started.elapsed().as_secs_f64() * 1000.0;
			let mut historical = tokio::time::timeout(TIMEOUT, late_track.fetch_group(0, None))
				.await
				.expect("historical fetch timeout")
				.expect("historical fetch failed while source is alive");
			let historical_frame = tokio::time::timeout(TIMEOUT, historical.read_frame())
				.await
				.expect("historical frame timeout")
				.expect("historical frame receive failed")
				.expect("historical group closed");
			let history_frame_ms = phase_started.elapsed().as_secs_f64() * 1000.0;
			let history_ttfs_ms = fetch_started.elapsed().as_secs_f64() * 1000.0;
			assert_eq!(historical.sequence, 0);
			assert_eq!(&historical_frame.payload[..], b"state-0");

			let observation = serde_json::json!({
				"peer_id": peer_id,
				"events_ms": {
					"connect_start": (connect_started.duration_since(phase_started)).as_secs_f64() * 1000.0,
					"connect_ready": connect_ready_ms,
					"subscribe_start": subscribe_start_ms,
					"first_group": first_group_ms,
					"first_frame": first_frame_ms,
					"fetch_start": fetch_start_ms,
					"history_frame": history_frame_ms
				},
				"current_state": {
					"sequence": late_group.sequence,
					"payload": String::from_utf8(late_frame.payload.to_vec()).expect("current payload is UTF-8")
				},
				"history": {
					"sequence": historical.sequence,
					"payload": String::from_utf8(historical_frame.payload.to_vec()).expect("history payload is UTF-8")
				},
				"timing_ms": {
					"current_state_ttfs": late_ttfs_ms,
					"current_state_first_frame": first_frame_ms - subscribe_start_ms,
					"history_fetch": history_ttfs_ms
				},
				"source_alive": true
			});
			drop(late_sub);
			drop(late_session);
			observation
		}
	}))
	.await;

	let fetch_telemetry =
		stats
			.report()
			.traffic
			.into_iter()
			.fold(moq_net::stats::Traffic::default(), |mut totals, entry| {
				totals.add(entry.publisher);
				totals.add(entry.subscriber);
				totals
			});
	let current_state_ttfs_ms = peer_observations
		.iter()
		.map(|peer| {
			peer["timing_ms"]["current_state_ttfs"]
				.as_f64()
				.expect("peer TTFS is numeric")
		})
		.sum::<f64>()
		/ peer_count as f64;
	let history_fetch_ms = peer_observations
		.iter()
		.map(|peer| {
			peer["timing_ms"]["history_fetch"]
				.as_f64()
				.expect("peer FETCH time is numeric")
		})
		.sum::<f64>()
		/ peer_count as f64;

	if let Some(path) = std::env::var_os("MOQ_PHASE5_ARTIFACT") {
		let artifact = serde_json::json!({
			"schema_version": 1,
			"test": "relay_websocket_late_join_replays_retained_history_while_source_alive",
			"peer_count": peer_count,
			"cache": {
				"capacity": "64MiB",
				"duration_ms": 5000,
				"cluster_linger_ms": 5000,
				"track_latency_max_ms": 30000
			},
			"current_state": {"sequence": 1, "payload": "state-1"},
			"history": {"sequence": 0, "payload": "state-0"},
			"timing_ms": {
				"current_state_ttfs": current_state_ttfs_ms,
				"history_fetch": history_fetch_ms
			},
			"fetch_telemetry": {
				"fetches": fetch_telemetry.fetches,
				"fetches_local": fetch_telemetry.fetches_local,
				"fetches_dynamic": fetch_telemetry.fetches_dynamic,
				"fetches_miss": fetch_telemetry.fetches_miss
			},
			"peers": peer_observations,
			"source_alive": true
		});
		let encoded = serde_json::to_vec_pretty(&artifact).expect("serialize Phase 5 artifact");
		std::fs::write(path, encoded).expect("write Phase 5 artifact");
	}

	eprintln!(
		"phase5_late_join peer_count={peer_count} cache_duration=5s current_sequence=1 history_sequence=0 source_alive=true fetches={} local={} dynamic={} miss={}",
		fetch_telemetry.fetches,
		fetch_telemetry.fetches_local,
		fetch_telemetry.fetches_dynamic,
		fetch_telemetry.fetches_miss
	);

	drop(pub_session);
	drop(track);
	drop(broadcast);
	web_handle.abort();
}

/// Connect a publisher and a subscriber to a real relay over `ws://`, push
/// one frame end-to-end, and assert both sides see the newest moq-lite ALPN.
/// Regression for the `serve_ws` downgrade to Lite02.
#[tokio::test]
async fn relay_websocket_round_trip_uses_newest_version() {
	let (port, web_handle) = spawn_relay().await;
	let url: url::Url = format!("ws://127.0.0.1:{port}/smoke").parse().expect("parse url");
	let expected_version = newest_lite_version();

	// ── publisher ───────────────────────────────────────────────────
	let pub_origin = Origin::random().produce();
	let mut broadcast = pub_origin
		.create_broadcast("test", moq_net::broadcast::Route::new().with_announce(true))
		.expect("create broadcast");
	let mut track = broadcast.create_track("video", None).expect("create track");
	let mut group = track.append_group().expect("append group");
	group
		.write_frame(moq_net::Timestamp::ZERO, b"hello".as_ref())
		.expect("write frame");
	group.finish().expect("finish group");

	let pub_session = tokio::time::timeout(TIMEOUT, client().with_publisher(&pub_origin).connect(url.clone()))
		.await
		.expect("publisher connect timeout")
		.expect("publisher connect failed");
	assert_eq!(
		pub_session.version(),
		expected_version,
		"publisher negotiated stale version"
	);

	// ── subscriber ──────────────────────────────────────────────────
	let sub_origin = Origin::random().produce();
	let mut announcements = sub_origin.consume().announced();

	let sub_session = tokio::time::timeout(TIMEOUT, client().with_subscriber(sub_origin).connect(url))
		.await
		.expect("subscriber connect timeout")
		.expect("subscriber connect failed");
	assert_eq!(
		sub_session.version(),
		expected_version,
		"subscriber negotiated stale version"
	);

	// ── data path ───────────────────────────────────────────────────
	let moq_net::announce::Update { path, broadcast: bc } = tokio::time::timeout(TIMEOUT, announcements.next())
		.await
		.expect("announcement timeout")
		.expect("origin closed");
	// Auth root for `/smoke` is "smoke"; the broadcast "test" announces underneath.
	assert_eq!(path.as_str(), "test");
	let bc = bc.expect("expected announce, got unannounce");

	let mut track_sub = bc.track("video").unwrap().subscribe(None).await.expect("consume_track");
	let mut group_sub = tokio::time::timeout(TIMEOUT, track_sub.recv_group())
		.await
		.expect("recv_group timeout")
		.expect("recv_group failed")
		.expect("track closed prematurely");
	let frame = tokio::time::timeout(TIMEOUT, group_sub.read_frame())
		.await
		.expect("read_frame timeout")
		.expect("read_frame failed")
		.expect("group closed prematurely");
	assert_eq!(&frame.payload[..], b"hello");

	// Hold the producers until after data is read; dropping them earlier
	// would close the publishing side of the broadcast.
	drop(track);
	drop(broadcast);

	drop(pub_session);
	drop(sub_session);
	web_handle.abort();
}

#[tokio::test]
async fn relay_web_serves_merged_routes() {
	tokio::time::pause();
	let port = free_tcp_port();
	let web = build_web(port, false).await;
	let app = web
		.routes()
		.route("/embedded", axum::routing::get(|| async { "embedded\n" }));

	let (server_result_tx, mut server_result_rx) = tokio::sync::oneshot::channel();
	let handle = tokio::spawn(async move {
		let _ = server_result_tx.send(web.serve(app).await);
	});

	wait_for_http(port, &mut server_result_rx).await;

	let body = reqwest::get(format!("http://127.0.0.1:{port}/embedded"))
		.await
		.expect("fetch embedded route")
		.text()
		.await
		.expect("read embedded response");
	assert_eq!(body, "embedded\n");

	handle.abort();
}

/// A client that dials a bare `host:port` with no path must still get a
/// WebSocket upgrade at the root, not the landing page. The empty path is the
/// root auth scope (same as the internal listener). Regression for the
/// `/{*path}`-only route, which left bare-URL clients (e.g.
/// `moqsink url="https://host:4443"`) with a silently dead WS fallback.
#[tokio::test]
async fn relay_websocket_rejects_reserved_cluster_markers() {
	let (port, web_handle) = spawn_relay().await;
	let response = reqwest::Client::new()
		.get(format!(
			"http://127.0.0.1:{port}/anon?control_only=true&namespace=observation%2Fdiagonal"
		))
		.header("Connection", "Upgrade")
		.header("Upgrade", "websocket")
		.header("Sec-WebSocket-Version", "13")
		.header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
		.send()
		.await
		.expect("reserved-marker WebSocket request failed");
	assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
	web_handle.abort();
}

#[tokio::test]
async fn relay_websocket_root_path_upgrades() {
	let (port, web_handle) = spawn_relay().await;
	// No path: the URL is just host:port, so the WS handshake targets "/".
	let url: url::Url = format!("ws://127.0.0.1:{port}").parse().expect("parse url");

	// ── publisher ───────────────────────────────────────────────────
	let pub_origin = Origin::random().produce();
	let mut broadcast = pub_origin
		.create_broadcast("test", moq_net::broadcast::Route::new().with_announce(true))
		.expect("create broadcast");
	let mut track = broadcast.create_track("video", None).expect("create track");
	let mut group = track.append_group().expect("append group");
	group
		.write_frame(moq_net::Timestamp::ZERO, b"hello".as_ref())
		.expect("write frame");
	group.finish().expect("finish group");

	let pub_session = tokio::time::timeout(
		TIMEOUT,
		client().with_publisher(pub_origin.consume()).connect(url.clone()),
	)
	.await
	.expect("publisher connect timeout")
	.expect("publisher connect failed (root-path WS upgrade)");

	// ── subscriber ──────────────────────────────────────────────────
	let sub_origin = Origin::random().produce();
	let mut announcements = sub_origin.consume().announced();
	let sub_session = tokio::time::timeout(TIMEOUT, client().with_subscriber(sub_origin).connect(url))
		.await
		.expect("subscriber connect timeout")
		.expect("subscriber connect failed (root-path WS upgrade)");

	// ── data path ───────────────────────────────────────────────────
	// The root auth scope is the empty path, so the broadcast announces at its
	// own name with no prefix.
	let moq_net::announce::Update { path, broadcast: bc } = tokio::time::timeout(TIMEOUT, announcements.next())
		.await
		.expect("announcement timeout")
		.expect("origin closed");
	assert_eq!(path.as_str(), "test");
	let bc = bc.expect("expected announce, got unannounce");

	let mut track_sub = bc.track("video").unwrap().subscribe(None).await.expect("consume_track");
	let mut group_sub = tokio::time::timeout(TIMEOUT, track_sub.recv_group())
		.await
		.expect("recv_group timeout")
		.expect("recv_group failed")
		.expect("track closed prematurely");
	let frame = tokio::time::timeout(TIMEOUT, group_sub.read_frame())
		.await
		.expect("read_frame timeout")
		.expect("read_frame failed")
		.expect("group closed prematurely");
	assert_eq!(&frame.payload[..], b"hello");

	drop(track);
	drop(broadcast);
	drop(pub_session);
	drop(sub_session);
	web_handle.abort();
}

/// Two publish-only clients (each `with_publisher`, no `with_subscriber`) coexist on one relay;
/// a single subscriber sees broadcasts forwarded from both. Verifies that multiple
/// publish-only connections don't interfere with each other or get torn down.
#[tokio::test]
async fn two_publish_only_clients_coexist() {
	let (port, web_handle) = spawn_relay().await;
	let url: url::Url = format!("ws://127.0.0.1:{port}/smoke").parse().expect("parse url");

	// ── two publish-only publishers, each serving a distinct broadcast ──
	let pub_a = Origin::random().produce();
	let mut broadcast_a = pub_a
		.create_broadcast("alpha", moq_net::broadcast::Route::new().with_announce(true))
		.expect("create broadcast a");
	let mut track_a = broadcast_a.create_track("video", None).expect("create track a");
	track_a
		.append_group()
		.expect("append group a")
		.write_frame(moq_net::Timestamp::ZERO, b"a".as_ref())
		.expect("write frame a");

	let pub_b = Origin::random().produce();
	let mut broadcast_b = pub_b
		.create_broadcast("beta", moq_net::broadcast::Route::new().with_announce(true))
		.expect("create broadcast b");
	let mut track_b = broadcast_b.create_track("video", None).expect("create track b");
	track_b
		.append_group()
		.expect("append group b")
		.write_frame(moq_net::Timestamp::ZERO, b"b".as_ref())
		.expect("write frame b");

	let sess_a = tokio::time::timeout(TIMEOUT, client().with_publisher(pub_a.consume()).connect(url.clone()))
		.await
		.expect("publisher a connect timeout")
		.expect("publisher a connect failed");
	let sess_b = tokio::time::timeout(TIMEOUT, client().with_publisher(pub_b.consume()).connect(url.clone()))
		.await
		.expect("publisher b connect timeout")
		.expect("publisher b connect failed");

	// ── one subscriber should see broadcasts from both publish-only clients ──
	let sub_origin = Origin::random().produce();
	let mut announcements = sub_origin.consume().announced();
	let sub_session = tokio::time::timeout(TIMEOUT, client().with_subscriber(sub_origin).connect(url))
		.await
		.expect("subscriber connect timeout")
		.expect("subscriber connect failed");

	let mut seen = std::collections::HashSet::new();
	while seen.len() < 2 {
		let moq_net::announce::Update { path, broadcast: bc } = tokio::time::timeout(TIMEOUT, announcements.next())
			.await
			.expect("announcement timeout")
			.expect("origin closed");
		if bc.is_some() {
			seen.insert(path.as_str().to_owned());
		}
	}
	assert!(
		seen.contains("alpha") && seen.contains("beta"),
		"expected both publish-only broadcasts, saw {seen:?}"
	);

	// Hold producers until announcements are observed.
	drop(track_a);
	drop(broadcast_a);
	drop(track_b);
	drop(broadcast_b);

	drop(sess_a);
	drop(sess_b);
	drop(sub_session);
	web_handle.abort();
}

/// Run the relay's accept loop over the given server config, the same path
/// `main.rs` uses. Authenticates through the shared [`Auth`], here with fully
/// public access (`--auth-public ""`) so no-JWT clients get the root.
///
/// Returns the QUIC socket the server bound, when it has one, so a caller that
/// asked for an ephemeral port can dial it.
async fn spawn_accept_relay(
	config: moq_native::ServerConfig,
	auth_config: AuthConfig,
) -> (Option<std::net::SocketAddr>, tokio::task::JoinHandle<()>) {
	let (addr, handle, _) = spawn_accept_relay_with_options(config, auth_config, ClusterConfig::default(), false).await;
	(addr, handle)
}

async fn spawn_accept_relay_with_options(
	config: moq_native::ServerConfig,
	auth_config: AuthConfig,
	cluster_config: ClusterConfig,
	enable_url_less_protocol_bytes: bool,
) -> (
	Option<std::net::SocketAddr>,
	tokio::task::JoinHandle<()>,
	Arc<Mutex<Vec<moq_net::ProtocolBytes>>>,
) {
	spawn_accept_relay_with_measurement(
		config,
		auth_config,
		cluster_config,
		enable_url_less_protocol_bytes,
		false,
	)
	.await
}

async fn spawn_accept_relay_with_measurement(
	config: moq_native::ServerConfig,
	auth_config: AuthConfig,
	cluster_config: ClusterConfig,
	enable_url_less_protocol_bytes: bool,
	enable_data_measurement: bool,
) -> (
	Option<std::net::SocketAddr>,
	tokio::task::JoinHandle<()>,
	Arc<Mutex<Vec<moq_net::ProtocolBytes>>>,
) {
	let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

	let mut server = config.init().expect("server init");
	let protocol_bytes = Arc::new(Mutex::new(Vec::new()));
	if enable_url_less_protocol_bytes || enable_data_measurement {
		let protocol_bytes_capture = protocol_bytes.clone();
		server = server.with_protocol_bytes_factory(move |url| {
			if url.is_some() && !enable_data_measurement {
				return None;
			}
			let bytes = moq_net::ProtocolBytes::enabled();
			protocol_bytes_capture
				.lock()
				.expect("protocol bytes mutex")
				.push(bytes.clone());
			Some(bytes)
		});
	}
	let addr = server.local_addr().ok();

	let auth = auth_config
		.init(&moq_native::tls::Client::default())
		.await
		.expect("auth init");

	let cluster = Cluster::new(cluster_config)
		.expect("cluster init")
		.with_protocol_bytes_data(enable_data_measurement);

	let handle = tokio::spawn(async move {
		let mut id = 0;
		while let Some(request) = server.accept().await {
			let conn = Connection {
				id,
				request,
				cluster: cluster.clone(),
				auth: auth.clone(),
			};
			id += 1;
			tokio::spawn(async move {
				let _ = conn.run().await;
			});
		}
	});

	(addr, handle, protocol_bytes)
}

/// Stand up the relay listening only on a plain-TCP qmux `--server-bind` on a
/// free loopback port, with fully public auth (no-JWT => whole root). Returns
/// the port and an abort handle.
async fn spawn_internal_relay() -> (u16, tokio::task::JoinHandle<()>) {
	// Pick a free TCP port, then drop the probe so the listener can bind it.
	let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe");
	let port = probe.local_addr().expect("local addr").port();
	drop(probe);

	// Stream-only: a TCP listener with no `--server-bind`, so no QUIC.
	let mut config = moq_native::ServerConfig::default();
	config.tcp.bind = Some(format!("127.0.0.1:{port}").parse().expect("parse addr"));

	// Public Simple([""]) lets any no-JWT stream client through at the root.
	#[allow(deprecated)]
	let public = PublicConfig::Simple(vec![String::new()]);
	let mut auth_config = AuthConfig::default();
	auth_config.public = Some(public);

	let (_, handle) = spawn_accept_relay(config, auth_config).await;

	let deadline = std::time::Instant::now() + Duration::from_secs(5);
	loop {
		if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
			break;
		}
		if std::time::Instant::now() >= deadline {
			panic!("internal listener never became ready on port {port}");
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}

	(port, handle)
}

async fn spawn_control_only_tcp_relay(
	enable_url_less_protocol_bytes: bool,
) -> (
	u16,
	tokio::task::JoinHandle<()>,
	Arc<Mutex<Vec<moq_net::ProtocolBytes>>>,
) {
	let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe");
	let port = probe.local_addr().expect("local addr").port();
	drop(probe);

	let mut config = moq_native::ServerConfig::default();
	config.tcp.bind = Some(format!("127.0.0.1:{port}").parse().expect("parse addr"));
	#[allow(deprecated)]
	let public = PublicConfig::Simple(vec![String::new()]);
	let mut auth_config = AuthConfig::default();
	auth_config.public = Some(public);
	let mut cluster_config = ClusterConfig::default();
	cluster_config.namespace_filter = Some(true);
	cluster_config.namespace = Some("room/run/area/west".to_string());
	let (addr, handle, protocol_bytes) =
		spawn_accept_relay_with_options(config, auth_config, cluster_config, enable_url_less_protocol_bytes).await;
	let deadline = std::time::Instant::now() + Duration::from_secs(5);
	loop {
		if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
			break;
		}
		if std::time::Instant::now() >= deadline {
			panic!("control-only TCP listener never became ready on {addr:?}:{port}");
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}
	(port, handle, protocol_bytes)
}

/// Connect a publisher and subscriber to a stream `--server-bind` over `tcp://`
/// (plain TCP, no TLS, no JWT) and confirm a frame round-trips. Exercises the
/// qmux-over-TCP transport and no-JWT resolution through public auth.
#[tokio::test]
async fn internal_tcp_round_trip() {
	let (port, handle) = spawn_internal_relay().await;
	// The explicit root target is carried in the SETUP; a pathless URL is rejected.
	let url: url::Url = format!("tcp://127.0.0.1:{port}/").parse().expect("parse url");
	let expected_version = newest_lite_version();

	// ── publisher ───────────────────────────────────────────────────
	let pub_origin = Origin::random().produce();
	let mut broadcast = pub_origin
		.create_broadcast("test", moq_net::broadcast::Route::new().with_announce(true))
		.expect("create broadcast");
	let mut track = broadcast.create_track("video", None).expect("create track");
	let mut group = track.append_group().expect("append group");
	group
		.write_frame(moq_net::Timestamp::ZERO, b"hello".as_ref())
		.expect("write frame");
	group.finish().expect("finish group");

	let pub_session = tokio::time::timeout(
		TIMEOUT,
		client().with_publisher(pub_origin.consume()).connect(url.clone()),
	)
	.await
	.expect("publisher connect timeout")
	.expect("publisher connect failed");
	assert_eq!(
		pub_session.version(),
		expected_version,
		"publisher should negotiate the newest moq-lite version in-band over TCP"
	);

	// ── subscriber ──────────────────────────────────────────────────
	let sub_origin = Origin::random().produce();
	let mut announcements = sub_origin.consume().announced();
	let sub_session = tokio::time::timeout(TIMEOUT, client().with_subscriber(sub_origin).connect(url))
		.await
		.expect("subscriber connect timeout")
		.expect("subscriber connect failed");

	// ── data path ───────────────────────────────────────────────────
	// The internal listener grants the empty root, so the broadcast announces
	// at its own name with no path prefix.
	let moq_net::announce::Update { path, broadcast: bc } = tokio::time::timeout(TIMEOUT, announcements.next())
		.await
		.expect("announcement timeout")
		.expect("origin closed");
	assert_eq!(path.as_str(), "test");
	let bc = bc.expect("expected announce, got unannounce");

	let mut track_sub = bc.track("video").unwrap().subscribe(None).await.expect("consume_track");
	let mut group_sub = tokio::time::timeout(TIMEOUT, track_sub.recv_group())
		.await
		.expect("recv_group timeout")
		.expect("recv_group failed")
		.expect("track closed prematurely");
	let frame = tokio::time::timeout(TIMEOUT, group_sub.read_frame())
		.await
		.expect("read_frame timeout")
		.expect("read_frame failed")
		.expect("group closed prematurely");
	assert_eq!(&frame.payload[..], b"hello");

	drop(track);
	drop(broadcast);
	drop(pub_session);
	drop(sub_session);
	handle.abort();
}

/// Stand up a stream `--server-bind` on a Unix socket and return the socket path
/// plus an abort handle.
#[cfg(unix)]
async fn spawn_internal_unix_relay() -> (std::path::PathBuf, tokio::task::JoinHandle<()>) {
	// Keep the path short: macOS caps AF_UNIX paths around 104 bytes, and the
	// system temp dir is long. /tmp is fine on macOS and Linux. A per-call counter
	// keeps concurrent tests in the same process off each other's socket.
	static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
	let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let path = std::path::PathBuf::from(format!("/tmp/moq-internal-{}-{seq}.sock", std::process::id()));

	// Stream-only: a Unix listener with no `--server-bind`, so no QUIC.
	let mut config = moq_native::ServerConfig::default();
	config.unix.bind = Some(path.clone());

	// Public Simple([""]) lets any no-JWT stream client through at the root.
	#[allow(deprecated)]
	let public = PublicConfig::Simple(vec![String::new()]);
	let mut auth_config = AuthConfig::default();
	auth_config.public = Some(public);

	let (_, handle) = spawn_accept_relay(config, auth_config).await;

	// Wait for the socket file to appear.
	let deadline = std::time::Instant::now() + Duration::from_secs(5);
	loop {
		if tokio::net::UnixStream::connect(&path).await.is_ok() {
			break;
		}
		if std::time::Instant::now() >= deadline {
			panic!("internal Unix listener never became ready at {}", path.display());
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}

	(path, handle)
}

#[cfg(unix)]
async fn spawn_control_only_unix_relay(
	enable_url_less_protocol_bytes: bool,
) -> (
	std::path::PathBuf,
	tokio::task::JoinHandle<()>,
	Arc<Mutex<Vec<moq_net::ProtocolBytes>>>,
) {
	static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
	let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let path = std::path::PathBuf::from(format!("/tmp/moq-control-only-{}-{seq}.sock", std::process::id()));
	let mut config = moq_native::ServerConfig::default();
	config.unix.bind = Some(path.clone());
	#[allow(deprecated)]
	let public = PublicConfig::Simple(vec![String::new()]);
	let mut auth_config = AuthConfig::default();
	auth_config.public = Some(public);
	let mut cluster_config = ClusterConfig::default();
	cluster_config.namespace_filter = Some(true);
	cluster_config.namespace = Some("room/run/area/west".to_string());
	let (addr, handle, protocol_bytes) =
		spawn_accept_relay_with_options(config, auth_config, cluster_config, enable_url_less_protocol_bytes).await;
	let deadline = std::time::Instant::now() + Duration::from_secs(5);
	loop {
		if tokio::net::UnixStream::connect(&path).await.is_ok() {
			break;
		}
		if std::time::Instant::now() >= deadline {
			panic!(
				"control-only Unix listener never became ready at {addr:?}:{}",
				path.display()
			);
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}
	(path, handle, protocol_bytes)
}

/// Connect over `unix://` (qmux on a Unix socket) and confirm a frame
/// round-trips. Also asserts both sides land on the newest moq-lite version,
/// which proves the in-band ALPN negotiation populated the protocol.
#[cfg(unix)]
#[tokio::test]
async fn internal_unix_round_trip() {
	let (path, handle) = spawn_internal_unix_relay().await;
	// `unix://` + an absolute path yields the triple-slash form the client expects.
	let url: url::Url = format!("unix://{}?path=/", path.display()).parse().expect("parse url");
	let expected_version = newest_lite_version();

	// ── publisher ───────────────────────────────────────────────────
	let pub_origin = Origin::random().produce();
	let mut broadcast = pub_origin
		.create_broadcast("test", moq_net::broadcast::Route::new().with_announce(true))
		.expect("create broadcast");
	let mut track = broadcast.create_track("video", None).expect("create track");
	let mut group = track.append_group().expect("append group");
	group
		.write_frame(moq_net::Timestamp::ZERO, b"hello".as_ref())
		.expect("write frame");
	group.finish().expect("finish group");

	let pub_session = tokio::time::timeout(
		TIMEOUT,
		client().with_publisher(pub_origin.consume()).connect(url.clone()),
	)
	.await
	.expect("publisher connect timeout")
	.expect("publisher connect failed");
	assert_eq!(
		pub_session.version(),
		expected_version,
		"publisher should negotiate the newest moq-lite version in-band over the Unix socket"
	);

	// ── subscriber ──────────────────────────────────────────────────
	let sub_origin = Origin::random().produce();
	let mut announcements = sub_origin.consume().announced();
	let sub_session = tokio::time::timeout(TIMEOUT, client().with_subscriber(sub_origin).connect(url))
		.await
		.expect("subscriber connect timeout")
		.expect("subscriber connect failed");

	// ── data path ───────────────────────────────────────────────────
	let moq_net::announce::Update {
		path: announced_path,
		broadcast: bc,
	} = tokio::time::timeout(TIMEOUT, announcements.next())
		.await
		.expect("announcement timeout")
		.expect("origin closed");
	assert_eq!(announced_path.as_str(), "test");
	let bc = bc.expect("expected announce, got unannounce");

	let mut track_sub = bc.track("video").unwrap().subscribe(None).await.expect("consume_track");
	let mut group_sub = tokio::time::timeout(TIMEOUT, track_sub.recv_group())
		.await
		.expect("recv_group timeout")
		.expect("recv_group failed")
		.expect("track closed prematurely");
	let frame = tokio::time::timeout(TIMEOUT, group_sub.read_frame())
		.await
		.expect("read_frame timeout")
		.expect("read_frame failed")
		.expect("group closed prematurely");
	assert_eq!(&frame.payload[..], b"hello");

	drop(track);
	drop(broadcast);
	drop(pub_session);
	drop(sub_session);
	handle.abort();
}

/// Every version whose SETUP carries a request path the server reads: moq-lite-05
/// (Setup Stream) and moq-transport 14-18 (the `Path` SETUP parameter, in-band on
/// the bidi stream for 14-16 and the uni Setup Stream for 17-18). lite-06-wip shares
/// lite-05's SETUP path handling but is opt-in only, so it isn't exercised here.
fn path_versions() -> Vec<moq_net::Version> {
	[
		"moq-lite-05",
		"moq-transport-14",
		"moq-transport-15",
		"moq-transport-16",
		"moq-transport-17",
		"moq-transport-18",
	]
	.iter()
	.map(|alpn| alpn.parse().expect("parse version alpn"))
	.collect()
}

/// Publisher and subscriber (both pinned to `version`) that announce/observe
/// `broadcast` over the internal listener at `pub_url` / `sub_url`. Returns the
/// path the subscriber sees the publisher's broadcast announced at, proving
/// whether the request path reached the server (it scopes the publisher's grant
/// to that root).
async fn path_round_trip(version: moq_net::Version, pub_url: url::Url, sub_url: url::Url, broadcast: &str) -> String {
	let pub_origin = Origin::random().produce();
	let mut bc = pub_origin
		.create_broadcast(broadcast, moq_net::broadcast::Route::new().with_announce(true))
		.expect("create broadcast");
	let mut track = bc.create_track("video", None).expect("create track");
	let mut group = track.append_group().expect("append group");
	group
		.write_frame(moq_net::Timestamp::ZERO, b"hello".as_ref())
		.expect("write frame");
	group.finish().expect("finish group");

	let pub_client = client_version(Some(version)).with_publisher(pub_origin.consume());
	let pub_session = tokio::time::timeout(TIMEOUT, pub_client.connect(pub_url))
		.await
		.expect("publisher connect timeout")
		.expect("publisher connect failed");

	let sub_origin = Origin::random().produce();
	let mut announcements = sub_origin.consume().announced();
	let sub_client = client_version(Some(version)).with_subscriber(sub_origin);
	let sub_session = tokio::time::timeout(TIMEOUT, sub_client.connect(sub_url))
		.await
		.expect("subscriber connect timeout")
		.expect("subscriber connect failed");

	let moq_net::announce::Update { path, .. } = tokio::time::timeout(TIMEOUT, announcements.next())
		.await
		.expect("announcement timeout")
		.expect("origin closed");

	drop(track);
	drop(bc);
	drop(pub_session);
	drop(sub_session);
	path.as_str().to_string()
}

/// A `tcp://host:port/<path>` client advertises `<path>` in the SETUP; the relay
/// scopes its grant to that root, so the publisher's broadcast lands prefixed.
/// Proves the request path can be specified and reaches the server over plain TCP,
/// across every version whose SETUP carries a path.
#[tokio::test]
async fn internal_tcp_path_reaches_server() {
	let (port, handle) = spawn_internal_relay().await;

	// Publisher addresses `/room`; subscriber addresses the bare root.
	let pub_url: url::Url = format!("tcp://127.0.0.1:{port}/room").parse().expect("parse url");
	let sub_url: url::Url = format!("tcp://127.0.0.1:{port}/").parse().expect("parse url");

	for version in path_versions() {
		let announced = path_round_trip(version, pub_url.clone(), sub_url.clone(), "test").await;
		assert_eq!(
			announced, "room/test",
			"the SETUP path should scope the publisher's grant ({version})"
		);
	}

	handle.abort();
}

/// `unix://<socket>` carries no resource path in its URL (that's the socket), so
/// the request path rides in the `?path=` query. Same assertion as TCP, across
/// every version whose SETUP carries a path.
#[cfg(unix)]
#[tokio::test]
async fn internal_unix_path_reaches_server() {
	let (path, handle) = spawn_internal_unix_relay().await;

	let pub_url: url::Url = format!("unix://{}?path=room", path.display())
		.parse()
		.expect("parse url");
	let sub_url: url::Url = format!("unix://{}?path=/", path.display()).parse().expect("parse url");

	for version in path_versions() {
		let announced = path_round_trip(version, pub_url.clone(), sub_url.clone(), "test").await;
		assert_eq!(
			announced, "room/test",
			"the SETUP path should scope the publisher's grant ({version})"
		);
	}

	handle.abort();
}

/// Stand up the relay listening only on a QUIC `--server-bind` on an ephemeral
/// loopback port, with fully public auth (no-JWT => whole root). Returns the bound
/// address and an abort handle.
async fn spawn_quic_relay() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
	let mut config = moq_native::ServerConfig::default();
	config.bind = Some("127.0.0.1:0".to_string());
	config.tls.generate = vec!["localhost".into()];

	#[allow(deprecated)]
	let public = PublicConfig::Simple(vec![String::new()]);
	let mut auth_config = AuthConfig::default();
	auth_config.public = Some(public);

	let (addr, handle) = spawn_accept_relay(config, auth_config).await;
	(addr.expect("relay bound no QUIC socket"), handle)
}

async fn spawn_control_only_quic_relay(
	enable_url_less_protocol_bytes: bool,
) -> (
	std::net::SocketAddr,
	tokio::task::JoinHandle<()>,
	Arc<Mutex<Vec<moq_net::ProtocolBytes>>>,
) {
	let mut config = moq_native::ServerConfig::default();
	config.bind = Some("127.0.0.1:0".to_string());
	config.tls.generate = vec!["localhost".into()];

	#[allow(deprecated)]
	let public = PublicConfig::Simple(vec![String::new()]);
	let mut auth_config = AuthConfig::default();
	auth_config.public = Some(public);

	let mut cluster_config = ClusterConfig::default();
	cluster_config.namespace_filter = Some(true);
	cluster_config.namespace = Some("room/run/area/west".to_string());
	let (addr, handle, protocol_bytes) =
		spawn_accept_relay_with_options(config, auth_config, cluster_config, enable_url_less_protocol_bytes).await;
	(addr.expect("relay bound no QUIC socket"), handle, protocol_bytes)
}

async fn spawn_data_measurement_quic_relay() -> (
	std::net::SocketAddr,
	tokio::task::JoinHandle<()>,
	Arc<Mutex<Vec<moq_net::ProtocolBytes>>>,
) {
	let mut config = moq_native::ServerConfig::default();
	config.bind = Some("127.0.0.1:0".to_string());
	config.tls.generate = vec!["localhost".into()];

	#[allow(deprecated)]
	let public = PublicConfig::Simple(vec![String::new()]);
	let mut auth_config = AuthConfig::default();
	auth_config.public = Some(public);
	let (addr, handle, protocol_bytes) =
		spawn_accept_relay_with_measurement(config, auth_config, ClusterConfig::default(), false, true).await;
	(addr.expect("relay bound no QUIC socket"), handle, protocol_bytes)
}

/// Raw QUIC has no request URI either, so `moqt://host:port/<path>` only reaches the
/// relay if the client puts it in the SETUP. Same assertion as TCP: the relay scopes
/// the publisher's grant to that root, across every version whose SETUP carries a path.
#[tokio::test]
async fn raw_quic_path_reaches_server() {
	let (addr, handle) = spawn_quic_relay().await;

	// Dialing an IP literal sends no SNI, so the SETUP is the only thing the server
	// has to go on.
	let pub_url: url::Url = format!("moqt://{addr}/room").parse().expect("parse url");
	let sub_url: url::Url = format!("moqt://{addr}/").parse().expect("parse url");

	for version in path_versions() {
		let announced = path_round_trip(version, pub_url.clone(), sub_url.clone(), "test").await;
		assert_eq!(
			announced, "room/test",
			"the SETUP path should scope the publisher's grant ({version})"
		);
	}

	handle.abort();
}

/// A URL-less request without an explicit SETUP path must not fall back to root
/// auth, including the legacy Lite01-04 versions that cannot carry a path at all.
#[tokio::test]
async fn url_less_pathless_request_is_rejected() {
	let (addr, handle) = spawn_quic_relay().await;
	for version_name in [
		"moq-lite-01",
		"moq-lite-02",
		"moq-lite-03",
		"moq-lite-04",
		"moq-transport-14",
		"moq-transport-19",
	] {
		let version: moq_net::Version = version_name.parse().expect("parse version");
		let url: url::Url = format!("moqt://{addr}").parse().expect("parse URL");
		if let Ok(session) = tokio::time::timeout(TIMEOUT, client_version(Some(version)).connect(url))
			.await
			.expect("pathless raw QUIC connect timeout")
		{
			tokio::time::timeout(TIMEOUT, session.closed())
				.await
				.expect("pathless raw QUIC session was not rejected");
		}
	}

	handle.abort();
}

/// Plain TCP qmux must reject the same pathless legacy and modern requests instead
/// of authorizing the public root implicitly.
#[tokio::test]
async fn tcp_qmux_pathless_request_is_rejected() {
	let (port, handle) = spawn_internal_relay().await;
	for version_name in ["moq-lite-01", "moq-lite-04", "moq-transport-19"] {
		let version: moq_net::Version = version_name.parse().expect("parse version");
		let url: url::Url = format!("tcp://127.0.0.1:{port}").parse().expect("parse URL");
		if let Ok(session) = tokio::time::timeout(TIMEOUT, client_version(Some(version)).connect(url))
			.await
			.expect("pathless TCP qmux connect timeout")
		{
			tokio::time::timeout(TIMEOUT, session.closed())
				.await
				.expect("pathless TCP qmux session was not rejected");
		}
	}

	handle.abort();
}

#[cfg(unix)]
/// Unix qmux must reject a missing `?path=` target for the same reason as TCP qmux.
#[tokio::test]
async fn unix_qmux_pathless_request_is_rejected() {
	let (path, handle) = spawn_internal_unix_relay().await;
	for version_name in ["moq-lite-01", "moq-lite-04", "moq-transport-19"] {
		let version: moq_net::Version = version_name.parse().expect("parse version");
		let url: url::Url = format!("unix://{}", path.display()).parse().expect("parse URL");
		if let Ok(session) = tokio::time::timeout(TIMEOUT, client_version(Some(version)).connect(url))
			.await
			.expect("pathless Unix qmux connect timeout")
		{
			tokio::time::timeout(TIMEOUT, session.closed())
				.await
				.expect("pathless Unix qmux session was not rejected");
		}
	}

	handle.abort();
}

/// A raw QUIC SETUP carries the control-only markers that URL-bearing transports
/// normally keep in their request URL. The relay must reject the marker when no
/// URL-less meter is installed, and must classify the SETUP as control bytes when
/// the explicit opt-in factory is present.
#[tokio::test]
async fn raw_quic_control_only_path_requires_and_records_protocol_bytes() {
	let version: moq_net::Version = "moq-transport-19".parse().expect("parse Draft19");
	let url = |addr: std::net::SocketAddr| -> url::Url {
		format!("moqt://{addr}/anon?control_only=true&namespace=room%2Frun%2Farea%2Feast")
			.parse()
			.expect("parse raw control-only URL")
	};

	let (addr, handle, protocol_bytes) = spawn_control_only_quic_relay(true).await;
	let session = tokio::time::timeout(TIMEOUT, client_version(Some(version)).connect(url(addr)))
		.await
		.expect("raw control-only connect timeout")
		.expect("raw control-only connect failed with meter enabled");
	assert_eq!(session.version(), version);
	let snapshot = tokio::time::timeout(TIMEOUT, async {
		loop {
			let snapshots: Vec<_> = protocol_bytes
				.lock()
				.expect("protocol bytes mutex")
				.iter()
				.map(moq_net::ProtocolBytes::snapshot)
				.collect();
			if let Some(snapshot) = snapshots
				.iter()
				.find(|snapshot| snapshot.control_bytes_sent > 0 && snapshot.control_bytes_received > 0)
				.copied()
			{
				break snapshot;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await
	.expect("raw QUIC factory did not meter the SETUP");
	assert_eq!(snapshot.data_bytes_sent, 0);
	assert_eq!(snapshot.data_bytes_received, 0);
	assert_eq!(snapshot.unclassified_bytes_sent, 0);
	assert_eq!(snapshot.unclassified_bytes_received, 0);
	drop(session);
	handle.abort();

	let (addr, handle, protocol_bytes) = spawn_control_only_quic_relay(false).await;
	let session = tokio::time::timeout(TIMEOUT, client_version(Some(version)).connect(url(addr)))
		.await
		.expect("raw control-only rejection handshake timeout")
		.expect("raw control-only transport setup failed unexpectedly");
	let closed = tokio::time::timeout(TIMEOUT, session.closed()).await;
	assert!(
		closed.is_ok(),
		"raw control-only connection bypassed missing meter enforcement"
	);
	assert!(
		protocol_bytes.lock().expect("protocol bytes mutex").is_empty(),
		"meter unexpectedly installed without opt-in"
	);
	drop(session);
	handle.abort();
}

/// A normal URL-less data session opts into the dedicated measurement path with
/// `protocol_bytes=true`. Its known IETF group stream must be counted as data,
/// while the separate control-only tests above remain negative controls.
#[tokio::test]
async fn raw_quic_data_session_opt_in_records_positive_data_bytes() {
	let version: moq_net::Version = "moq-transport-19".parse().expect("parse Draft19");
	let (addr, handle, protocol_bytes) = spawn_control_only_quic_relay(true).await;
	let url: url::Url = format!("moqt://{addr}/anon?protocol_bytes=true")
		.parse()
		.expect("parse raw data URL");

	let pub_origin = Origin::random().produce();
	let mut broadcast = pub_origin
		.create_broadcast("metered", moq_net::broadcast::Route::new().with_announce(true))
		.expect("create broadcast");
	let mut track = broadcast.create_track("video", None).expect("create track");
	let mut group = track.append_group().expect("append group");
	group
		.write_frame(moq_net::Timestamp::ZERO, b"metered-data".as_ref())
		.expect("write frame");
	group.finish().expect("finish group");

	let pub_session = tokio::time::timeout(
		TIMEOUT,
		client_version(Some(version))
			.with_publisher(pub_origin.consume())
			.connect(url.clone()),
	)
	.await
	.expect("data publisher connect timeout")
	.expect("data publisher connect failed");

	let sub_origin = Origin::random().produce();
	let mut announcements = sub_origin.consume().announced();
	let sub_session = tokio::time::timeout(
		TIMEOUT,
		client_version(Some(version)).with_subscriber(sub_origin).connect(url),
	)
	.await
	.expect("data subscriber connect timeout")
	.expect("data subscriber connect failed");

	let moq_net::announce::Update { broadcast: bc, .. } = tokio::time::timeout(TIMEOUT, announcements.next())
		.await
		.expect("data announcement timeout")
		.expect("data origin closed");
	let bc = bc.expect("expected metered broadcast announcement");
	let mut track_sub = bc
		.track("video")
		.expect("find metered track")
		.subscribe(None)
		.await
		.expect("subscribe");
	let mut group_sub = tokio::time::timeout(TIMEOUT, track_sub.recv_group())
		.await
		.expect("data group timeout")
		.expect("data group failed")
		.expect("data track closed");
	let frame = tokio::time::timeout(TIMEOUT, group_sub.read_frame())
		.await
		.expect("data frame timeout")
		.expect("data frame failed")
		.expect("data group closed");
	assert_eq!(&frame.payload[..], b"metered-data");

	let snapshot = tokio::time::timeout(TIMEOUT, async {
		loop {
			let snapshots: Vec<_> = protocol_bytes
				.lock()
				.expect("protocol bytes mutex")
				.iter()
				.map(moq_net::ProtocolBytes::snapshot)
				.collect();
			if let Some(snapshot) = snapshots
				.iter()
				.find(|snapshot| snapshot.data_bytes_sent > 0 || snapshot.data_bytes_received > 0)
				.copied()
			{
				break snapshot;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await
	.expect("data session did not produce positive protocol data bytes");
	assert!(snapshot.data_bytes_sent > 0 || snapshot.data_bytes_received > 0);
	assert_eq!(snapshot.unclassified_bytes_sent, 0);
	assert_eq!(snapshot.unclassified_bytes_received, 0);

	drop(track);
	drop(broadcast);
	drop(pub_session);
	drop(sub_session);
	handle.abort();
}

/// The node-level `protocol_bytes` flag also enables URL-less regular sessions
/// without requiring a per-URL marker. This is the explicit data-session gate
/// used by the measurement-only relay configuration.
#[tokio::test]
async fn raw_quic_data_node_flag_attaches_meter_without_marker() {
	let version: moq_net::Version = "moq-transport-19".parse().expect("parse Draft19");
	let (addr, handle, protocol_bytes) = spawn_data_measurement_quic_relay().await;
	let url: url::Url = format!("moqt://{addr}/anon").parse().expect("parse raw data URL");
	let session = tokio::time::timeout(TIMEOUT, client_version(Some(version)).connect(url))
		.await
		.expect("data flag connect timeout")
		.expect("data flag connect failed");

	let snapshot = tokio::time::timeout(TIMEOUT, async {
		loop {
			let snapshots: Vec<_> = protocol_bytes
				.lock()
				.expect("protocol bytes mutex")
				.iter()
				.map(moq_net::ProtocolBytes::snapshot)
				.collect();
			if let Some(snapshot) = snapshots
				.iter()
				.find(|snapshot| snapshot.control_bytes_sent > 0 && snapshot.control_bytes_received > 0)
				.copied()
			{
				break snapshot;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await
	.expect("node data flag did not attach a meter");
	assert_eq!(snapshot.data_bytes_sent, 0);
	assert_eq!(snapshot.data_bytes_received, 0);
	assert_eq!(snapshot.unclassified_bytes_sent, 0);
	assert_eq!(snapshot.unclassified_bytes_received, 0);

	drop(session);
	handle.abort();
}

/// Plain-TCP qmux has no request URI, so it must use the same SETUP marker and
/// URL-less meter policy as raw QUIC. This keeps the two URL-less bindings from
/// drifting apart.
#[tokio::test]
async fn tcp_qmux_control_only_path_requires_and_records_protocol_bytes() {
	let version: moq_net::Version = "moq-transport-19".parse().expect("parse Draft19");
	let url = |port: u16| -> url::Url {
		format!("tcp://127.0.0.1:{port}/anon?control_only=true&namespace=room%2Frun%2Farea%2Feast")
			.parse()
			.expect("parse TCP control-only URL")
	};

	let (port, handle, protocol_bytes) = spawn_control_only_tcp_relay(true).await;
	let session = tokio::time::timeout(TIMEOUT, client_version(Some(version)).connect(url(port)))
		.await
		.expect("TCP control-only connect timeout")
		.expect("TCP control-only connect failed with meter enabled");
	assert_eq!(session.version(), version);
	let snapshot = tokio::time::timeout(TIMEOUT, async {
		loop {
			let snapshots: Vec<_> = protocol_bytes
				.lock()
				.expect("protocol bytes mutex")
				.iter()
				.map(moq_net::ProtocolBytes::snapshot)
				.collect();
			if let Some(snapshot) = snapshots
				.iter()
				.find(|snapshot| snapshot.control_bytes_sent > 0 && snapshot.control_bytes_received > 0)
				.copied()
			{
				break snapshot;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await
	.expect("TCP qmux factory did not meter the SETUP");
	assert_eq!(snapshot.data_bytes_sent, 0);
	assert_eq!(snapshot.data_bytes_received, 0);
	assert_eq!(snapshot.unclassified_bytes_sent, 0);
	assert_eq!(snapshot.unclassified_bytes_received, 0);
	drop(session);
	handle.abort();

	let (port, handle, protocol_bytes) = spawn_control_only_tcp_relay(false).await;
	let session = tokio::time::timeout(TIMEOUT, client_version(Some(version)).connect(url(port)))
		.await
		.expect("TCP control-only rejection handshake timeout")
		.expect("TCP control-only transport setup failed unexpectedly");
	let closed = tokio::time::timeout(TIMEOUT, session.closed()).await;
	assert!(
		closed.is_ok(),
		"TCP control-only connection bypassed missing meter enforcement"
	);
	assert!(
		protocol_bytes.lock().expect("protocol bytes mutex").is_empty(),
		"TCP meter unexpectedly installed without opt-in"
	);
	drop(session);
	handle.abort();
}

#[cfg(unix)]
/// Unix qmux is the second URL-less stream binding and must preserve the same
/// control-only marker and opt-in metering contract as TCP qmux.
#[tokio::test]
async fn unix_qmux_control_only_path_requires_and_records_protocol_bytes() {
	let version: moq_net::Version = "moq-transport-19".parse().expect("parse Draft19");
	let url = |path: &std::path::Path| -> url::Url {
		let mut url: url::Url = format!("unix://{}", path.display()).parse().expect("parse Unix URL");
		url.query_pairs_mut()
			.append_pair("path", "/anon?control_only=true&namespace=room%2Frun%2Farea%2Feast");
		url
	};

	let (path, handle, protocol_bytes) = spawn_control_only_unix_relay(true).await;
	let session = tokio::time::timeout(TIMEOUT, client_version(Some(version)).connect(url(&path)))
		.await
		.expect("Unix control-only connect timeout")
		.expect("Unix control-only connect failed with meter enabled");
	assert_eq!(session.version(), version);
	let snapshot = tokio::time::timeout(TIMEOUT, async {
		loop {
			let snapshots: Vec<_> = protocol_bytes
				.lock()
				.expect("protocol bytes mutex")
				.iter()
				.map(moq_net::ProtocolBytes::snapshot)
				.collect();
			if let Some(snapshot) = snapshots
				.iter()
				.find(|snapshot| snapshot.control_bytes_sent > 0 && snapshot.control_bytes_received > 0)
				.copied()
			{
				break snapshot;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await
	.expect("Unix qmux factory did not meter the SETUP");
	assert_eq!(snapshot.data_bytes_sent, 0);
	assert_eq!(snapshot.data_bytes_received, 0);
	assert_eq!(snapshot.unclassified_bytes_sent, 0);
	assert_eq!(snapshot.unclassified_bytes_received, 0);
	drop(session);
	handle.abort();

	let (path, handle, protocol_bytes) = spawn_control_only_unix_relay(false).await;
	let session = tokio::time::timeout(TIMEOUT, client_version(Some(version)).connect(url(&path)))
		.await
		.expect("Unix control-only rejection handshake timeout")
		.expect("Unix control-only transport setup failed unexpectedly");
	let closed = tokio::time::timeout(TIMEOUT, session.closed()).await;
	assert!(
		closed.is_ok(),
		"Unix control-only connection bypassed missing meter enforcement"
	);
	assert!(
		protocol_bytes.lock().expect("protocol bytes mutex").is_empty(),
		"Unix meter unexpectedly installed without opt-in"
	);
	drop(session);
	handle.abort();
}

/// `/health` is a liveness probe that always returns `200 ok`.
#[tokio::test]
async fn health_endpoint_reports_ok() {
	let (port, web_handle) = spawn_relay().await;

	let resp = tokio::time::timeout(TIMEOUT, reqwest::get(format!("http://127.0.0.1:{port}/health")))
		.await
		.expect("health request timeout")
		.expect("health request failed");

	assert_eq!(resp.status(), reqwest::StatusCode::OK);
	let body = resp.text().await.expect("health body");
	assert_eq!(body, "ok\n");

	web_handle.abort();
}

/// Stand up a stream relay whose public access grants **subscribe only**, returning
/// the TCP port and an abort handle. A no-JWT client gets the root for subscribing but
/// no publish scope, so a publisher's role is rejected.
async fn spawn_subscribe_only_relay() -> (u16, tokio::task::JoinHandle<()>) {
	let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe");
	let port = probe.local_addr().expect("local addr").port();
	drop(probe);

	let mut config = moq_native::ServerConfig::default();
	config.tcp.bind = Some(format!("127.0.0.1:{port}").parse().expect("parse addr"));

	// Subscribe-only public access: the root is granted for subscribing, never publishing.
	#[allow(deprecated)]
	let public_subscribe = PublicConfig::Simple(vec![String::new()]);
	let mut auth_config = AuthConfig::default();
	auth_config.public_subscribe = Some(public_subscribe);

	let (_, handle) = spawn_accept_relay(config, auth_config).await;

	let deadline = std::time::Instant::now() + Duration::from_secs(5);
	loop {
		if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
			break;
		}
		if std::time::Instant::now() >= deadline {
			panic!("subscribe-only listener never became ready on port {port}");
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}

	(port, handle)
}

/// A publisher whose token grants only subscribe scope is rejected during the
/// handshake instead of being accepted and silently carrying no media. The client
/// advertises `Role::Publisher` in its SETUP (derived from `with_publisher`), and the
/// relay closes the session because the token has no publish scope. This is the
/// regression guard for moq.pro#338: before the role hint, this connection was
/// accepted and the publisher streamed into a dropped session forever.
#[tokio::test]
async fn subscribe_only_public_rejects_publisher_role() {
	let (port, handle) = spawn_subscribe_only_relay().await;
	let url: url::Url = format!("tcp://127.0.0.1:{port}/").parse().expect("parse url");

	let pub_origin = Origin::random().produce();

	// The lite-05 client resolves `connect()` optimistically, so it may return Ok
	// before the relay's verdict lands. Either the connect fails outright, or the
	// session it returns closes shortly after with the relay's rejection. A correctly
	// scoped subscriber, by contrast, would stay open indefinitely.
	match tokio::time::timeout(TIMEOUT, client().with_publisher(pub_origin.consume()).connect(url)).await {
		Ok(Ok(session)) => {
			tokio::time::timeout(TIMEOUT, session.closed())
				.await
				.expect("relay should close a publisher whose token lacks publish scope, not leave it open");
		}
		Ok(Err(_)) => {} // rejected synchronously at connect; also acceptable.
		Err(_) => panic!("publisher connect neither resolved nor was rejected within the timeout"),
	}

	handle.abort();
}

/// The mirror of the reject test: a subscriber (`Role::Subscriber`, from `with_subscriber`)
/// is accepted by the same subscribe-only relay, and its session stays open. This proves
/// the role gate rejects only the mismatched direction, not the whole listener.
#[tokio::test]
async fn subscribe_only_public_accepts_subscriber_role() {
	let (port, handle) = spawn_subscribe_only_relay().await;
	let url: url::Url = format!("tcp://127.0.0.1:{port}/").parse().expect("parse url");

	let sub_origin = Origin::random().produce();
	let session = tokio::time::timeout(TIMEOUT, client().with_subscriber(sub_origin).connect(url))
		.await
		.expect("subscriber connect timeout")
		.expect("subscriber connect failed");

	// The session must NOT be closed by the relay: a short wait should time out.
	let still_open = tokio::time::timeout(Duration::from_millis(500), session.closed()).await;
	assert!(
		still_open.is_err(),
		"subscribe-only relay should keep a subscriber session open"
	);

	handle.abort();
}

/// The mirror of [`spawn_subscribe_only_relay`]: public access grants **publish only**,
/// so a no-JWT client gets the root for publishing but no subscribe scope.
async fn spawn_publish_only_relay() -> (u16, tokio::task::JoinHandle<()>) {
	let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe");
	let port = probe.local_addr().expect("local addr").port();
	drop(probe);

	let mut config = moq_native::ServerConfig::default();
	config.tcp.bind = Some(format!("127.0.0.1:{port}").parse().expect("parse addr"));

	// Publish-only public access: the root is granted for publishing, never subscribing.
	#[allow(deprecated)]
	let public_publish = PublicConfig::Simple(vec![String::new()]);
	let mut auth_config = AuthConfig::default();
	auth_config.public_publish = Some(public_publish);

	let (_, handle) = spawn_accept_relay(config, auth_config).await;

	let deadline = std::time::Instant::now() + Duration::from_secs(5);
	loop {
		if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
			break;
		}
		if std::time::Instant::now() >= deadline {
			panic!("publish-only listener never became ready on port {port}");
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}

	(port, handle)
}

/// The mirror of the publisher-reject test, covering the other branch of the role gate:
/// a subscriber (`Role::Subscriber`, from `with_subscriber`) whose token grants only
/// publish scope is rejected during the handshake instead of left silently empty.
#[tokio::test]
async fn publish_only_public_rejects_subscriber_role() {
	let (port, handle) = spawn_publish_only_relay().await;
	let url: url::Url = format!("tcp://127.0.0.1:{port}/").parse().expect("parse url");

	let sub_origin = Origin::random().produce();

	// Like the publisher case, `connect()` may resolve optimistically; either it fails
	// outright, or the session the relay hands back closes shortly after.
	match tokio::time::timeout(TIMEOUT, client().with_subscriber(sub_origin).connect(url)).await {
		Ok(Ok(session)) => {
			tokio::time::timeout(TIMEOUT, session.closed())
				.await
				.expect("relay should close a subscriber whose token lacks subscribe scope, not leave it open");
		}
		Ok(Err(_)) => {} // rejected synchronously at connect; also acceptable.
		Err(_) => panic!("subscriber connect neither resolved nor was rejected within the timeout"),
	}

	handle.abort();
}
