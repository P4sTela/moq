use std::{
	collections::HashMap,
	sync::{atomic, Arc},
};

use crate::{
	coding::{Reader, Stream},
	lite::{self, Version},
	model::BroadcastProducer,
	AsPath, Broadcast, Error, Frame, FrameProducer, Group, GroupProducer, OriginProducer, Path, PathOwned,
	TrackProducer,
};

use tokio::sync::oneshot;
use web_async::Lock;

#[derive(Clone)]
struct Subscription {
	track: TrackProducer,
	broadcast: PathOwned,
	track_name: String,
	first_group_received: bool,
	last_group_sequence: Option<u64>,
	group_count: u64,
	gap_count: u64,
	skipped_total: u64,
	max_skipped: u64,
}

#[derive(Clone)]
pub(super) struct Subscriber<S: web_transport_trait::Session> {
	session: S,

	origin: Option<OriginProducer>,
	subscribes: Lock<HashMap<u64, Subscription>>,
	next_id: Arc<atomic::AtomicU64>,
	version: Version,
}

impl<S: web_transport_trait::Session> Subscriber<S> {
	pub fn new(session: S, origin: Option<OriginProducer>, version: Version) -> Self {
		Self {
			session,
			origin,
			subscribes: Default::default(),
			next_id: Default::default(),
			version,
		}
	}

	/// Send a signal when the subscriber is initialized.
	pub async fn run(self, init: oneshot::Sender<()>) -> Result<(), Error> {
		tokio::select! {
			Err(err) = self.clone().run_announce(init) => Err(err),
			res = self.run_uni() => res,
		}
	}

	async fn run_uni(self) -> Result<(), Error> {
		loop {
			let stream = self
				.session
				.accept_uni()
				.await
				.map_err(|err| Error::Transport(Arc::new(err)))?;

			let stream = Reader::new(stream, self.version);
			let this = self.clone();

			web_async::spawn(async move {
				if let Err(err) = this.run_uni_stream(stream).await {
					tracing::debug!(%err, "error running uni stream");
				}
			});
		}
	}

	async fn run_uni_stream(mut self, mut stream: Reader<S::RecvStream, Version>) -> Result<(), Error> {
		let kind = stream.decode().await?;

		let res = match kind {
			lite::DataType::Group => self.recv_group(&mut stream).await,
		};

		if let Err(err) = res {
			stream.abort(&err);
		}

		Ok(())
	}

	async fn run_announce(mut self, init: oneshot::Sender<()>) -> Result<(), Error> {
		if self.origin.is_none() {
			// Don't do anything if there's no origin configured.
			let _ = init.send(());
			return Ok(());
		}

		let mut stream = Stream::open(&self.session, self.version).await?;
		stream.writer.encode(&lite::ControlType::Announce).await?;

		tracing::trace!(root = %self.log_path(""), "announced start");

		// Ask for everything.
		// TODO This should actually ask for each root.
		let msg = lite::AnnouncePlease { prefix: "".into() };
		stream.writer.encode(&msg).await?;

		let mut producers = HashMap::new();

		let msg: lite::AnnounceInit = stream.reader.decode().await?;
		for path in msg.suffixes {
			self.start_announce(path, &mut producers)?;
		}

		let _ = init.send(());

		while let Some(announce) = stream.reader.decode_maybe::<lite::Announce>().await? {
			match announce {
				lite::Announce::Active { suffix: path } => {
					self.start_announce(path, &mut producers)?;
				}
				lite::Announce::Ended { suffix: path } => {
					tracing::debug!(broadcast = %self.log_path(&path), "unannounced");

					// Close the producer.
					let mut producer = producers.remove(&path.into_owned()).ok_or(Error::NotFound)?;
					producer.close();
				}
			}
		}

		// Close the stream when there's nothing more to announce.
		stream.writer.finish()?;
		stream.writer.closed().await
	}

