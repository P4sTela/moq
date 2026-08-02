pub(crate) const DEFAULT_MAX_CONCURRENT_UNI_STREAMS: u32 = 100;

pub(crate) fn resolve_max_concurrent_uni_streams(value: Option<u32>) -> anyhow::Result<u32> {
	let value = value.unwrap_or(DEFAULT_MAX_CONCURRENT_UNI_STREAMS);
	anyhow::ensure!(value > 0, "max concurrent uni streams must be positive");
	Ok(value)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn resolves_default_and_explicit_uni_stream_credits() {
		assert_eq!(resolve_max_concurrent_uni_streams(None).unwrap(), 100);
		assert_eq!(resolve_max_concurrent_uni_streams(Some(1024)).unwrap(), 1024);
	}

	#[test]
	fn rejects_zero_uni_stream_credit() {
		assert!(resolve_max_concurrent_uni_streams(Some(0)).is_err());
	}
}

pub mod client;
mod crypto;
pub mod log;
pub mod server;

pub use client::*;
pub use log::*;
pub use server::*;

// Re-export these crates.
pub use moq_lite;
pub use rustls;
pub use web_transport_quinn;
