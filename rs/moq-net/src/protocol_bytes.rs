//! Optional MoQ application-stream byte accounting.
//!
//! The counters in this module sit below the MoQ codecs and above the WebTransport
//! implementation. They count bytes accepted by MoQ stream/datagram APIs, excluding
//! QUIC/TLS/HTTP-3 framing, acknowledgements, retransmissions, and transport-handshake
//! bytes. MoQ SETUP bytes are included because the meter is attached before SETUP parsing.
//!
//! For the `moq-lite` wire format, stream bytes are classified from the first stream
//! type varint: bidirectional streams are control, unidirectional SETUP streams are
//! control, and unidirectional GROUP streams are data. IETF `moq-transport` streams
//! use their message/stream type registry: known bidi messages and SETUP/FETCH uni
//! streams are control, valid group uni streams are data, and unknown values remain
//! unclassified. Unknown protocols and invalid stream types remain unclassified rather
//! than being silently counted as control.
//! Counter snapshots serialize aggregate and bucket updates, so their values reconcile
//! even while a stream-type varint is split across transport reads or writes.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use web_transport_trait::{RecvStream, SendStream, Session as TransportSession, Stats};

/// A cloneable opt-in counter for bytes accepted by MoQ stream/datagram APIs.
#[derive(Clone, Debug)]
pub struct ProtocolBytes {
	inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
	bytes_sent: u64,
	bytes_received: u64,
	control_bytes_sent: u64,
	control_bytes_received: u64,
	data_bytes_sent: u64,
	data_bytes_received: u64,
	unclassified_bytes_sent: u64,
	unclassified_bytes_received: u64,
}

/// A point-in-time snapshot of MoQ application-stream bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProtocolByteSnapshot {
	/// Bytes accepted by the local transport for MoQ writes.
	pub bytes_sent: u64,
	/// Bytes returned by the local transport for MoQ reads.
	pub bytes_received: u64,
	/// Control-stream bytes accepted by the local transport for MoQ writes.
	pub control_bytes_sent: u64,
	/// Control-stream bytes returned by the local transport for MoQ reads.
	pub control_bytes_received: u64,
	/// Data-stream/datagram bytes accepted by the local transport for MoQ writes.
	pub data_bytes_sent: u64,
	/// Data-stream/datagram bytes returned by the local transport for MoQ reads.
	pub data_bytes_received: u64,
	/// Bytes from unknown or invalid stream types accepted by the local transport for writes.
	pub unclassified_bytes_sent: u64,
	/// Bytes from unknown or invalid stream types returned by the local transport for reads.
	pub unclassified_bytes_received: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ByteClass {
	Control,
	Data,
	Unclassified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamKind {
	Bi,
	Uni,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IetfVersion {
	Draft14,
	Draft15,
	Draft16,
	Draft17,
	Draft18,
	Draft19,
}

impl IetfVersion {
	fn from_protocol(protocol: &str) -> Option<Self> {
		match protocol {
			crate::ALPN_14 => Some(Self::Draft14),
			crate::ALPN_15 => Some(Self::Draft15),
			crate::ALPN_16 => Some(Self::Draft16),
			crate::ALPN_17 => Some(Self::Draft17),
			crate::ALPN_18 => Some(Self::Draft18),
			crate::ALPN_19 => Some(Self::Draft19),
			_ => None,
		}
	}

	fn as_ietf(self) -> crate::ietf::Version {
		match self {
			Self::Draft14 => crate::ietf::Version::Draft14,
			Self::Draft15 => crate::ietf::Version::Draft15,
			Self::Draft16 => crate::ietf::Version::Draft16,
			Self::Draft17 => crate::ietf::Version::Draft17,
			Self::Draft18 => crate::ietf::Version::Draft18,
			Self::Draft19 => crate::ietf::Version::Draft19,
		}
	}

	fn has_modern_setup(self) -> bool {
		matches!(self, Self::Draft17 | Self::Draft18 | Self::Draft19)
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProtocolFamily {
	Lite,
	/// The `moql` ALPN is shared by Lite01/02 and IETF Draft14 setup negotiation.
	SharedMoql,
	Ietf(IetfVersion),
	Unknown,
}

impl ProtocolFamily {
	fn from_protocol(protocol: Option<&str>) -> Self {
		match protocol {
			// Classify the union of the two legacy wire registries until SETUP negotiation
			// selects the version. This avoids mislabeling the first bidi SETUP stream when
			// an IETF14 peer uses the same ALPN as Lite01/02.
			Some(crate::ALPN_LITE) => Self::SharedMoql,
			Some(protocol) => match crate::Version::from_alpn(protocol) {
				Some(crate::Version::Lite(_)) => Self::Lite,
				Some(crate::Version::Ietf(_)) => IetfVersion::from_protocol(protocol)
					.map(Self::Ietf)
					.unwrap_or(Self::Unknown),
				None => Self::Unknown,
			},
			None => Self::Unknown,
		}
	}

	fn from_version(version: crate::Version) -> Self {
		match version {
			crate::Version::Lite(_) => Self::Lite,
			crate::Version::Ietf(version) => Self::Ietf(match version {
				crate::ietf::Version::Draft14 => IetfVersion::Draft14,
				crate::ietf::Version::Draft15 => IetfVersion::Draft15,
				crate::ietf::Version::Draft16 => IetfVersion::Draft16,
				crate::ietf::Version::Draft17 => IetfVersion::Draft17,
				crate::ietf::Version::Draft18 => IetfVersion::Draft18,
				crate::ietf::Version::Draft19 => IetfVersion::Draft19,
			}),
		}
	}
}

#[derive(Debug)]
struct ClassifierState {
	class: Option<ByteClass>,
	prefix: [u8; 9],
	prefix_len: usize,
	pending_bytes: usize,
}

#[derive(Debug)]
struct StreamClassifier {
	kind: StreamKind,
	family: Arc<Mutex<ProtocolFamily>>,
	state: Mutex<ClassifierState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClassificationDelta {
	class: ByteClass,
	bytes: usize,
	/// Bytes provisionally counted as unclassified while a split type varint was pending.
	reclassify_unclassified: usize,
}

impl StreamClassifier {
	#[cfg(test)]
	fn new(kind: StreamKind, family: ProtocolFamily) -> Self {
		Self::with_family(kind, Arc::new(Mutex::new(family)))
	}

	fn with_family(kind: StreamKind, family: Arc<Mutex<ProtocolFamily>>) -> Self {
		Self {
			kind,
			family,
			state: Mutex::new(ClassifierState {
				class: None,
				prefix: [0; 9],
				prefix_len: 0,
				pending_bytes: 0,
			}),
		}
	}

	/// Classify bytes accepted by one I/O operation. Prefix bytes are provisionally
	/// counted as unclassified until a split stream-type varint is complete; the
	/// returned delta then moves those bytes into their final class atomically.
	fn record(&self, bytes: &[u8]) -> Option<ClassificationDelta> {
		if bytes.is_empty() {
			return None;
		}

		let family = *self.family.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		if let Some(class) = state.class {
			return Some(ClassificationDelta {
				class,
				bytes: bytes.len(),
				reclassify_unclassified: 0,
			});
		}

		if family == ProtocolFamily::Unknown {
			state.class = Some(ByteClass::Unclassified);
			return Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: bytes.len(),
				reclassify_unclassified: 0,
			});
		}

		let previous = state.pending_bytes;
		state.pending_bytes += bytes.len();
		let prefix_start = state.prefix_len;
		let available = bytes.len().min(9 - prefix_start);
		state.prefix[prefix_start..prefix_start + available].copy_from_slice(&bytes[..available]);
		state.prefix_len += available;

		let Some(expected) = varint_len(state.prefix[0], family) else {
			state.class = Some(ByteClass::Unclassified);
			state.pending_bytes = 0;
			return Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: previous + bytes.len(),
				reclassify_unclassified: previous,
			});
		};
		if state.prefix_len < expected {
			return Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: bytes.len(),
				reclassify_unclassified: 0,
			});
		}

		let value = decode_prefix(&state.prefix[..expected], family);
		let class = match family {
			ProtocolFamily::Lite => classify_lite_stream(self.kind, value),
			ProtocolFamily::SharedMoql => classify_shared_moql(self.kind, value),
			ProtocolFamily::Ietf(version) => classify_ietf_stream(version, self.kind, value),
			ProtocolFamily::Unknown => ByteClass::Unclassified,
		};
		state.class = Some(class);
		state.pending_bytes = 0;
		Some(ClassificationDelta {
			class,
			bytes: previous + bytes.len(),
			reclassify_unclassified: previous,
		})
	}

	/// Mark an incomplete prefix as unclassified when the stream closes or resets.
	/// Its bytes were already provisionally counted in that bucket.
	fn flush(&self) {
		let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		if state.class.is_none() && state.pending_bytes > 0 {
			state.class = Some(ByteClass::Unclassified);
			state.pending_bytes = 0;
		}
	}
}