	fn start_announce(
		&mut self,
		path: PathOwned,
		producers: &mut HashMap<PathOwned, BroadcastProducer>,
	) -> Result<(), Error> {
		tracing::debug!(broadcast = %self.log_path(&path), "announce");

		// Ignore duplicate announcements - we already have a proxy for this broadcast.
		// This can happen in bidirectional scenarios where local and remote broadcasts
		// are published to the same origin, causing feedback loops.
		if producers.contains_key(&path) {
			tracing::debug!(broadcast = %self.log_path(&path), "ignoring duplicate announce");
			return Ok(());
		}

		let broadcast = Broadcast::produce();
		producers.insert(path.to_owned(), broadcast.producer.clone());

		// Run the broadcast in the background until all consumers are dropped.
		self.origin
			.as_mut()
			.unwrap()
			.publish_broadcast(path.clone(), broadcast.consumer);

		web_async::spawn(self.clone().run_broadcast(path, broadcast.producer));

		Ok(())
	}

	async fn run_broadcast(self, path: PathOwned, mut broadcast: BroadcastProducer) {
		// Actually start serving subscriptions.
		loop {
			// Keep serving requests until there are no more consumers.
			// This way we'll clean up the task when the broadcast is no longer needed.
			let track = tokio::select! {
				_ = broadcast.unused() => break,
				producer = broadcast.requested_track() => match producer {
					Some(producer) => producer,
					None => break,
				},
				_ = self.session.closed() => break,
			};

			let id = self.next_id.fetch_add(1, atomic::Ordering::Relaxed);
			let mut this = self.clone();

			let path = path.clone();
			web_async::spawn(async move {
				this.run_subscribe(id, path, track).await;
				this.subscribes.lock().remove(&id);
			});
		}
	}

	async fn run_subscribe(&mut self, id: u64, broadcast: Path<'_>, track: TrackProducer) {
		self.subscribes.lock().insert(
			id,
			Subscription {
				track: track.clone(),
				broadcast: broadcast.to_owned(),
				track_name: track.info.name.clone(),
				first_group_received: false,
				last_group_sequence: None,
				group_count: 0,
				gap_count: 0,
				skipped_total: 0,
				max_skipped: 0,
			},
		);

		let msg = lite::Subscribe {
			id,
			broadcast: broadcast.to_owned(),
			track: (&track.info.name).into(),
			priority: track.info.priority,
		};

		tracing::info!(id, broadcast = %self.log_path(&broadcast), track = %track.info.name, "subscribe started");

		let res = tokio::select! {
			_ = track.unused() => Err(Error::Cancel),
			res = self.run_track(msg) => res,
		};

		let summary = self
			.subscribes
			.lock()
			.get(&id)
			.map(|subscription| {
				(
					subscription.group_count,
					subscription.gap_count,
					subscription.skipped_total,
					subscription.max_skipped,
				)
			})
			.unwrap_or_default();
		tracing::info!(
			id,
			broadcast = %self.log_path(&broadcast),
			track = %track.info.name,
			groups = summary.0,
			gap_count = summary.1,
			skipped_total = summary.2,
			max_skipped = summary.3,
			"incoming group summary",
		);

		match res {
			Err(Error::Cancel) | Err(Error::Transport(_)) => {
				tracing::info!(id, broadcast = %self.log_path(&broadcast), track = %track.info.name, "subscribe cancelled");
				track.abort(Error::Cancel);
			}
			Err(err) => {
				tracing::warn!(id, broadcast = %self.log_path(&broadcast), track = %track.info.name, %err, "subscribe error");
				track.abort(err);
			}
			_ => {
				tracing::info!(id, broadcast = %self.log_path(&broadcast), track = %track.info.name, "subscribe complete");
				track.close();
			}
		}
	}

	async fn run_track(&mut self, msg: lite::Subscribe<'_>) -> Result<(), Error> {
		let mut stream = Stream::open(&self.session, self.version).await?;
		stream.writer.encode(&lite::ControlType::Subscribe).await?;

		if let Err(err) = self.run_track_stream(&mut stream, msg).await {
			stream.writer.abort(&err);
			return Err(err);
		}

		stream.writer.finish()?;
		stream.writer.closed().await
	}

