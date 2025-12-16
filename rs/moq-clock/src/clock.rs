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
		// Send current timestamp as a single frame
		let timestamp = now.format("%Y-%m-%d %H:%M:%S").to_string();
		tracing::debug!(timestamp = %timestamp, "writing frame");
		segment.write_frame(timestamp);
		segment.close();
		tracing::debug!("segment closed");

		Ok(())
	}
}
pub struct Subscriber {
	track: TrackConsumer,
}

impl Subscriber {
	pub fn new(track: TrackConsumer) -> Self {
		Self { track }
	}

	pub async fn run(mut self) -> anyhow::Result<()> {
		while let Some(mut group) = self.track.next_group().await? {
			let base = group
				.read_frame()
				.await
				.context("failed to get first object")?
				.context("empty group")?;

			let base = String::from_utf8_lossy(&base);

			while let Some(object) = group.read_frame().await? {
				let str = String::from_utf8_lossy(&object);
				println!("{base}{str}");
			}
		}

		Ok(())
	}
}