fn classify_lite_stream(kind: StreamKind, value: u64) -> ByteClass {
	match kind {
		StreamKind::Bi => {
			if value <= 6 {
				ByteClass::Control
			} else {
				ByteClass::Unclassified
			}
		}
		StreamKind::Uni => match value {
			0 => ByteClass::Data,
			1 => ByteClass::Control,
			_ => ByteClass::Unclassified,
		},
	}
}

fn classify_shared_moql(kind: StreamKind, value: u64) -> ByteClass {
	match (
		classify_lite_stream(kind, value),
		classify_ietf_stream(IetfVersion::Draft14, kind, value),
	) {
		(ByteClass::Data, _) | (_, ByteClass::Data) => ByteClass::Data,
		(ByteClass::Control, _) | (_, ByteClass::Control) => ByteClass::Control,
		_ => ByteClass::Unclassified,
	}
}

fn classify_ietf_stream(version: IetfVersion, kind: StreamKind, value: u64) -> ByteClass {
	match kind {
		StreamKind::Bi => {
			if crate::ietf::is_control_message_type(version.as_ietf(), value)
				|| (!version.has_modern_setup() && matches!(value, 0x20 | 0x21))
			{
				ByteClass::Control
			} else {
				ByteClass::Unclassified
			}
		}
		StreamKind::Uni => {
			if value == crate::ietf::FetchHeader::TYPE
				|| (version.has_modern_setup() && value == crate::setup::SETUP_V17)
			{
				ByteClass::Control
			} else if crate::ietf::is_group_stream_type(version.as_ietf(), value) {
				ByteClass::Data
			} else {
				ByteClass::Unclassified
			}
		}
	}
}

