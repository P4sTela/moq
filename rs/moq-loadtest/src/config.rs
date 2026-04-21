use anyhow::{bail, Context};
use clap::Parser;
use url::Url;

fn parse_workers(s: &str) -> Result<WorkerCount, String> {
    match s {
        "auto" => Ok(WorkerCount::Auto),
        _ => s
            .parse::<usize>()
            .map(WorkerCount::Fixed)
            .map_err(|e| format!("invalid worker count '{}': {}", s, e)),
    }
}

#[derive(Clone, Debug)]
pub enum WorkerCount {
    Fixed(usize),
    Auto,
}

impl WorkerCount {
    /// Resolve to a concrete number of workers
    pub fn resolve(&self, num_clients: usize) -> usize {
        match self {
            WorkerCount::Fixed(n) => *n,
            WorkerCount::Auto => {
                let cpus = num_cpus::get();
                let by_clients = (num_clients + 9) / 10; // ceil(clients / 10)
                by_clients.min(cpus).max(1)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct RelaySpec {
    pub config: String,
    pub url: Url,
}

fn parse_relay_spec(s: &str) -> anyhow::Result<RelaySpec> {
    let eq_idx = s
        .find('=')
        .context("expected format: config.toml=http://host:port/path")?;
    let config = s[..eq_idx].to_string();
    let url = Url::parse(&s[eq_idx + 1..]).context("invalid relay URL")?;
    Ok(RelaySpec { config, url })
}

#[derive(Parser, Clone)]
#[command(name = "moq-loadtest")]
#[command(about = "Native QUIC load test tool for MoQ relay (N-to-N pub/sub)")]
pub struct Config {
    /// Relay URL for client connections (repeatable).
    /// If --start-relay is used, those URLs are used by default.
    #[arg(long = "relay")]
    pub relay_urls: Vec<Url>,

    /// Start a root relay (clients do NOT connect here).
    /// Format: config.toml=http://host:port/path
    #[arg(long = "start-root", value_parser = parse_relay_spec)]
    pub start_root: Vec<RelaySpec>,

    /// Start a leaf relay and connect clients to it (repeatable).
    /// Format: config.toml=http://host:port/path
    #[arg(long = "start-relay", value_parser = parse_relay_spec)]
    pub start_relay: Vec<RelaySpec>,

    /// Number of virtual clients
    #[arg(long = "clients", default_value = "10")]
    pub num_clients: usize,

    /// Publish rate in Hz
    #[arg(long = "rate", default_value = "10")]
    pub publish_rate_hz: u32,

    /// Payload size in bytes
    #[arg(long = "payload", default_value = "128")]
    pub payload_bytes: usize,

    /// Test duration in seconds
    #[arg(long = "duration", default_value = "30")]
    pub duration_seconds: u64,

    /// Delay between client connections in ms
    #[arg(long = "stagger", default_value = "50")]
    pub stagger_delay_ms: u64,

    /// Build relay in release mode
    #[arg(long = "release", default_value = "true")]
    pub release_build: bool,

    /// Build relay in debug mode (overrides --release)
    #[arg(long = "debug")]
    pub debug_build: bool,

    /// Number of worker processes (default: 1 = single-process mode, "auto" for automatic)
    #[arg(long = "workers", default_value = "1", value_parser = parse_workers)]
    pub workers: WorkerCount,

    /// Internal: run as a worker child process (hidden from help)
    #[arg(long = "worker-mode", hide = true)]
    pub worker_mode: bool,

    /// Directory for results output
    #[arg(long = "results-dir")]
    pub results_dir: Option<String>,

    /// The MoQ client configuration (TLS settings)
    #[command(flatten)]
    pub client: moq_native::ClientConfig,

    /// The log configuration
    #[command(flatten)]
    pub log: moq_native::Log,
}

impl Config {
    /// All relay specs to start (roots + leaves)
    pub fn all_start_relays(&self) -> Vec<&RelaySpec> {
        self.start_root.iter().chain(self.start_relay.iter()).collect()
    }

    /// Relay URLs that clients should connect to
    pub fn client_relay_urls(&self) -> anyhow::Result<Vec<Url>> {
        if !self.relay_urls.is_empty() {
            return Ok(self.relay_urls.clone());
        }

        let managed: Vec<Url> = self.start_relay.iter().map(|s| s.url.clone()).collect();
        if !managed.is_empty() {
            return Ok(managed);
        }

        if self.start_root.is_empty() && self.start_relay.is_empty() {
            return Ok(vec![Url::parse("http://localhost:4443/anon")?]);
        }

        bail!("No relay URLs for clients. Use --relay or --start-relay to specify where clients connect.")
    }

    /// Whether TLS verification is disabled
    pub fn tls_disable_verify(&self) -> bool {
        self.client.tls.disable_verify.unwrap_or(false)
    }

    /// Whether to build in release mode
    pub fn is_release(&self) -> bool {
        if self.debug_build {
            false
        } else {
            self.release_build
        }
    }
}
