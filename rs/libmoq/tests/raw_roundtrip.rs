//! Round-trip integration test for the RAW moq-lite C API.
//!
//! Requires a running dev relay:
//!   cd moq-src && ./target/debug/moq-relay dev/root.toml
//!
//! Publishes a raw frame on one session and receives it byte-identical on
//! another session subscribed to the same path, all through the C FFI.
//!
//! Skips (passes) automatically if no relay is reachable on localhost:4443.

use std::ffi::{c_void, CString};
use std::sync::mpsc;
use std::time::Duration;

// The C functions under test, called directly through the crate's rlib.
// (They are `pub extern "C"`, so this exercises the exact same code paths a
// C/Unity caller would hit, while linking cleanly in a Rust integration test.)
use moq::{
	moq_broadcast_consume, moq_broadcast_create_raw, moq_broadcast_publish_raw, moq_log_level,
	moq_session_connect, moq_track_create_raw, moq_track_subscribe, moq_track_write_raw,
};

extern "C" fn on_status(_user: *mut c_void, code: i32) {
	eprintln!("session status: {code}");
}

// Forwards each received frame's bytes over an mpsc::Sender<Vec<u8>>.
extern "C" fn on_frame(user: *mut c_void, data: *const u8, size: usize) {
	let tx = unsafe { &*(user as *const mpsc::Sender<Vec<u8>>) };
	let bytes = unsafe { std::slice::from_raw_parts(data, size) }.to_vec();
	let _ = tx.send(bytes);
}

fn relay_reachable() -> bool {
	std::net::TcpStream::connect_timeout(
		&"127.0.0.1:4443".parse().unwrap(),
		Duration::from_millis(300),
	)
	.is_ok()
}

#[test]
fn raw_roundtrip() {
	if !relay_reachable() {
		eprintln!("relay not reachable on localhost:4443; skipping (pass)");
		return;
	}

	unsafe {
		let level = CString::new("debug").unwrap();
		moq_log_level(level.as_ptr());

		let url = CString::new("http://localhost:4443/anon").unwrap();
		let path = CString::new("libmoq-test/raw").unwrap();
		let track_name = CString::new("data").unwrap();

		// --- Publisher session ---
		let pub_session = moq_session_connect(url.as_ptr(), Some(on_status), std::ptr::null_mut());
		assert!(pub_session > 0, "pub connect failed: {pub_session}");

		// --- Subscriber session ---
		let sub_session = moq_session_connect(url.as_ptr(), Some(on_status), std::ptr::null_mut());
		assert!(sub_session > 0, "sub connect failed: {sub_session}");

		// Give both sessions time to establish.
		std::thread::sleep(Duration::from_millis(1500));

		// Publisher: create + publish a raw broadcast/track.
		let broadcast = moq_broadcast_create_raw();
		assert!(broadcast > 0, "create_raw failed: {broadcast}");
		let track = moq_track_create_raw(broadcast, track_name.as_ptr());
		assert!(track > 0, "track_create_raw failed: {track}");
		let r = moq_broadcast_publish_raw(broadcast, pub_session, path.as_ptr());
		assert_eq!(r, 0, "publish_raw failed: {r}");

		// Subscriber: consume the broadcast and subscribe to the track.
		let (tx, rx) = mpsc::channel::<Vec<u8>>();
		let tx_box = Box::new(tx);
		let tx_ptr = Box::into_raw(tx_box) as *mut c_void;

		let consumer = moq_broadcast_consume(sub_session, path.as_ptr());
		assert!(consumer > 0, "consume failed: {consumer}");
		let task = moq_track_subscribe(consumer, track_name.as_ptr(), Some(on_frame), tx_ptr);
		assert!(task > 0, "subscribe failed: {task}");

		// Let the subscription/announcement settle.
		std::thread::sleep(Duration::from_millis(1000));

		// Publish a frame.
		let payload: &[u8] = b"hello-raw-moq-12345";
		let w = moq_track_write_raw(track, payload.as_ptr(), payload.len());
		assert_eq!(w, 0, "write_raw failed: {w}");

		// Await receipt.
		let received = rx
			.recv_timeout(Duration::from_secs(5))
			.expect("did not receive frame within timeout");

		assert_eq!(received, payload, "frame bytes mismatch");
		eprintln!("PASS: received {} bytes byte-identical", received.len());

		// Leak tx_ptr intentionally (process exits); avoids racing the bg task.
	}
}