// Draft17+ IETF versions changed protocol varints from QUIC's two-bit
// length tag to a leading-ones length prefix. Draft17 also rejects the
// seven-byte form that Draft18+ permits.
fn varint_len(first: u8, family: ProtocolFamily) -> Option<usize> {
	match family {
		ProtocolFamily::Ietf(IetfVersion::Draft17) if first.leading_ones() == 6 => None,
		ProtocolFamily::Ietf(IetfVersion::Draft17 | IetfVersion::Draft18 | IetfVersion::Draft19) => {
			Some(first.leading_ones() as usize + 1)
		}
		ProtocolFamily::Lite
		| ProtocolFamily::SharedMoql
		| ProtocolFamily::Ietf(IetfVersion::Draft14 | IetfVersion::Draft15 | IetfVersion::Draft16)
		| ProtocolFamily::Unknown => Some(1 << (first >> 6)),
	}
}

fn decode_prefix(prefix: &[u8], family: ProtocolFamily) -> u64 {
	match family {
		ProtocolFamily::Ietf(IetfVersion::Draft17 | IetfVersion::Draft18 | IetfVersion::Draft19) => {
			let leading_ones = prefix[0].leading_ones() as usize;
			let payload_bits = 7usize.saturating_sub(leading_ones);
			let mask = if payload_bits == 0 {
				0
			} else {
				((1u16 << payload_bits) - 1) as u8
			};
			let mut value = u64::from(prefix[0] & mask);
			for byte in &prefix[1..] {
				value = (value << 8) | u64::from(*byte);
			}
			value
		}
		ProtocolFamily::Lite
		| ProtocolFamily::SharedMoql
		| ProtocolFamily::Ietf(IetfVersion::Draft14 | IetfVersion::Draft15 | IetfVersion::Draft16)
		| ProtocolFamily::Unknown => {
			let mut value = u64::from(prefix[0] & 0x3f);
			for byte in &prefix[1..] {
				value = (value << 8) | u64::from(*byte);
			}
			value
		}
	}
}

impl ProtocolBytes {
	/// Create an enabled counter.
	pub fn enabled() -> Self {
		Self {
			inner: Arc::new(Mutex::new(Inner::default())),
		}
	}

	/// Read the current counters without blocking protocol progress.
	pub fn snapshot(&self) -> ProtocolByteSnapshot {
		let inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		ProtocolByteSnapshot {
			bytes_sent: inner.bytes_sent,
			bytes_received: inner.bytes_received,
			control_bytes_sent: inner.control_bytes_sent,
			control_bytes_received: inner.control_bytes_received,
			data_bytes_sent: inner.data_bytes_sent,
			data_bytes_received: inner.data_bytes_received,
			unclassified_bytes_sent: inner.unclassified_bytes_sent,
			unclassified_bytes_received: inner.unclassified_bytes_received,
		}
	}

	fn sent_stream(&self, classifier: &StreamClassifier, bytes: &[u8]) {
		if bytes.is_empty() {
			return;
		}
		let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let delta = classifier.record(bytes);
		inner.bytes_sent += bytes.len() as u64;
		if let Some(delta) = delta {
			apply_delta(&mut inner, delta, true);
		}
	}

	fn received_stream(&self, classifier: &StreamClassifier, bytes: &[u8]) {
		if bytes.is_empty() {
			return;
		}
		let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let delta = classifier.record(bytes);
		inner.bytes_received += bytes.len() as u64;
		if let Some(delta) = delta {
			apply_delta(&mut inner, delta, false);
		}
	}

	fn flush_sent(&self, classifier: &StreamClassifier) {
		let _inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		classifier.flush();
	}

	fn flush_received(&self, classifier: &StreamClassifier) {
		let _inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		classifier.flush();
	}

	fn sent_datagram(&self, bytes: usize) {
		let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		inner.bytes_sent += bytes as u64;
		inner.data_bytes_sent += bytes as u64;
	}

	fn received_datagram(&self, bytes: usize) {
		let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		inner.bytes_received += bytes as u64;
		inner.data_bytes_received += bytes as u64;
	}
}

fn apply_delta(inner: &mut Inner, delta: ClassificationDelta, sent: bool) {
	if sent {
		add_class(
			&mut inner.control_bytes_sent,
			&mut inner.data_bytes_sent,
			&mut inner.unclassified_bytes_sent,
			delta,
		);
	} else {
		add_class(
			&mut inner.control_bytes_received,
			&mut inner.data_bytes_received,
			&mut inner.unclassified_bytes_received,
			delta,
		);
	}
}

fn add_class(control: &mut u64, data: &mut u64, unclassified: &mut u64, delta: ClassificationDelta) {
	if delta.reclassify_unclassified > 0 {
		*unclassified -= delta.reclassify_unclassified as u64;
	}
	match delta.class {
		ByteClass::Control => *control += delta.bytes as u64,
		ByteClass::Data => *data += delta.bytes as u64,
		ByteClass::Unclassified => *unclassified += delta.bytes as u64,
	}
}

/// A transport session whose stream/datagram payloads are counted by [`ProtocolBytes`].
/// With `bytes == None` it remains a type-stable delegating adapter and allocates no
/// counter or classifier state; callers that need a fully unwrapped default path should
/// avoid constructing this internal adapter.
#[doc(hidden)]
#[derive(Clone)]
pub struct MeteredSession<S> {
	inner: S,
	bytes: Option<ProtocolBytes>,
	family_state: Option<Arc<Mutex<ProtocolFamily>>>,
}

impl<S> MeteredSession<S> {
	pub(crate) fn new(inner: S, bytes: Option<ProtocolBytes>) -> Self
	where
		S: TransportSession,
	{
		let family_state = bytes
			.as_ref()
			.map(|_| Arc::new(Mutex::new(ProtocolFamily::from_protocol(inner.protocol()))));
		Self {
			inner,
			bytes,
			family_state,
		}
	}

