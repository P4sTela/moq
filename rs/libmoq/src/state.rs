use std::ops::{Deref, DerefMut};
use std::sync::{
	atomic::{AtomicBool, Ordering},
	Arc,
	LazyLock,
	Mutex,
	MutexGuard,
};

use tokio::sync::oneshot;
use url::Url;

use moq_lite::AsPath;

use crate::{ffi, Error, Id, NonZeroSlab};

/// A pending or resolved subscription to a broadcast.
///
/// We keep the scoped origin consumer and path so the spawned subscribe task
/// can wait for the broadcast to be announced online (robust with IETF MoQ)
/// before subscribing to a track.
struct BroadcastSub {
	origin: moq_lite::OriginConsumer,
	path: moq_lite::PathOwned,
}

struct Session {
	// The collection of published broadcasts.
	origin: moq_lite::OriginProducer,

	// The collection of subscribed/announced broadcasts received from the peer.
	subscribe: moq_lite::OriginConsumer,

	// A simple signal to notify the background task when closed.
	#[allow(dead_code)]
	closed: oneshot::Sender<()>,

	// Disable the FFI callback before asking the background task to stop. This
	// prevents a late Error::Closed callback from entering a torn-down Mono
	// runtime during Unity process shutdown.
	callback_active: Arc<AtomicBool>,
}

pub struct State {
	// All sessions by ID.
	sessions: NonZeroSlab<Session>, // TODO clean these up on error.

	// All broadcasts, indexed by an ID.
	broadcasts: NonZeroSlab<hang::BroadcastProducer>,

	// All tracks, indexed by an ID.
	tracks: NonZeroSlab<hang::import::Decoder>,

	// Raw (non-hang) broadcasts used by the Unity transport, indexed by an ID.
	raw_broadcasts: NonZeroSlab<moq_lite::BroadcastProducer>,

	// Raw (non-hang) track producers, indexed by an ID.
	raw_tracks_pub: NonZeroSlab<moq_lite::TrackProducer>,

	// Broadcast consumers obtained from a subscription, indexed by an ID.
	broadcast_consumers: NonZeroSlab<BroadcastSub>,

	// Background task handles (read loops, announce loops) so they can be aborted on close.
	tasks: NonZeroSlab<tokio::task::AbortHandle>,
}

pub struct StateGuard {
	_runtime: tokio::runtime::EnterGuard<'static>,
	state: MutexGuard<'static, State>,
}

impl Deref for StateGuard {
	type Target = State;
	fn deref(&self) -> &Self::Target {
		&self.state
	}
}

impl DerefMut for StateGuard {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.state
	}
}

impl State {
	pub fn lock() -> StateGuard {
		let runtime = RUNTIME.enter();
		let state = STATE.lock().unwrap();
		StateGuard {
			_runtime: runtime,
			state,
		}
	}
}

static RUNTIME: LazyLock<tokio::runtime::Handle> = LazyLock::new(|| {
	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let handle = runtime.handle().clone();

	std::thread::Builder::new()
		.name("libmoq".into())
		.spawn(move || {
			runtime.block_on(std::future::pending::<()>());
		})
		.expect("failed to spawn runtime thread");

	handle
});

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::new()));

impl State {
	fn new() -> Self {
		Self {
			sessions: Default::default(),
			broadcasts: Default::default(),
			tracks: Default::default(),
			raw_broadcasts: Default::default(),
			raw_tracks_pub: Default::default(),
			broadcast_consumers: Default::default(),
			tasks: Default::default(),
		}
	}

	pub fn session_connect(&mut self, url: Url, mut callback: ffi::Callback) -> Result<Id, Error> {
		// The origin used to publish our broadcasts to the peer.
		let publish = moq_lite::Origin::produce();
		// The origin used to receive broadcasts announced by the peer.
		let subscribe = moq_lite::Origin::produce();

		// Used just to notify when the session is removed from the map.
		let closed = oneshot::channel();
		let callback_active = callback.cancellation();

		let id = self.sessions.insert(Session {
			closed: closed.0,
			callback_active,
			origin: publish.producer,
			// The consumer side is cloned cheaply by `consume()` when subscribing/announcing.
			subscribe: subscribe.consumer,
		});

		tokio::spawn(async move {
			let err = tokio::select! {
				// No more receiver, which means [session_close] was called.
				_ = closed.1 => Ok(()),
				// The connection failed.
				res = Self::session_connect_run(url, publish.consumer, subscribe.producer, &mut callback) => res,
			}
			.err()
			.unwrap_or(Error::Closed);

			callback.call(err);
		});

		Ok(id)
	}

