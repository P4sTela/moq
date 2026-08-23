//! An implementation of the IETF MoQ specification.
//!
//! Not all features are supported; just to provide compatibility with the crate API.
//!
//! You should not use this module directly; see [crate] for the high-level API.

#[macro_use]
mod parameters;
mod adapter;
mod control;
mod fetch;
mod goaway;
mod group;
mod location;
pub mod message;
mod namespace;
mod properties;
mod publish;
mod publish_namespace;
mod publisher;
mod request;
mod session;
mod subscribe;
mod subscribe_namespace;
mod subscriber;
mod track;
mod version;

use control::Control;
pub use fetch::*;
pub use goaway::*;
pub use group::*;
pub use location::*;
pub use message::Message;
pub use parameters::*;
pub use publish::*;
pub use publish_namespace::*;
use publisher::*;
pub use request::*;
pub use session::*;
pub use subscribe::*;
pub use subscribe_namespace::*; // includes PublishBlocked
use subscriber::*;
pub use track::*;
pub use version::Version;

/// Whether an IETF bidi stream's first varint is a registered control message.
///
/// Keep this registry next to the IETF message implementations so the byte meter does
/// not classify reserved message types as control merely because they fit a numeric range.
pub(crate) fn is_control_message_type(version: Version, id: u64) -> bool {
	match id {
		// Message types present throughout the supported drafts.
		0x02 | 0x03 | 0x04 | 0x05 | 0x06 | 0x07 | 0x08 | 0x0a | 0x0b | 0x0d | 0x10 | 0x16 | 0x17 | 0x18 | 0x19
		| 0x1a | 0x1d | 0x1e | 0x1f => true,
		// PUBLISH_NAMESPACE_DONE and PUBLISH_NAMESPACE_CANCEL are legacy namespace
		// messages (v14-v16; v17+ uses the modern namespace protocol).
		0x09 | 0x0c => matches!(version, Version::Draft14 | Version::Draft15 | Version::Draft16),
		// NAMESPACE_DONE is a draft-16 follow-up on the real bidi stream.
		0x0e => matches!(version, Version::Draft16),
		// PUBLISH_BLOCKED is draft-17 only.
		0x0f => matches!(version, Version::Draft17),
		// Legacy SUBSCRIBE_NAMESPACE is used through draft-17; draft-18 renamed it.
		0x11 => matches!(
			version,
			Version::Draft14 | Version::Draft15 | Version::Draft16 | Version::Draft17
		),
		// These response types are draft-14 only.
		0x12 | 0x13 => matches!(version, Version::Draft14),
		// UNSUBSCRIBE_NAMESPACE is draft-14/15; draft-16 uses stream close.
		0x14 => matches!(version, Version::Draft14 | Version::Draft15),
		// MAX_REQUEST_ID was removed in draft-17.
		0x15 => matches!(version, Version::Draft14 | Version::Draft15 | Version::Draft16),
		// Draft-18 renumbered SUBSCRIBE_NAMESPACE and introduced SUBSCRIBE_TRACKS.
		0x50 | 0x51 => matches!(version, Version::Draft18 | Version::Draft19),
		_ => false,
	}
}

/// Whether an IETF unidirectional stream starts with a valid group header.
pub(crate) fn is_group_stream_type(version: Version, id: u64) -> bool {
	GroupFlags::decode(id, version).is_ok()
}