	/// Publish the version selected by the SETUP negotiation to classifiers created
	/// from this session. A session with metering disabled has no state to update.
	pub(crate) fn set_version(&self, version: crate::Version) {
		if let Some(family) = &self.family_state {
			*family.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = ProtocolFamily::from_version(version);
		}
	}
}

#[doc(hidden)]
#[derive(Clone)]
pub struct MeteredSendStream<S> {
	inner: S,
	bytes: Option<ProtocolBytes>,
	classifier: Option<Arc<StreamClassifier>>,
}

#[doc(hidden)]
#[derive(Clone)]
pub struct MeteredRecvStream<S> {
	inner: S,
	bytes: Option<ProtocolBytes>,
	classifier: Option<Arc<StreamClassifier>>,
}

impl<S: SendStream> SendStream for MeteredSendStream<S> {
	type Error = S::Error;

	fn write(
		&mut self,
		buf: &[u8],
	) -> impl std::future::Future<Output = Result<usize, Self::Error>> + web_transport_trait::MaybeSend {
		let bytes = self.bytes.clone();
		let classifier = self.classifier.clone();
		async move {
			let written = match self.inner.write(buf).await {
				Ok(written) => written,
				Err(err) => {
					if let (Some(bytes), Some(classifier)) = (&bytes, &classifier) {
						bytes.flush_sent(classifier);
					}
					return Err(err);
				}
			};
			if let (Some(bytes), Some(classifier)) = (&bytes, &classifier) {
				let accepted = written.min(buf.len());
				bytes.sent_stream(classifier, &buf[..accepted]);
			}
			Ok(written)
		}
	}

	fn set_priority(&mut self, order: u8) {
		self.inner.set_priority(order);
	}

	fn finish(&mut self) -> Result<(), Self::Error> {
		let result = self.inner.finish();
		if let (Some(bytes), Some(classifier)) = (&self.bytes, &self.classifier) {
			bytes.flush_sent(classifier);
		}
		result
	}

	fn reset(&mut self, code: u32) {
		self.inner.reset(code);
		if let (Some(bytes), Some(classifier)) = (&self.bytes, &self.classifier) {
			bytes.flush_sent(classifier);
		}
	}

	fn closed(
		&mut self,
	) -> impl std::future::Future<Output = Result<(), Self::Error>> + web_transport_trait::MaybeSend {
		let bytes = self.bytes.clone();
		let classifier = self.classifier.clone();
		async move {
			let result = self.inner.closed().await;
			if let (Some(bytes), Some(classifier)) = (&bytes, &classifier) {
				bytes.flush_sent(classifier);
			}
			result
		}
	}
}

impl<S: RecvStream> RecvStream for MeteredRecvStream<S> {
	type Error = S::Error;

	fn read(
		&mut self,
		dst: &mut [u8],
	) -> impl std::future::Future<Output = Result<Option<usize>, Self::Error>> + web_transport_trait::MaybeSend {
		let bytes = self.bytes.clone();
		let classifier = self.classifier.clone();
		async move {
			match self.inner.read(dst).await {
				Ok(Some(read)) => {
					if let (Some(bytes), Some(classifier)) = (&bytes, &classifier) {
						let accepted = read.min(dst.len());
						bytes.received_stream(classifier, &dst[..accepted]);
					}

					Ok(Some(read))
				}
				Ok(None) => {
					if let (Some(bytes), Some(classifier)) = (&bytes, &classifier) {
						bytes.flush_received(classifier);
					}
					Ok(None)
				}
				Err(err) => {
					if let (Some(bytes), Some(classifier)) = (&bytes, &classifier) {
						bytes.flush_received(classifier);
					}
					Err(err)
				}
			}
		}
	}

	fn stop(&mut self, code: u32) {
		self.inner.stop(code);
		if let (Some(bytes), Some(classifier)) = (&self.bytes, &self.classifier) {
			bytes.flush_received(classifier);
		}
	}

	fn closed(
		&mut self,
	) -> impl std::future::Future<Output = Result<(), Self::Error>> + web_transport_trait::MaybeSend {
		let bytes = self.bytes.clone();
		let classifier = self.classifier.clone();
		async move {
			let result = self.inner.closed().await;
			if let (Some(bytes), Some(classifier)) = (&bytes, &classifier) {
				bytes.flush_received(classifier);
			}
			result
		}
	}
}

impl<S: TransportSession> TransportSession for MeteredSession<S> {
	type SendStream = MeteredSendStream<S::SendStream>;
	type RecvStream = MeteredRecvStream<S::RecvStream>;
	type Error = S::Error;

	async fn accept_uni(&self) -> Result<Self::RecvStream, Self::Error> {
		Ok(MeteredRecvStream {
			inner: self.inner.accept_uni().await?,
			bytes: self.bytes.clone(),
			classifier: self.classifier(StreamKind::Uni),
		})
	}

