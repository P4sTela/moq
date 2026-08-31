use std::{
	fs,
	path::{Path, PathBuf},
	time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use moq_native::moq_net::{self, Origin};
use serde_json::{Value, json};
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(15);
const CACHE_DURATION: Duration = Duration::from_secs(5);
const TRACK_LATENCY_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Role {
	Publisher,
	Late,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Stratum {
	SourceAlive,
	RetentionBoundary,
	SourceLoss,
}

impl Stratum {
	fn as_str(self) -> &'static str {
		match self {
			Self::SourceAlive => "source-alive",
			Self::RetentionBoundary => "retention-boundary",
			Self::SourceLoss => "source-loss",
		}
	}

	fn current(self) -> (u64, &'static [u8]) {
		match self {
			Self::SourceAlive | Self::SourceLoss => (1, b"state-1"),
			Self::RetentionBoundary => (2, b"state-2"),
		}
	}
}

#[derive(Clone, Debug, Parser)]
#[command(name = "moq-phase5-client", about = "Canonical Phase 5 mesh workload client")]
struct Args {
	#[arg(long, env = "MP_PHASE5_ROLE", value_enum)]
	role: Role,

	#[arg(long, env = "MP_PHASE5_STRATUM", value_enum, default_value = "source-alive")]
	stratum: Stratum,

	#[arg(long, env = "MP_PHASE5_RELAY_URL")]
	relay_url: String,

	#[arg(long, env = "MP_PHASE5_SCHEDULE_DIR")]
	schedule_dir: PathBuf,

	#[arg(long, env = "MP_PHASE5_ARTIFACT")]
	artifact: PathBuf,

	#[arg(long, env = "MP_PHASE5_PEER_ID", default_value_t = 0)]
	peer_id: usize,

	#[arg(long, env = "MP_PHASE5_PEER_COUNT", default_value_t = 1)]
	peer_count: usize,

	#[arg(long, env = "MP_PHASE5_WAIT_TIMEOUT_MS", default_value_t = 30_000)]
	wait_timeout_ms: u64,
}

fn monotonic_ns() -> u64 {
	#[cfg(unix)]
	{
		let mut value = libc::timespec { tv_sec: 0, tv_nsec: 0 };
		// SAFETY: `value` is a valid writable timespec and CLOCK_MONOTONIC is
		// supported on the Unix targets used by the mesh containers.
		let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) };
		assert_eq!(result, 0, "clock_gettime(CLOCK_MONOTONIC) failed");
		return (value.tv_sec as u64)
			.saturating_mul(1_000_000_000)
			.saturating_add(value.tv_nsec as u64);
	}

	#[cfg(not(unix))]
	{
		Instant::now().elapsed().as_nanos() as u64
	}
}

fn client() -> moq_native::Client {
	let mut config = moq_native::ClientConfig::default();
	config.tls.disable_verify = Some(true);
	config.websocket.delay = None;
	config.bind = "0.0.0.0:0".parse().expect("parse wildcard client bind");
	config.init().expect("initialize canonical client")
}

fn url(args: &Args) -> Result<Url> {
	args.relay_url
		.parse()
		.with_context(|| format!("invalid relay URL: {}", args.relay_url))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
	let parent = path.parent().context("artifact has no parent directory")?;
	fs::create_dir_all(parent).with_context(|| format!("create artifact directory {}", parent.display()))?;
	let file_name = path.file_name().context("artifact has no file name")?.to_string_lossy();
	let temporary = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));
	let encoded = serde_json::to_vec_pretty(value).context("serialize Phase 5 artifact")?;
	fs::write(&temporary, encoded).with_context(|| format!("write temporary artifact {}", temporary.display()))?;
	fs::rename(&temporary, path).with_context(|| format!("publish artifact {}", path.display()))?;
	Ok(())
}

fn marker_path(schedule_dir: &Path, name: &str) -> PathBuf {
	schedule_dir.join(name)
}

fn peer_marker(schedule_dir: &Path, prefix: &str, peer_id: usize) -> PathBuf {
	schedule_dir.join(format!("{prefix}-{peer_id}.json"))
}

