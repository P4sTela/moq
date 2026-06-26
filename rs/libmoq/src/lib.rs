mod error;
mod ffi;
mod id;
mod state;

pub use error::*;
pub use id::*;
use state::*;

use std::ffi::c_void;
use std::os::raw::c_char;
use std::str::FromStr;

use tracing::Level;

/// Initialize the library with a log level.
///
/// This should be called before any other functions.
/// The log_level is a string: "error", "warn", "info", "debug", "trace"
///
/// Returns a zero on success, or a negative code on failure.
///
/// # Safety
/// - The caller must ensure that level is a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn moq_log_level(level: *const c_char) -> i32 {
	ffi::return_code(move || {
		match ffi::parse_str(level)? {
			"" => moq_native::Log::default(),
			level => moq_native::Log {
				level: Level::from_str(level)?,
			},
		}
		.init();

		Ok(())
	})
}

/// Start establishing a connection to a MoQ server.
///
/// This may be called multiple times to connect to different servers.
/// Broadcast may be published before or after the connection is established.
///
/// Returns a non-zero handle to the session on success, or a negative code on (immediate) failure.
/// You should call [moq_session_close], even on error, to free up resources.
///
/// The callback is called on success (status 0) and later when closed (status non-zero).
///
/// # Safety
/// - The caller must ensure that url is a valid null-terminated C string.
/// - The caller must ensure that callback is a valid function pointer, or null.
/// - The caller must ensure that user_data is a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn moq_session_connect(
	url: *const c_char,
	callback: Option<extern "C" fn(user_data: *mut c_void, code: i32)>,
	user_data: *mut c_void,
) -> i32 {
	ffi::return_code(move || {
		let url = ffi::parse_url(url)?;
		let callback = ffi::Callback::new(user_data, callback);
		State::lock().session_connect(url, callback)
	})
}

/// Close a connection to a MoQ server.
///
/// Returns a zero on success, or a negative code on failure.
///
/// The [moq_session_connect] callback will be called with [Error::Closed].
#[no_mangle]
pub extern "C" fn moq_session_close(id: i32) -> i32 {
	ffi::return_code(move || {
		let id = ffi::parse_id(id)?;
		State::lock().session_close(id)
	})
}

/// Create a new broadcast; a collection of tracks.
///
/// Returns a non-zero handle to the broadcast on success.
#[no_mangle]
pub extern "C" fn moq_broadcast_create() -> i32 {
	ffi::return_code(move || State::lock().create_broadcast())
}

/// Remove a broadcast and all its tracks.
///
/// Returns a zero on success, or a negative code on failure.
#[no_mangle]
pub extern "C" fn moq_broadcast_close(id: i32) -> i32 {
	ffi::return_code(move || {
		let id = ffi::parse_id(id)?;
		State::lock().remove_broadcast(id)
	})
}

/// Publish the broadcast to the indicated session with the given path.
///
/// Returns a zero on success, or a negative code on failure.
/// The same broadcast may be published to multiple connections.
///
/// # Safety
/// - The caller must ensure that path is a valid null-terminated C string, or null.
// TODO add an unpublish method.
#[no_mangle]
pub unsafe extern "C" fn moq_broadcast_publish(id: i32, session: i32, path: *const c_char) -> i32 {
	ffi::return_code(move || {
		let id = ffi::parse_id(id)?;
		let session = ffi::parse_id(session)?;
		let path = ffi::parse_str(path)?;
		State::lock().publish_broadcast(id, session, path)
	})
}

/// Create a new track for a broadcast.
///
/// The encoding of `extra` depends on the `format`.
/// See [hang::import::Generic] for the available formats.
///
/// Returns a non-zero handle to the track on success, or a negative code on failure.
///
/// # Safety
/// - The caller must ensure that format is a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn moq_track_create(
	broadcast: i32,
	format: *const c_char,
	init: *const u8,
	init_size: usize,
) -> i32 {
	ffi::return_code(move || {
		let broadcast = ffi::parse_id(broadcast)?;
		let format = ffi::parse_str(format)?;
		let init = ffi::parse_slice(init, init_size)?;

		State::lock().create_track(broadcast, format, init)
	})
}

/// Remove a track from a broadcast.
#[no_mangle]
pub extern "C" fn moq_track_close(id: i32) -> i32 {
	ffi::return_code(move || {
		let id = ffi::parse_id(id)?;
		State::lock().remove_track(id)
	})
}

/// Write data to a track.
///
/// The encoding of `data` depends on the track `format`.
/// The timestamp is in microseconds.
///
/// Returns a zero on success, or a negative code on failure.
///
/// # Safety
/// - The caller must ensure that data is a valid pointer, or null.
#[no_mangle]
pub unsafe extern "C" fn moq_track_write(id: i32, data: *const u8, data_size: usize, pts: u64) -> i32 {
	ffi::return_code(move || {
		let id = ffi::parse_id(id)?;
		let data = ffi::parse_slice(data, data_size)?;
		State::lock().write_track(id, data, pts)
	})
}

// ============================================================================
// Raw moq-lite pub/sub API (no hang, no pts) — for the Unity native transport.
//
// These send/receive arbitrary byte frames. Each written frame becomes its own
// independently-deliverable single-frame group.
//
// IMPORTANT: subscribe/announce callbacks fire on the background runtime thread,
// not the caller's thread. The C# side must marshal to its main thread.
// ============================================================================

/// Create a new RAW (non-hang) broadcast; a collection of raw byte tracks.
///
/// Returns a non-zero handle to the raw broadcast on success.
#[no_mangle]
pub extern "C" fn moq_broadcast_create_raw() -> i32 {
	ffi::return_code(move || State::lock().create_broadcast_raw())
}