	async fn run_track_stream(
		&mut self,
		stream: &mut Stream<S, Version>,
		msg: lite::Subscribe<'_>,
	) -> Result<(), Error> {
		stream.writer.encode(&msg).await?;

		// TODO use the response correctly populate the track info
		let _info: lite::SubscribeOk = stream.reader.decode().await?;

		// Wait until the stream is closed
		stream.reader.closed().await?;

		Ok(())
	}

	pub async fn recv_group(&mut self, stream: &mut Reader<S::RecvStream, Version>) -> Result<(), Error> {
		let hdr: lite::Group = stream.decode().await?;

		let (group, first_group, previous, broadcast, track_name) = {
			let mut subs = self.subscribes.lock();
			let subscription = subs.get_mut(&hdr.subscribe).ok_or(Error::Cancel)?;

			subscription.group_count += 1;
			let group = Group { sequence: hdr.sequence };
			let group = subscription.track.create_group(group).ok_or(Error::Old)?;
			let first_group = !subscription.first_group_received;
			let previous = subscription.last_group_sequence;
			subscription.first_group_received = true;
			subscription.last_group_sequence = Some(
				previous.map_or(hdr.sequence, |previous| previous.max(hdr.sequence)),
			);
			(
				group,
				first_group,
				previous,
				subscription.broadcast.clone(),
				subscription.track_name.clone(),
			)
		};

		if let Some(previous) = previous {
			if hdr.sequence > previous.saturating_add(1) {
				let skipped = hdr.sequence - previous - 1;
				let mut subs = self.subscribes.lock();
				if let Some(subscription) = subs.get_mut(&hdr.subscribe) {
					subscription.gap_count += 1;
					subscription.skipped_total += skipped;
					subscription.max_skipped = subscription.max_skipped.max(skipped);
				}
				tracing::debug!(
					broadcast = %broadcast,
					track = %track_name,
					subscribe = hdr.subscribe,
					previous,
					sequence = hdr.sequence,
					skipped,
					"incoming group sequence gap",
				);
			}
		}
		if first_group {
			tracing::debug!(
				broadcast = %broadcast,
				track = %track_name,
				subscribe = hdr.subscribe,
				sequence = hdr.sequence,
				"first group received",
			);
		}

		let res = tokio::select! {
			_ = group.unused() => Err(Error::Cancel),
			res = self.run_group(stream, group.clone()) => res,
		};

		match res {
			Err(Error::Cancel) | Err(Error::Transport(_)) => {
				tracing::trace!(group = %group.info.sequence, "group cancelled");
				group.abort(Error::Cancel);
			}
			Err(err) => {
				tracing::debug!(%err, group = %group.info.sequence, "group error");
				group.abort(err);
			}
			_ => {
				tracing::trace!(group = %group.info.sequence, "group complete");
				group.close();
			}
		}

		Ok(())
	}

	async fn run_group(
		&mut self,
		stream: &mut Reader<S::RecvStream, Version>,
		mut group: GroupProducer,
	) -> Result<(), Error> {
		while let Some(size) = stream.decode_maybe::<u64>().await? {
			let frame = group.create_frame(Frame { size });

			let res = tokio::select! {
				_ = frame.unused() => Err(Error::Cancel),
				res = self.run_frame(stream, frame.clone()) => res,
			};

			if let Err(err) = res {
				frame.abort(err.clone());
				return Err(err);
			}
		}

		group.close();

		Ok(())
	}

	async fn run_frame(
		&mut self,
		stream: &mut Reader<S::RecvStream, Version>,
		mut frame: FrameProducer,
	) -> Result<(), Error> {
		let mut remain = frame.info.size;

		tracing::trace!(size = %frame.info.size, "reading frame");

		const MAX_CHUNK: usize = 1024 * 1024; // 1 MiB
		while remain > 0 {
			let chunk = stream
				.read(MAX_CHUNK.min(remain as usize))
				.await?
				.ok_or(Error::WrongSize)?;
			remain = remain.checked_sub(chunk.len() as u64).ok_or(Error::WrongSize)?;
			frame.write_chunk(chunk);
		}

		tracing::trace!(size = %frame.info.size, "read frame");

		frame.close();

		Ok(())
	}

	fn log_path(&self, path: impl AsPath) -> Path<'_> {
		self.origin.as_ref().unwrap().root().join(path)
	}
}