	async fn session_connect_run(
		url: Url,
		publish: moq_lite::OriginConsumer,
		subscribe: moq_lite::OriginProducer,
		callback: &mut ffi::Callback,
	) -> Result<(), Error> {
		let config = moq_native::ClientConfig::default();
		let client = config.init().map_err(|err| Error::Connect(Arc::new(err)))?;
		let connection = client.connect(url).await.map_err(|err| Error::Connect(Arc::new(err)))?;
		let session = moq_lite::Session::connect(connection, publish, subscribe).await?;
		callback.call(());

		session.closed().await?;
		Ok(())
	}

	pub fn session_close(&mut self, id: Id) -> Result<(), Error> {
		let session = self.sessions.remove(id).ok_or(Error::NotFound)?;
		session.callback_active.store(false, Ordering::Release);
		Ok(())
	}

	pub fn publish_broadcast<P: moq_lite::AsPath>(&mut self, broadcast: Id, session: Id, path: P) -> Result<(), Error> {
		let path = path.as_path();
		let broadcast = self.broadcasts.get_mut(broadcast).ok_or(Error::NotFound)?;
		let session = self.sessions.get_mut(session).ok_or(Error::NotFound)?;

		session.origin.publish_broadcast(path, broadcast.consume());

		Ok(())
	}

	pub fn create_broadcast(&mut self) -> Id {
		let broadcast = moq_lite::Broadcast::produce();
		self.broadcasts.insert(broadcast.producer.into())
	}

	pub fn remove_broadcast(&mut self, broadcast: Id) -> Result<(), Error> {
		self.broadcasts.remove(broadcast).ok_or(Error::NotFound)?;
		Ok(())
	}

	pub fn create_track(&mut self, broadcast: Id, format: &str, init: &[u8]) -> Result<Id, Error> {
		let broadcast = self.broadcasts.get_mut(broadcast).ok_or(Error::NotFound)?;
		let mut decoder = hang::import::Decoder::new(broadcast.clone(), format)
			.ok_or_else(|| Error::UnknownFormat(format.to_string()))?;

		let mut temp = init;
		decoder
			.initialize(&mut temp)
			.map_err(|err| Error::InitFailed(Arc::new(err)))?;
		assert!(init.is_empty(), "buffer was not fully consumed");

		let id = self.tracks.insert(decoder);
		Ok(id)
	}

	pub fn write_track(&mut self, track: Id, mut data: &[u8], pts: u64) -> Result<(), Error> {
		let track = self.tracks.get_mut(track).ok_or(Error::NotFound)?;

		let pts = hang::Timestamp::from_micros(pts)?;
		track
			.decode_frame(&mut data, Some(pts))
			.map_err(|err| Error::DecodeFailed(Arc::new(err)))?;
		assert!(data.is_empty(), "buffer was not fully consumed");

		Ok(())
	}

	pub fn remove_track(&mut self, track: Id) -> Result<(), Error> {
		self.tracks.remove(track).ok_or(Error::NotFound)?;
		Ok(())
	}

	// ===================== Raw moq-lite pub/sub API =====================

	/// Create a raw (non-hang) broadcast. Returns a handle into `raw_broadcasts`.
	pub fn create_broadcast_raw(&mut self) -> Id {
		let broadcast = moq_lite::Broadcast::produce();
		self.raw_broadcasts.insert(broadcast.producer)
	}

	pub fn remove_broadcast_raw(&mut self, broadcast: Id) -> Result<(), Error> {
		self.raw_broadcasts.remove(broadcast).ok_or(Error::NotFound)?;
		Ok(())
	}

	/// Publish a raw broadcast to the given session under `path`.
	pub fn publish_broadcast_raw<P: moq_lite::AsPath>(
		&mut self,
		broadcast: Id,
		session: Id,
		path: P,
	) -> Result<(), Error> {
		let path = path.as_path();
		let broadcast = self.raw_broadcasts.get_mut(broadcast).ok_or(Error::NotFound)?;
		let session = self.sessions.get_mut(session).ok_or(Error::NotFound)?;

		session.origin.publish_broadcast(path, broadcast.consume());
		Ok(())
	}

