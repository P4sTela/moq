use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::{AuthConfig, CachePolicyConfig, ClusterConfig, WebConfig};

#[derive(Parser, Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
	/// The QUIC/TLS configuration for the server.
	#[command(flatten)]
	pub server: moq_native::ServerConfig,

	/// The QUIC/TLS configuration for the client. (clustering only)
	#[command(flatten)]
	#[serde(default)]
	pub client: moq_native::ClientConfig,

	/// Log configuration.
	#[command(flatten)]
	#[serde(default)]
	pub log: moq_native::Log,

	/// Cluster configuration.
	#[command(flatten)]
	#[serde(default)]
	pub cluster: ClusterConfig,

	/// Authentication configuration.
	#[command(flatten)]
	#[serde(default)]
	pub auth: AuthConfig,

	/// Optionally run a TCP HTTP/WebSocket server.
	#[command(flatten)]
	#[serde(default)]
	pub web: WebConfig,

	/// Cache policy configuration.
	#[command(flatten)]
	#[serde(default)]
	pub cache_policy: CachePolicyConfig,

	/// If provided, load the configuration from this file.
	#[serde(default)]
	pub file: Option<String>,
}

impl Config {
	pub fn load() -> anyhow::Result<Self> {
		// Parse just the CLI arguments initially.
		let mut config = Config::parse();

		// If a file is provided, load it and merge the CLI arguments.
		if let Some(file) = config.file {
			config = toml::from_str(&std::fs::read_to_string(file)?)?;
			config.update_from(std::env::args());
		}

		config.log.init();
		tracing::trace!(?config, "final config");

		Ok(config)
	}
}

#[cfg(test)]
mod tests {
	use super::Config;

	#[test]
	fn deserializes_role_specific_uni_stream_credits() {
		let config: Config = toml::from_str(
			r#"
				[server]
				listen = "127.0.0.1:4443"
				max_concurrent_uni_streams = 100

				[client]
				max_concurrent_uni_streams = 1024
			"#,
		)
		.unwrap_or_else(|err| panic!("failed to deserialize relay config: {err}"));

		assert_eq!(config.server.max_concurrent_uni_streams, Some(100));
		assert_eq!(config.client.max_concurrent_uni_streams, Some(1024));
	}
}