	async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
		let (send, recv) = self.inner.accept_bi().await?;
		Ok((
			self.wrap_send(send, StreamKind::Bi),
			self.wrap_recv(recv, StreamKind::Bi),
		))
	}

	async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
		let (send, recv) = self.inner.open_bi().await?;
		Ok((
			self.wrap_send(send, StreamKind::Bi),
			self.wrap_recv(recv, StreamKind::Bi),
		))
	}

	async fn open_uni(&self) -> Result<Self::SendStream, Self::Error> {
		Ok(self.wrap_send(self.inner.open_uni().await?, StreamKind::Uni))
	}

	fn send_datagram(&self, payload: Bytes) -> Result<(), Self::Error> {
		let size = payload.len();
		let result = self.inner.send_datagram(payload);
		if result.is_ok()
			&& let Some(bytes) = &self.bytes
		{
			bytes.sent_datagram(size);
		}
		result
	}

	async fn recv_datagram(&self) -> Result<Bytes, Self::Error> {
		let payload = self.inner.recv_datagram().await?;
		if let Some(bytes) = &self.bytes {
			bytes.received_datagram(payload.len());
		}
		Ok(payload)
	}

	fn max_datagram_size(&self) -> usize {
		self.inner.max_datagram_size()
	}

	fn protocol(&self) -> Option<&str> {
		self.inner.protocol()
	}

	fn close(&self, code: u32, reason: &str) {
		self.inner.close(code, reason);
	}

	async fn closed(&self) -> Self::Error {
		self.inner.closed().await
	}

	fn stats(&self) -> impl Stats {
		self.inner.stats()
	}
}

impl<S> MeteredSession<S> {
	fn wrap_send(&self, inner: S::SendStream, kind: StreamKind) -> MeteredSendStream<S::SendStream>
	where
		S: TransportSession,
	{
		MeteredSendStream {
			inner,
			bytes: self.bytes.clone(),
			classifier: self.classifier(kind),
		}
	}

	fn wrap_recv(&self, inner: S::RecvStream, kind: StreamKind) -> MeteredRecvStream<S::RecvStream>
	where
		S: TransportSession,
	{
		MeteredRecvStream {
			inner,
			bytes: self.bytes.clone(),
			classifier: self.classifier(kind),
		}
	}

	fn classifier(&self, kind: StreamKind) -> Option<Arc<StreamClassifier>>
	where
		S: TransportSession,
	{
		self.family_state
			.as_ref()
			.map(|family| Arc::new(StreamClassifier::with_family(kind, family.clone())))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::lite::test_transport::{Log, SinkSend};

	#[test]
	fn snapshots_are_shared_and_monotonic() {
		let bytes = ProtocolBytes::enabled();
		let clone = bytes.clone();
		let control = StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Lite);
		bytes.sent_stream(&control, &[2; 7]);
		clone.received_datagram(11);
		let snapshot = bytes.snapshot();
		assert_eq!(snapshot.bytes_sent, 7);
		assert_eq!(snapshot.bytes_received, 11);
		assert_eq!(snapshot.control_bytes_sent, 7);
		assert_eq!(snapshot.data_bytes_received, 11);
	}

	#[test]
	fn lite_stream_prefixes_classify_control_and_data() {
		let control = StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Lite);
		assert_eq!(
			control.record(&[2, 9]),
			Some(ClassificationDelta {
				class: ByteClass::Control,
				bytes: 2,
				reclassify_unclassified: 0,
			}),
		);