	/// Create a raw track on a raw broadcast. Returns a handle into `raw_tracks_pub`.
	pub fn create_track_raw(&mut self, broadcast: Id, name: &str) -> Result<Id, Error> {
		let broadcast = self.raw_broadcasts.get_mut(broadcast).ok_or(Error::NotFound)?;
		let track = broadcast.create_track(moq_lite::Track::new(name));
		Ok(self.raw_tracks_pub.insert(track))
	}

	/// Write a raw frame to a raw track. Each frame becomes its own single-frame group.
	pub fn write_track_raw(&mut self, track: Id, data: &[u8]) -> Result<(), Error> {
		let track = self.raw_tracks_pub.get_mut(track).ok_or(Error::NotFound)?;
		track.write_frame(bytes::Bytes::copy_from_slice(data));
		Ok(())
	}

	pub fn remove_track_raw(&mut self, track: Id) -> Result<(), Error> {
		self.raw_tracks_pub.remove(track).ok_or(Error::NotFound)?;
		Ok(())
	}

	/// Begin consuming a broadcast announced on the given session under `path`.
	///
	/// This does not block; the announce-aware wait is deferred to [Self::subscribe_track]
	/// (robust with IETF MoQ). Returns a handle into `broadcast_consumers`.
	pub fn consume_broadcast(&mut self, session: Id, path: &str) -> Result<Id, Error> {
		let session = self.sessions.get_mut(session).ok_or(Error::NotFound)?;
		let path = moq_lite::Path::from(path).to_owned();

		// Scope a cloned consumer to just this prefix so the subscribe task can
		// loop `announced()` waiting for the broadcast to come online.
		let origin = session
			.subscribe
			.consume_only(&[path.as_path()])
			.ok_or(Error::NotFound)?;

		Ok(self.broadcast_consumers.insert(BroadcastSub { origin, path }))
	}

	pub fn remove_broadcast_consumer(&mut self, id: Id) -> Result<(), Error> {
		self.broadcast_consumers.remove(id).ok_or(Error::NotFound)?;
		Ok(())
	}

	/// Subscribe to a track on a consumed broadcast, invoking `callback` per frame.
	///
	/// The spawned task waits for the broadcast to be announced online, subscribes
	/// to the track, then loops groups/frames delivering each frame's bytes.
	/// Returns a task handle that can be passed to [Self::unsubscribe_track].
	pub fn subscribe_track(
		&mut self,
		broadcast_consumer: Id,
		name: &str,
		callback: ffi::DataCallback,
	) -> Result<Id, Error> {
		let sub = self.broadcast_consumers.get_mut(broadcast_consumer).ok_or(Error::NotFound)?;
		let mut origin = sub.origin.consume();
		let path = sub.path.clone();
		let track = moq_lite::Track::new(name);

		let handle = tokio::spawn(async move {
			// Try the fast path; otherwise wait for the broadcast to be announced online.
			let broadcast = match origin.consume_broadcast(path.as_path()) {
				Some(broadcast) => broadcast,
				None => loop {
					match origin.announced().await {
						Some((_, Some(broadcast))) => break broadcast,
						Some((_, None)) => continue,
						None => return,
					}
				},
			};

			let mut track = broadcast.subscribe_track(&track);
			while let Ok(Some(mut group)) = track.next_group().await {
				while let Ok(Some(frame)) = group.read_frame().await {
					callback.call(&frame);
				}
			}
		});

		Ok(self.tasks.insert(handle.abort_handle()))
	}

	pub fn unsubscribe_track(&mut self, task: Id) -> Result<(), Error> {
		let handle = self.tasks.remove(task).ok_or(Error::NotFound)?;
		handle.abort();
		Ok(())
	}

	/// Watch for broadcast announcements on the given session matching `prefix`,
	/// invoking `callback(path, active)` for each (un)announcement.
	///
	/// Returns a task handle that can be passed to [Self::unsubscribe_track].
	pub fn session_announced(
		&mut self,
		session: Id,
		prefix: &str,
		callback: ffi::AnnounceCallback,
	) -> Result<Id, Error> {
		let session = self.sessions.get_mut(session).ok_or(Error::NotFound)?;
		let prefix = moq_lite::Path::from(prefix).to_owned();

		let mut origin = session
			.subscribe
			.consume_only(&[prefix.as_path()])
			.ok_or(Error::NotFound)?;

		let handle = tokio::spawn(async move {
			while let Some((path, active)) = origin.announced().await {
				callback.call(path.as_str(), active.is_some());
			}
		});

		Ok(self.tasks.insert(handle.abort_handle()))
	}
}
