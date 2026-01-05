use anyhow::Context;

use chrono::prelude::*;
use moq_lite::*;

pub struct Publisher {
	track: TrackProducer,
}

impl Publisher {
	pub fn new(track: TrackProducer) -> Self {
		Self { track }
	}

	pub async fn run(mut self) -> anyhow::Result<()> {
		let start = Utc::now();
		let mut now = start;

		// Start at zero for testing
		let mut sequence = 0u64;

		loop {
			let segment = self.track.create_group(sequence.into()).unwrap();

			tracing::info!(sequence = sequence, "sending segment");

			sequence += 1;

			tokio::spawn(async move {
				if let Err(err) = Self::send_segment(segment, now).await {
					tracing::warn!("failed to send segment: {:?}", err);
				}
			});

			// Send every 1 second instead of every 1 minute
			let next = now + chrono::Duration::try_seconds(1).unwrap();
			let next = next.with_nanosecond(0).unwrap();

			let delay = (next - now).to_std().unwrap();
			tokio::time::sleep(delay).await;

			now = next; // just assume we didn't undersleep
		}
	}

	async fn send_segment(mut segment: GroupProducer, now: DateTime<Utc>) -> anyhow::Result<()> {
		// Send current timestamp with millisecond precision for latency measurement
		// Format: "YYYY-MM-DD HH:MM:SS.mmm" where mmm is milliseconds
		let timestamp = now.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
		tracing::debug!(timestamp = %timestamp, "writing frame");
		segment.write_frame(timestamp);
		segment.close();
		tracing::debug!("segment closed");

		Ok(())
	}
}
/// Latency statistics for measuring cache performance
#[derive(Default)]
pub struct LatencyStats {
	pub samples: Vec<f64>,
	pub min_ms: f64,
	pub max_ms: f64,
	pub total_ms: f64,
	pub count: u64,
}

impl LatencyStats {
	pub fn record(&mut self, latency_ms: f64) {
		self.samples.push(latency_ms);
		self.count += 1;
		self.total_ms += latency_ms;
		if self.count == 1 {
			self.min_ms = latency_ms;
			self.max_ms = latency_ms;
		} else {
			self.min_ms = self.min_ms.min(latency_ms);
			self.max_ms = self.max_ms.max(latency_ms);
		}
	}

	pub fn avg_ms(&self) -> f64 {
		if self.count == 0 {
			0.0
		} else {
			self.total_ms / self.count as f64
		}
	}

	pub fn percentile(&self, p: f64) -> f64 {
		if self.samples.is_empty() {
			return 0.0;
		}
		let mut sorted = self.samples.clone();
		sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
		let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
		sorted[idx.min(sorted.len() - 1)]
	}

	pub fn print_summary(&self) {
		if self.count == 0 {
			println!("\n=== Latency Statistics ===");
			println!("No samples collected");
			return;
		}
		println!("\n=== Latency Statistics ===");
		println!("Samples:    {}", self.count);
		println!("Min:        {:.2} ms", self.min_ms);
		println!("Max:        {:.2} ms", self.max_ms);
		println!("Average:    {:.2} ms", self.avg_ms());
		println!("P50:        {:.2} ms", self.percentile(50.0));
		println!("P95:        {:.2} ms", self.percentile(95.0));
		println!("P99:        {:.2} ms", self.percentile(99.0));
		println!("===========================\n");
	}
}

pub struct Subscriber {
	track: TrackConsumer,
	measure_latency: bool,
}

impl Subscriber {
	pub fn new(track: TrackConsumer) -> Self {
		Self {
			track,
			measure_latency: false,
		}
	}

	pub fn with_latency_measurement(track: TrackConsumer) -> Self {
		Self {
			track,
			measure_latency: true,
		}
	}

	pub async fn run(mut self) -> anyhow::Result<()> {
		let mut stats = LatencyStats::default();

		// Install Ctrl+C handler to print stats on exit
		let stats_on_exit = self.measure_latency;

		while let Some(mut group) = self.track.next_group().await? {
			let base = group
				.read_frame()
				.await
				.context("failed to get first object")?
				.context("empty group")?;

			let base_str = String::from_utf8_lossy(&base);

			if self.measure_latency {
				// Parse timestamp and calculate latency
				let recv_time = Utc::now();
				if let Ok(send_time) = NaiveDateTime::parse_from_str(&base_str, "%Y-%m-%d %H:%M:%S%.3f") {
					let send_time = send_time.and_utc();
					let latency = recv_time.signed_duration_since(send_time);
					let latency_ms = latency.num_milliseconds() as f64
						+ (latency.num_microseconds().unwrap_or(0) % 1000) as f64 / 1000.0;

					stats.record(latency_ms);
					println!(
						"[{}] latency: {:.2} ms (avg: {:.2} ms, min: {:.2} ms, max: {:.2} ms)",
						base_str,
						latency_ms,
						stats.avg_ms(),
						stats.min_ms,
						stats.max_ms
					);
				} else {
					// Fallback to old format parsing
					println!("{}", base_str);
				}
			} else {
				println!("{}", base_str);
			}

			while let Some(object) = group.read_frame().await? {
				let str = String::from_utf8_lossy(&object);
				println!("{base_str}{str}");
			}
		}

		// Print final statistics
		if stats_on_exit && stats.count > 0 {
			stats.print_summary();
		}

		Ok(())
	}
}