		let data = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Lite);
		assert_eq!(
			data.record(&[0, 9]),
			Some(ClassificationDelta {
				class: ByteClass::Data,
				bytes: 2,
				reclassify_unclassified: 0,
			}),
		);

		let setup = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Lite);
		assert_eq!(
			setup.record(&[1, 9]),
			Some(ClassificationDelta {
				class: ByteClass::Control,
				bytes: 2,
				reclassify_unclassified: 0,
			}),
		);
	}

	#[test]
	fn split_varint_prefix_is_classified_when_completed() {
		let stream = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Lite);
		assert_eq!(
			stream.record(&[0x40]),
			Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: 1,
				reclassify_unclassified: 0,
			}),
		);
		assert_eq!(
			stream.record(&[0, 9]),
			Some(ClassificationDelta {
				class: ByteClass::Data,
				bytes: 3,
				reclassify_unclassified: 1,
			}),
		);

		let bytes = ProtocolBytes::enabled();
		let stream = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Lite);
		bytes.sent_stream(&stream, &[0x40]);
		assert_eq!(bytes.snapshot().unclassified_bytes_sent, 1);
		bytes.sent_stream(&stream, &[0, 9]);
		let snapshot = bytes.snapshot();
		assert_eq!(snapshot.bytes_sent, 3);
		assert_eq!(snapshot.data_bytes_sent, 3);
		assert_eq!(snapshot.unclassified_bytes_sent, 0);
	}

	#[test]
	fn ietf_alpns_select_the_expected_classifier_family() {
		assert_eq!(ProtocolFamily::from_protocol(Some("moql")), ProtocolFamily::SharedMoql);
		assert_eq!(
			ProtocolFamily::from_protocol(Some(crate::ALPN_LITE_05)),
			ProtocolFamily::Lite
		);
		assert_eq!(
			ProtocolFamily::from_protocol(Some(crate::ALPN_14)),
			ProtocolFamily::Ietf(IetfVersion::Draft14)
		);
		assert_eq!(
			ProtocolFamily::from_protocol(Some(crate::ALPN_19)),
			ProtocolFamily::Ietf(IetfVersion::Draft19)
		);
		assert_eq!(ProtocolFamily::from_protocol(Some("not-moq")), ProtocolFamily::Unknown);
		assert_eq!(
			ProtocolFamily::from_protocol(Some("moq-lite-future")),
			ProtocolFamily::Unknown
		);
		assert_eq!(ProtocolFamily::from_protocol(None), ProtocolFamily::Unknown);
	}

	#[test]
	fn shared_moql_classifies_both_legacy_setup_families() {
		let ietf_setup = StreamClassifier::new(StreamKind::Bi, ProtocolFamily::SharedMoql);
		assert_eq!(
			ietf_setup.record(&[0x20, 0x00]),
			Some(ClassificationDelta {
				class: ByteClass::Control,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let lite_group = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::SharedMoql);
		assert_eq!(
			lite_group.record(&[0, 9]),
			Some(ClassificationDelta {
				class: ByteClass::Data,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let ietf_group = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::SharedMoql);
		assert_eq!(
			ietf_group.record(&[0x10, 0x01]),
			Some(ClassificationDelta {
				class: ByteClass::Data,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let ietf_fetch = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::SharedMoql);
		assert_eq!(
			ietf_fetch.record(&[0x05, 0x00]),
			Some(ClassificationDelta {
				class: ByteClass::Control,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);
	}

	#[test]
	fn negotiated_family_is_used_for_new_streams() {
		let family = Arc::new(Mutex::new(ProtocolFamily::SharedMoql));
		let bytes = ProtocolBytes::enabled();

		let shared = StreamClassifier::with_family(StreamKind::Uni, family.clone());
		bytes.sent_stream(&shared, &[0x10, 0x01]);
		assert_eq!(bytes.snapshot().data_bytes_sent, 2);

		*family.lock().unwrap() = ProtocolFamily::from_version(crate::Version::Lite(crate::lite::Version::Lite01));
		let lite = StreamClassifier::with_family(StreamKind::Uni, family.clone());
		bytes.sent_stream(&lite, &[0x10, 0x01]);
		assert_eq!(bytes.snapshot().data_bytes_sent, 2);
		assert_eq!(bytes.snapshot().unclassified_bytes_sent, 2);

		*family.lock().unwrap() = ProtocolFamily::from_version(crate::Version::Ietf(crate::ietf::Version::Draft14));
		let ietf = StreamClassifier::with_family(StreamKind::Uni, family);
		bytes.sent_stream(&ietf, &[0x10, 0x01]);
		assert_eq!(bytes.snapshot().data_bytes_sent, 4);
		assert_eq!(bytes.snapshot().unclassified_bytes_sent, 2);
	}

	#[test]
	fn negotiated_family_update_reclassifies_split_prefix_without_double_counting() {
		let family = Arc::new(Mutex::new(ProtocolFamily::SharedMoql));
		let bytes = ProtocolBytes::enabled();
		let stream = StreamClassifier::with_family(StreamKind::Uni, family.clone());

		bytes.received_stream(&stream, &[0x40]);
		assert_eq!(bytes.snapshot().unclassified_bytes_received, 1);

		*family.lock().unwrap() = ProtocolFamily::from_version(crate::Version::Lite(crate::lite::Version::Lite01));
		bytes.received_stream(&stream, &[0x10]);
		let snapshot = bytes.snapshot();
		assert_eq!(snapshot.bytes_received, 2);
		assert_eq!(snapshot.data_bytes_received, 0);
		assert_eq!(snapshot.unclassified_bytes_received, 2);
	}

	#[test]
	fn ietf_stream_types_classify_control_data_and_unknown_values() {
		let bidi = StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Ietf(IetfVersion::Draft19));
		assert_eq!(
			bidi.record(&[0x03, 0x00, 0x00]),
			Some(ClassificationDelta {
				class: ByteClass::Control,
				bytes: 3,
				reclassify_unclassified: 0,
			})
		);

		let unknown_bidi = StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Ietf(IetfVersion::Draft19));
		assert_eq!(
			unknown_bidi.record(&[0x1b, 0x00]),
			Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let legacy_namespace = StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Ietf(IetfVersion::Draft17));
		assert_eq!(
			legacy_namespace.record(&[0x40, 0x50]),
			Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let modern_namespace = StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Ietf(IetfVersion::Draft18));
		assert_eq!(
			modern_namespace.record(&[0x50, 0x00]),
			Some(ClassificationDelta {
				class: ByteClass::Control,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let legacy_setup = StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Ietf(IetfVersion::Draft16));
		assert_eq!(
			legacy_setup.record(&[0x20, 0x00]),
			Some(ClassificationDelta {
				class: ByteClass::Control,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let legacy_modern_setup = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Ietf(IetfVersion::Draft16));
		assert_eq!(
			legacy_modern_setup.record(&[0x6f, 0x00]),
			Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let setup = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Ietf(IetfVersion::Draft19));
		assert_eq!(
			setup.record(&[0xaf, 0x00, 0x00]),
			Some(ClassificationDelta {
				class: ByteClass::Control,
				bytes: 3,
				reclassify_unclassified: 0,
			})
		);

		let fetch = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Ietf(IetfVersion::Draft19));
		assert_eq!(
			fetch.record(&[0x05, 0x00]),
			Some(ClassificationDelta {
				class: ByteClass::Control,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let group = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Ietf(IetfVersion::Draft19));
		assert_eq!(
			group.record(&[0x50, 0x01, 0x01]),
			Some(ClassificationDelta {
				class: ByteClass::Data,
				bytes: 3,
				reclassify_unclassified: 0,
			})
		);

		let draft14_no_priority = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Ietf(IetfVersion::Draft14));
		assert_eq!(
			draft14_no_priority.record(&[0x30, 0x01]),
			Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let unknown_uni = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Ietf(IetfVersion::Draft19));
		assert_eq!(
			unknown_uni.record(&[0x22, 0x00]),
			Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: 2,
				reclassify_unclassified: 0,
			})
		);

		let invalid_draft17_varint = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Ietf(IetfVersion::Draft17));
		assert_eq!(
			invalid_draft17_varint.record(&[0xfc, 0, 0, 0, 0, 0, 0]),
			Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: 7,
				reclassify_unclassified: 0,
			})
		);
	}

	#[test]
	fn ietf_setup_varint_is_reclassified_after_a_split_read() {
		let bytes = ProtocolBytes::enabled();
		let stream = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Ietf(IetfVersion::Draft19));
		bytes.received_stream(&stream, &[0xaf]);
		assert_eq!(bytes.snapshot().unclassified_bytes_received, 1);

		bytes.received_stream(&stream, &[0x00, 0x00]);
		let snapshot = bytes.snapshot();
		assert_eq!(snapshot.bytes_received, 3);
		assert_eq!(snapshot.control_bytes_received, 3);
		assert_eq!(snapshot.data_bytes_received, 0);
		assert_eq!(snapshot.unclassified_bytes_received, 0);
	}

	#[test]
	fn unknown_stream_type_is_not_control() {
		let stream = StreamClassifier::new(StreamKind::Uni, ProtocolFamily::Lite);
		assert_eq!(
			stream.record(&[2, 9]),
			Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: 2,
				reclassify_unclassified: 0,
			}),
		);

		let unknown_protocol = StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Unknown);
		assert_eq!(
			unknown_protocol.record(&[0, 9]),
			Some(ClassificationDelta {
				class: ByteClass::Unclassified,
				bytes: 2,
				reclassify_unclassified: 0,
			}),
		);
	}

	#[test]
	fn successful_stream_write_updates_classified_counters() {
		let bytes = ProtocolBytes::enabled();
		let log = Log::default();
		let mut stream = MeteredSendStream {
			inner: SinkSend::new(log),
			bytes: Some(bytes.clone()),
			classifier: Some(Arc::new(StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Lite))),
		};
		let written = match futures::executor::block_on(stream.write(&[2, 3, 4])) {
			Ok(written) => written,
			Err(error) => panic!("write failed: {error}"),
		};
		assert_eq!(written, 3);
		assert_eq!(bytes.snapshot().control_bytes_sent, 3);
		assert_eq!(bytes.snapshot().unclassified_bytes_sent, 0);
	}

	#[derive(Debug, Clone, Copy, PartialEq, Eq)]
	enum TestError {
		Io,
	}

	impl std::fmt::Display for TestError {
		fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			write!(f, "test transport error")
		}
	}

	impl std::error::Error for TestError {}

	impl web_transport_trait::Error for TestError {
		fn session_error(&self) -> Option<(u32, String)> {
			Some((0, "test transport error".to_string()))
		}
	}

	#[derive(Clone, Copy)]
	enum WriteBehavior {
		Partial(usize),
		Fail,
	}

	struct ScriptedSend {
		behavior: WriteBehavior,
	}

	impl web_transport_trait::SendStream for ScriptedSend {
		type Error = TestError;

		async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
			match self.behavior {
				WriteBehavior::Partial(size) => Ok(size.min(buf.len())),
				WriteBehavior::Fail => Err(TestError::Io),
			}
		}

		fn set_priority(&mut self, _order: u8) {}

		fn finish(&mut self) -> Result<(), Self::Error> {
			Ok(())
		}

		fn reset(&mut self, _code: u32) {}

		async fn closed(&mut self) -> Result<(), Self::Error> {
			Ok(())
		}
	}

	enum ReadBehavior {
		Reported { data: Vec<u8>, reported: usize },
		Fail,
		Eof,
	}

	struct ScriptedRecv {
		behavior: ReadBehavior,
	}

	impl web_transport_trait::RecvStream for ScriptedRecv {
		type Error = TestError;

		async fn read(&mut self, dst: &mut [u8]) -> Result<Option<usize>, Self::Error> {
			let behavior = std::mem::replace(&mut self.behavior, ReadBehavior::Eof);
			match behavior {
				ReadBehavior::Reported { data, reported } => {
					let copied = data.len().min(reported).min(dst.len());
					dst[..copied].copy_from_slice(&data[..copied]);
					Ok(Some(reported))
				}
				ReadBehavior::Fail => Err(TestError::Io),
				ReadBehavior::Eof => Ok(None),
			}
		}

		fn stop(&mut self, _code: u32) {}

		async fn closed(&mut self) -> Result<(), Self::Error> {
			Ok(())
		}
	}

	#[test]
	fn partial_and_failed_writes_count_only_accepted_bytes() {
		let bytes = ProtocolBytes::enabled();
		let mut partial = MeteredSendStream {
			inner: ScriptedSend {
				behavior: WriteBehavior::Partial(1),
			},
			bytes: Some(bytes.clone()),
			classifier: Some(Arc::new(StreamClassifier::new(
				StreamKind::Uni,
				ProtocolFamily::Ietf(IetfVersion::Draft19),
			))),
		};
		assert_eq!(futures::executor::block_on(partial.write(&[0xaf, 0, 0])).unwrap(), 1);
		assert_eq!(bytes.snapshot().bytes_sent, 1);
		assert_eq!(bytes.snapshot().unclassified_bytes_sent, 1);
		partial.finish().unwrap();

		let mut failed = MeteredSendStream {
			inner: ScriptedSend {
				behavior: WriteBehavior::Fail,
			},
			bytes: Some(bytes.clone()),
			classifier: Some(Arc::new(StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Lite))),
		};
		assert!(futures::executor::block_on(failed.write(&[2, 3])).is_err());
		let snapshot = bytes.snapshot();
		assert_eq!(snapshot.bytes_sent, 1);
		assert_eq!(snapshot.control_bytes_sent, 0);
		assert_eq!(snapshot.unclassified_bytes_sent, 1);
	}

	#[test]
	fn partial_reads_clamp_accounting_and_failed_reads_do_not_add_bytes() {
		let bytes = ProtocolBytes::enabled();
		let mut partial = MeteredRecvStream {
			inner: ScriptedRecv {
				behavior: ReadBehavior::Reported {
					data: vec![0xaf],
					reported: 99,
				},
			},
			bytes: Some(bytes.clone()),
			classifier: Some(Arc::new(StreamClassifier::new(
				StreamKind::Uni,
				ProtocolFamily::Ietf(IetfVersion::Draft19),
			))),
		};
		let mut dst = [0; 1];
		assert_eq!(futures::executor::block_on(partial.read(&mut dst)).unwrap(), Some(99));
		partial.stop(7);
		let snapshot = bytes.snapshot();
		assert_eq!(snapshot.bytes_received, 1);
		assert_eq!(snapshot.unclassified_bytes_received, 1);

		let mut failed = MeteredRecvStream {
			inner: ScriptedRecv {
				behavior: ReadBehavior::Fail,
			},
			bytes: Some(bytes.clone()),
			classifier: Some(Arc::new(StreamClassifier::new(StreamKind::Bi, ProtocolFamily::Lite))),
		};
		let mut dst = [0; 2];
		assert!(futures::executor::block_on(failed.read(&mut dst)).is_err());
		let snapshot = bytes.snapshot();
		assert_eq!(snapshot.bytes_received, 1);
		assert_eq!(snapshot.unclassified_bytes_received, 1);
	}

	struct TestStats;

	impl Stats for TestStats {
		fn estimated_send_rate(&self) -> Option<u64> {
			None
		}
	}

	#[derive(Clone)]
	struct DatagramSession {
		protocol: Option<&'static str>,
		send_ok: bool,
		received: Result<Bytes, TestError>,
	}

	impl DatagramSession {
		fn new(send_ok: bool, received: Result<Bytes, TestError>) -> Self {
			Self {
				protocol: Some(crate::ALPN_LITE_05),
				send_ok,
				received,
			}
		}
	}

	impl web_transport_trait::Session for DatagramSession {
		type SendStream = ScriptedSend;
		type RecvStream = ScriptedRecv;
		type Error = TestError;

		async fn accept_uni(&self) -> Result<Self::RecvStream, Self::Error> {
			std::future::pending().await
		}

		async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
			std::future::pending().await
		}

		async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
			std::future::pending().await
		}

		async fn open_uni(&self) -> Result<Self::SendStream, Self::Error> {
			std::future::pending().await
		}

		fn send_datagram(&self, _payload: Bytes) -> Result<(), Self::Error> {
			if self.send_ok { Ok(()) } else { Err(TestError::Io) }
		}

		async fn recv_datagram(&self) -> Result<Bytes, Self::Error> {
			self.received.clone()
		}

		fn max_datagram_size(&self) -> usize {
			1200
		}

		fn protocol(&self) -> Option<&str> {
			self.protocol
		}

		fn close(&self, _code: u32, _reason: &str) {}

		async fn closed(&self) -> Self::Error {
			std::future::pending().await
		}

		fn stats(&self) -> impl Stats {
			TestStats
		}
	}

	#[test]
	fn metered_session_publishes_negotiated_version_to_stream_classifiers() {
		let mut transport = DatagramSession::new(true, Ok(Bytes::from_static(b"recv")));
		transport.protocol = Some(crate::ALPN_LITE);
		let bytes = ProtocolBytes::enabled();
		let session = MeteredSession::new(transport, Some(bytes.clone()));

		let split = session.classifier(StreamKind::Uni).unwrap();
		bytes.received_stream(&split, &[0x40]);
		session.set_version(crate::Version::Lite(crate::lite::Version::Lite01));
		bytes.received_stream(&split, &[0x10]);
		let snapshot = bytes.snapshot();
		assert_eq!(snapshot.bytes_received, 2);
		assert_eq!(snapshot.data_bytes_received, 0);
		assert_eq!(snapshot.unclassified_bytes_received, 2);

		session.set_version(crate::Version::Ietf(crate::ietf::Version::Draft14));
		let ietf = session.classifier(StreamKind::Uni).unwrap();
		bytes.sent_stream(&ietf, &[0x10, 0x01]);
		let snapshot = bytes.snapshot();
		assert_eq!(snapshot.data_bytes_sent, 2);
		assert_eq!(snapshot.unclassified_bytes_sent, 0);
	}

	#[test]
	fn datagrams_count_only_successful_send_and_receive() {
		let bytes = ProtocolBytes::enabled();
		let session = MeteredSession::new(
			DatagramSession::new(true, Ok(Bytes::from_static(b"recv"))),
			Some(bytes.clone()),
		);
		session.send_datagram(Bytes::from_static(b"send")).unwrap();
		assert_eq!(
			futures::executor::block_on(session.recv_datagram()).unwrap(),
			Bytes::from_static(b"recv")
		);

		let failed = MeteredSession::new(DatagramSession::new(false, Err(TestError::Io)), Some(bytes.clone()));
		assert!(failed.send_datagram(Bytes::from_static(b"drop")).is_err());
		assert!(futures::executor::block_on(failed.recv_datagram()).is_err());

		let snapshot = bytes.snapshot();
		assert_eq!(snapshot.bytes_sent, 4);
		assert_eq!(snapshot.bytes_received, 4);
		assert_eq!(snapshot.data_bytes_sent, 4);
		assert_eq!(snapshot.data_bytes_received, 4);
		assert_eq!(snapshot.unclassified_bytes_sent, 0);
		assert_eq!(snapshot.unclassified_bytes_received, 0);
	}
}