/// Remove a RAW broadcast and all its tracks.
///
/// Returns a zero on success, or a negative code on failure.
#[no_mangle]
pub extern "C" fn moq_broadcast_close_raw(id: i32) -> i32 {
	ffi::return_code(move || {
		let id = ffi::parse_id(id)?;
		State::lock().remove_broadcast_raw(id)
	})
}

/// Publish a RAW broadcast to the indicated session with the given path.
///
/// Returns a zero on success, or a negative code on failure.
///
/// # Safety
/// - The caller must ensure that path is a valid null-terminated C string, or null.
#[no_mangle]
pub unsafe extern "C" fn moq_broadcast_publish_raw(id: i32, session: i32, path: *const c_char) -> i32 {
	ffi::return_code(move || {
		let id = ffi::parse_id(id)?;
		let session = ffi::parse_id(session)?;
		let path = ffi::parse_str(path)?;
		State::lock().publish_broadcast_raw(id, session, path)
	})
}

/// Create a new RAW track on a RAW broadcast.
///
/// Returns a non-zero handle to the track on success, or a negative code on failure.
///
/// # Safety
/// - The caller must ensure that name is a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn moq_track_create_raw(broadcast: i32, name: *const c_char) -> i32 {
	ffi::return_code(move || {
		let broadcast = ffi::parse_id(broadcast)?;
		let name = ffi::parse_str(name)?;
		State::lock().create_track_raw(broadcast, name)
	})
}

/// Write a raw byte frame to a RAW track. Each frame becomes its own group.
///
/// Returns a zero on success, or a negative code on failure.
///
/// # Safety
/// - The caller must ensure that data is a valid pointer, or null.
#[no_mangle]
pub unsafe extern "C" fn moq_track_write_raw(track: i32, data: *const u8, size: usize) -> i32 {
	ffi::return_code(move || {
		let track = ffi::parse_id(track)?;
		let data = ffi::parse_slice(data, size)?;
		State::lock().write_track_raw(track, data)
	})
}

/// Remove a RAW track from a broadcast.
#[no_mangle]
pub extern "C" fn moq_track_close_raw(id: i32) -> i32 {
	ffi::return_code(move || {
		let id = ffi::parse_id(id)?;
		State::lock().remove_track_raw(id)
	})
}

/// Begin consuming a broadcast announced on the given session under `path`.
///
/// Returns a non-zero handle to the broadcast consumer on success.
/// The actual announce-aware wait happens when you subscribe to a track.
///
/// # Safety
/// - The caller must ensure that path is a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn moq_broadcast_consume(session: i32, path: *const c_char) -> i32 {
	ffi::return_code(move || {
		let session = ffi::parse_id(session)?;
		let path = ffi::parse_str(path)?;
		State::lock().consume_broadcast(session, path)
	})
}

/// Close a broadcast consumer handle.
#[no_mangle]
pub extern "C" fn moq_broadcast_consumer_close(id: i32) -> i32 {
	ffi::return_code(move || {
		let id = ffi::parse_id(id)?;
		State::lock().remove_broadcast_consumer(id)
	})
}

/// Subscribe to a track on a consumed broadcast.
///
/// The callback is invoked once per received frame with the raw bytes.
/// Returns a non-zero task handle (pass to [moq_track_unsubscribe] to stop).
///
/// NOTE: The callback fires on the background runtime thread, not the caller's.
///
/// # Safety
/// - The caller must ensure that name is a valid null-terminated C string.
/// - The caller must ensure that callback is a valid function pointer, or null.
/// - The caller must ensure that user_data is valid for the lifetime of the subscription.
#[no_mangle]
pub unsafe extern "C" fn moq_track_subscribe(
	broadcast_consumer: i32,
	name: *const c_char,
	callback: Option<extern "C" fn(user_data: *mut c_void, data: *const u8, size: usize)>,
	user_data: *mut c_void,
) -> i32 {
	ffi::return_code(move || {
		let broadcast_consumer = ffi::parse_id(broadcast_consumer)?;
		let name = ffi::parse_str(name)?;
		let callback = ffi::DataCallback::new(user_data, callback);
		State::lock().subscribe_track(broadcast_consumer, name, callback)
	})
}

/// Stop a track subscription started by [moq_track_subscribe].
///
/// Returns a zero on success, or a negative code on failure.
#[no_mangle]
pub extern "C" fn moq_track_unsubscribe(task: i32) -> i32 {
	ffi::return_code(move || {
		let task = ffi::parse_id(task)?;
		State::lock().unsubscribe_track(task)
	})
}

/// Watch for broadcast (un)announcements on the given session matching `prefix`.
///
/// The callback is invoked with the absolute path and active=1 (online) / 0 (offline).
/// Returns a non-zero task handle (pass to [moq_track_unsubscribe] to stop).
///
/// NOTE: The callback fires on the background runtime thread, not the caller's.
/// The `path` argument is only valid for the duration of the call.
///
/// # Safety
/// - The caller must ensure that prefix is a valid null-terminated C string.
/// - The caller must ensure that callback is a valid function pointer, or null.
/// - The caller must ensure that user_data is valid for the lifetime of the watch.
#[no_mangle]
pub unsafe extern "C" fn moq_session_announced(
	session: i32,
	prefix: *const c_char,
	callback: Option<extern "C" fn(user_data: *mut c_void, path: *const c_char, active: i32)>,
	user_data: *mut c_void,
) -> i32 {
	ffi::return_code(move || {
		let session = ffi::parse_id(session)?;
		let prefix = ffi::parse_str(prefix)?;
		let callback = ffi::AnnounceCallback::new(user_data, callback);
		State::lock().session_announced(session, prefix, callback)
	})
}