async fn wait_for_file(path: &Path, timeout: Duration) -> Result<()> {
	let deadline = Instant::now() + timeout;
	loop {
		if path.is_file() {
			return Ok(());
		}
		if Instant::now() >= deadline {
			bail!("timed out waiting for marker {}", path.display());
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}
}

async fn wait_for_peer_markers(schedule_dir: &Path, prefix: &str, peer_count: usize, timeout: Duration) -> Result<()> {
	let deadline = Instant::now() + timeout;
	loop {
		if (0..peer_count).all(|peer_id| peer_marker(schedule_dir, prefix, peer_id).is_file()) {
			return Ok(());
		}
		if Instant::now() >= deadline {
			bail!(
				"timed out waiting for {prefix}-0..{prefix}-{}",
				peer_count.saturating_sub(1)
			);
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}
}

async fn receive_group(
	subscription: &mut moq_net::track::Subscriber,
	timeout_message: &'static str,
) -> Result<moq_net::group::Consumer> {
	tokio::time::timeout(TIMEOUT, subscription.recv_group())
		.await
		.context(timeout_message)?
		.context("receive group failed")?
		.context("track closed")
}

async fn run_early(args: Args) -> Result<()> {
	let started_ns = monotonic_ns();
	let (expected_sequence, expected_payload) = args.stratum.current();
	let origin = Origin::random().produce();
	let mut announcements = origin.consume().announced();
	let session = tokio::time::timeout(TIMEOUT, client().with_subscriber(origin).connect(url(&args)?))
		.await
		.context("early materializer connect timeout")?
		.context("early materializer connect failed")?;
	let ready_ns = monotonic_ns();
	let moq_net::announce::Update { path, broadcast } = tokio::time::timeout(TIMEOUT, announcements.next())
		.await
		.context("early announcement timeout")?
		.context("early origin closed")?;
	ensure!(path.as_str() == "late-join", "unexpected early broadcast path: {path}");
	let track = broadcast
		.context("missing early broadcast")?
		.track("state")
		.context("missing early state track")?;
	let mut subscription = track
		.subscribe(Some(
			moq_net::track::Subscription::default()
				.with_group_start(0)
				.with_latency_max(TRACK_LATENCY_MAX),
		))
		.await
		.context("early state subscribe")?;

	let mut group = receive_group(&mut subscription, "early group timeout").await?;
	ensure!(
		group.sequence == 0,
		"early first sequence is {}, expected 0",
		group.sequence
	);
	let frame = tokio::time::timeout(TIMEOUT, group.read_frame())
		.await
		.context("early first frame timeout")?
		.context("early first frame receive failed")?
		.context("early first group closed")?;
	ensure!(&frame.payload[..] == b"state-0", "early first payload mismatch");

	loop {
		group = receive_group(&mut subscription, "early current group timeout").await?;
		let sequence = group.sequence;
		let frame = tokio::time::timeout(TIMEOUT, group.read_frame())
			.await
			.context("early current frame timeout")?
			.context("early current frame receive failed")?
			.context("early current group closed")?;
		if sequence == expected_sequence {
			ensure!(&frame.payload[..] == expected_payload, "early current payload mismatch");
			break;
		}
		ensure!(
			sequence < expected_sequence,
			"early sequence {sequence} passed expected {expected_sequence}"
		);
	}
	let finished_ns = monotonic_ns();
	write_json(
		&args.artifact,
		&json!({
			"schema_version": 1,
			"role": "early-materializer",
			"stratum": args.stratum.as_str(),
			"peer_count": args.peer_count,
			"current_state": {
				"sequence": expected_sequence,
				"payload": String::from_utf8(expected_payload.to_vec()).expect("early payload is UTF-8")
			},
			"events_ns": {
				"connect_start": started_ns,
				"connect_ready": ready_ns,
				"current_state": finished_ns
			},
			"source_alive": true
		}),
	)?;
	drop(session);
	Ok(())
}

async fn run_late(args: Args) -> Result<()> {
	ensure!(
		args.peer_id < args.peer_count,
		"peer id {} is outside peer count {}",
		args.peer_id,
		args.peer_count
	);
	let timeout = Duration::from_millis(args.wait_timeout_ms);
	let (expected_sequence, expected_payload) = args.stratum.current();
	let ready = marker_path(&args.schedule_dir, "phase5-publisher-ready.json");
	wait_for_file(&ready, timeout).await?;

	let connect_started_ns = monotonic_ns();
	let origin = Origin::random().produce();
	let mut announcements = origin.consume().announced();
	let session = tokio::time::timeout(TIMEOUT, client().with_subscriber(origin).connect(url(&args)?))
		.await
		.context("late subscriber connect timeout")?
		.context("late subscriber connect failed")?;
	let connect_ready_ns = monotonic_ns();
	let moq_net::announce::Update { path, broadcast } = tokio::time::timeout(TIMEOUT, announcements.next())
		.await
		.context("late announcement timeout")?
		.context("late origin closed")?;
	ensure!(path.as_str() == "late-join", "unexpected late broadcast path: {path}");
	let track = broadcast
		.context("missing late broadcast")?
		.track("state")
		.context("missing late state track")?;
	let subscribe_started_ns = monotonic_ns();
	let mut subscription = track.clone().subscribe(None).await.context("late state subscribe")?;
	let mut current = receive_group(&mut subscription, "late current group timeout").await?;
	let first_group_ns = monotonic_ns();
	ensure!(
		current.sequence == expected_sequence,
		"late sequence is {}, expected {expected_sequence}",
		current.sequence
	);
	let frame = tokio::time::timeout(TIMEOUT, current.read_frame())
		.await
		.context("late current frame timeout")?
		.context("late current frame receive failed")?
		.context("late current group closed")?;
	let first_frame_ns = monotonic_ns();
	ensure!(&frame.payload[..] == expected_payload, "late current payload mismatch");

	let current_ack = peer_marker(&args.schedule_dir, "phase5-current-ack", args.peer_id);
	write_json(
		&current_ack,
		&json!({
			"peer_id": args.peer_id,
			"monotonic_ns": first_frame_ns,
			"sequence": expected_sequence
		}),
	)?;

	if matches!(args.stratum, Stratum::SourceLoss) {
		wait_for_file(
			&marker_path(&args.schedule_dir, "phase5-publisher-disconnected.json"),
			timeout,
		)
		.await?;
	}

	let fetch_started_ns = monotonic_ns();
	let history = if matches!(args.stratum, Stratum::SourceLoss) {
		let fetched = tokio::time::timeout(TIMEOUT, track.fetch_group(0, None)).await;
		let (restored, sequence, payload, error) = match fetched {
			Ok(Ok(mut historical)) => match tokio::time::timeout(TIMEOUT, historical.read_frame()).await {
				Ok(Ok(Some(frame))) => (
					true,
					Some(historical.sequence),
					Some(String::from_utf8(frame.payload.to_vec()).context("history payload is UTF-8")?),
					None,
				),
				Ok(Ok(None)) => (
					false,
					Some(historical.sequence),
					None,
					Some("history group closed".to_string()),
				),
				Ok(Err(err)) => (false, Some(historical.sequence), None, Some(format!("frame: {err:?}"))),
				Err(_) => (
					false,
					Some(historical.sequence),
					None,
					Some("frame timeout".to_string()),
				),
			},
			Ok(Err(err)) => (false, None, None, Some(format!("fetch: {err:?}"))),
			Err(_) => (false, None, None, Some("fetch timeout".to_string())),
		};
		json!({
			"restored": restored,
			"sequence": sequence,
			"payload": payload,
			"error": error
		})
	} else {
		let mut historical = tokio::time::timeout(TIMEOUT, track.fetch_group(0, None))
			.await
			.context("history fetch timeout")?
			.context("history fetch failed")?;
		let historical_sequence = historical.sequence;
		let historical_frame = tokio::time::timeout(TIMEOUT, historical.read_frame())
			.await
			.context("history frame timeout")?
			.context("history frame receive failed")?
			.context("history group closed")?;
		ensure!(
			historical_sequence == 0,
			"history sequence is {historical_sequence}, expected 0"
		);
		ensure!(&historical_frame.payload[..] == b"state-0", "history payload mismatch");
		json!({
			"sequence": historical_sequence,
			"payload": String::from_utf8(historical_frame.payload.to_vec()).context("history payload is UTF-8")?
		})
	};
	let history_frame_ns = monotonic_ns();
	let source_alive = !matches!(args.stratum, Stratum::SourceLoss);
	let artifact = json!({
		"schema_version": 1,
		"role": "late",
		"stratum": args.stratum.as_str(),
		"peer_id": args.peer_id,
		"peer_count": args.peer_count,
		"events_ns": {
			"connect_start": connect_started_ns,
			"connect_ready": connect_ready_ns,
			"subscribe_start": subscribe_started_ns,
			"first_group": first_group_ns,
			"first_frame": first_frame_ns,
			"fetch_start": fetch_started_ns,
			"history_frame": history_frame_ns
		},
		"current_state": {
			"sequence": expected_sequence,
			"payload": String::from_utf8(expected_payload.to_vec()).expect("current payload is UTF-8")
		},
		"history": history,
		"source_alive": source_alive
	});
	write_json(&args.artifact, &artifact)?;
	write_json(
		&peer_marker(&args.schedule_dir, "phase5-done", args.peer_id),
		&json!({
			"peer_id": args.peer_id,
			"monotonic_ns": history_frame_ns,
			"history_restored": artifact["history"].get("restored").and_then(Value::as_bool).unwrap_or(true)
		}),
	)?;
	drop(subscription);
	drop(session);
	Ok(())
}

async fn run_publisher(args: Args) -> Result<()> {
	ensure!(args.peer_count >= 1, "peer count must be positive");
	let timeout = Duration::from_millis(args.wait_timeout_ms);
	let (expected_sequence, expected_payload) = args.stratum.current();
	let started_ns = monotonic_ns();
	let origin = Origin::random().produce();
	let mut broadcast = origin
		.create_broadcast("late-join", moq_net::broadcast::Route::new().with_announce(true))
		.context("create Phase 5 broadcast")?;
	let track_info = moq_net::track::Info::default().with_latency_max(TRACK_LATENCY_MAX);
	let mut track = broadcast
		.create_track("state", Some(track_info))
		.context("create Phase 5 state track")?;
	let mut old_group = track.append_group().context("append state-0")?;
	old_group
		.write_frame(moq_net::Timestamp::ZERO, b"state-0".as_ref())
		.context("write state-0")?;
	old_group.finish().context("finish state-0")?;

	let pub_session = tokio::time::timeout(TIMEOUT, client().with_publisher(&origin).connect(url(&args)?))
		.await
		.context("publisher connect timeout")?
		.context("publisher connect failed")?;
	let connected_ns = monotonic_ns();
	let early_args = Args {
		role: Role::Late,
		artifact: args.schedule_dir.join("phase5-early-materializer.json"),
		..args.clone()
	};
	let early_task = tokio::spawn(run_early(early_args));

	tokio::time::sleep(Duration::from_millis(100)).await;
	let mut first_current = track.append_group().context("append state-1")?;
	first_current
		.write_frame(
			moq_net::Timestamp::from_millis(100).context("timestamp state-1")?,
			b"state-1".as_ref(),
		)
		.context("write state-1")?;
	first_current.finish().context("finish state-1")?;
	let state1_ns = monotonic_ns();

	if matches!(args.stratum, Stratum::RetentionBoundary) {
		tokio::time::sleep(CACHE_DURATION + Duration::from_millis(100)).await;
		let mut boundary = track.append_group().context("append state-2")?;
		boundary
			.write_frame(
				moq_net::Timestamp::from_millis(5200).context("timestamp state-2")?,
				b"state-2".as_ref(),
			)
			.context("write state-2")?;
		boundary.finish().context("finish state-2")?;
	}
	early_task.await.context("early materializer task join")??;
	let current_ns = monotonic_ns();
	write_json(
		&marker_path(&args.schedule_dir, "phase5-publisher-ready.json"),
		&json!({
			"stratum": args.stratum.as_str(),
			"sequence": expected_sequence,
			"payload": String::from_utf8(expected_payload.to_vec()).expect("publisher payload is UTF-8"),
			"monotonic_ns": current_ns
		}),
	)?;

	let late_args = Args {
		role: Role::Late,
		peer_id: 0,
		artifact: args.schedule_dir.join("phase5-peer-0.json"),
		..args.clone()
	};
	let late_task = tokio::spawn(run_late(late_args));
	let disconnected_ns = if matches!(args.stratum, Stratum::SourceLoss) {
		wait_for_peer_markers(&args.schedule_dir, "phase5-current-ack", args.peer_count, timeout).await?;
		drop(pub_session);
		let disconnected_ns = monotonic_ns();
		write_json(
			&marker_path(&args.schedule_dir, "phase5-publisher-disconnected.json"),
			&json!({"monotonic_ns": disconnected_ns, "source_alive": false}),
		)?;
		Some(disconnected_ns)
	} else {
		wait_for_peer_markers(&args.schedule_dir, "phase5-done", args.peer_count, timeout).await?;
		None
	};
	late_task.await.context("late peer 0 task join")??;
	let finished_ns = monotonic_ns();
	write_json(
		&args.artifact,
		&json!({
			"schema_version": 1,
			"role": "publisher",
			"stratum": args.stratum.as_str(),
			"peer_count": args.peer_count,
			"events_ns": {
				"publisher_start": started_ns,
				"publisher_connected": connected_ns,
				"state_1_written": state1_ns,
				"current_state_ready": current_ns,
				"publisher_disconnected": disconnected_ns,
				"finished": finished_ns
			},
			"current_state": {
				"sequence": expected_sequence,
				"payload": String::from_utf8(expected_payload.to_vec()).expect("publisher payload is UTF-8")
			},
			"source_alive": !matches!(args.stratum, Stratum::SourceLoss)
		}),
	)?;
	drop(track);
	drop(broadcast);
	Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
	let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
	let args = Args::parse();
	ensure!(args.peer_count >= 1, "peer count must be positive");
	match args.role {
		Role::Publisher => run_publisher(args).await,
		Role::Late => run_late(args).await,
	}
}
