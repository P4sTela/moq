use crate::metrics::Metrics;
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Serialize)]
pub struct StatsReport {
    pub connections: ConnectionStats,
    pub latency: LatencyStats,
    pub throughput: ThroughputStats,
    pub duration: DurationStats,
}

#[derive(Debug, Serialize)]
pub struct ConnectionStats {
    pub total: usize,
    pub successful: usize,
    pub failed: usize,
    #[serde(rename = "avgTimeMs")]
    pub avg_time_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct LatencyStats {
    pub count: usize,
    #[serde(rename = "avgMs")]
    pub avg_ms: f64,
    #[serde(rename = "p50Ms")]
    pub p50_ms: f64,
    #[serde(rename = "p95Ms")]
    pub p95_ms: f64,
    #[serde(rename = "p99Ms")]
    pub p99_ms: f64,
    #[serde(rename = "minMs")]
    pub min_ms: f64,
    #[serde(rename = "maxMs")]
    pub max_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct ThroughputStats {
    #[serde(rename = "totalPublished")]
    pub total_published: u64,
    #[serde(rename = "totalReceived")]
    pub total_received: u64,
    #[serde(rename = "expectedReceived")]
    pub expected_received: u64,
    #[serde(rename = "deliveryRate")]
    pub delivery_rate: f64,
    #[serde(rename = "publishRateActual")]
    pub publish_rate_actual: f64,
}

#[derive(Debug, Serialize)]
pub struct DurationStats {
    #[serde(rename = "elapsedSeconds")]
    pub elapsed_seconds: f64,
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

pub fn compute_stats(metrics: &Arc<Metrics>) -> StatsReport {
    let elapsed_seconds = metrics.elapsed_seconds();
    let connected_clients = metrics.connected_clients();

    // Connection stats
    let conn_records = metrics.take_connection_records();
    let total = conn_records.len();
    let successful: Vec<_> = conn_records.iter().filter(|r| r.success).collect();
    let avg_conn_time = if successful.is_empty() {
        0.0
    } else {
        successful.iter().map(|r| r.time_ms).sum::<f64>() / successful.len() as f64
    };

    // Latency stats
    let mut samples = metrics.take_latency_samples();
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let count = samples.len();
    let latency = if count > 0 {
        LatencyStats {
            count,
            avg_ms: round2(samples.iter().sum::<f64>() / count as f64),
            p50_ms: round2(percentile(&samples, 0.5)),
            p95_ms: round2(percentile(&samples, 0.95)),
            p99_ms: round2(percentile(&samples, 0.99)),
            min_ms: round2(samples[0]),
            max_ms: round2(samples[count - 1]),
        }
    } else {
        LatencyStats {
            count: 0,
            avg_ms: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
            min_ms: 0.0,
            max_ms: 0.0,
        }
    };

    // Throughput stats
    let pub_counts = metrics.take_publish_counts();
    let recv_counts = metrics.take_receive_counts();
    let total_published: u64 = pub_counts.values().sum();
    let total_received: u64 = recv_counts.values().sum();
    let expected_received = if connected_clients > 1 {
        total_published * (connected_clients as u64 - 1)
    } else {
        0
    };

    StatsReport {
        connections: ConnectionStats {
            total,
            successful: successful.len(),
            failed: total - successful.len(),
            avg_time_ms: round2(avg_conn_time),
        },
        latency,
        throughput: ThroughputStats {
            total_published,
            total_received,
            expected_received,
            delivery_rate: if expected_received > 0 {
                round2((total_received as f64 / expected_received as f64) * 100.0)
            } else {
                0.0
            },
            publish_rate_actual: if elapsed_seconds > 0.0 {
                round2(total_published as f64 / elapsed_seconds)
            } else {
                0.0
            },
        },
        duration: DurationStats {
            elapsed_seconds: round2(elapsed_seconds),
        },
    }
}

pub fn print_report(stats: &StatsReport) {
    println!("\n=== Load Test Results ===");
    println!();
    println!("Connections:");
    println!("  Total:      {}", stats.connections.total);
    println!("  Successful: {}", stats.connections.successful);
    println!("  Failed:     {}", stats.connections.failed);
    println!("  Avg time:   {}ms", stats.connections.avg_time_ms);
    println!();
    println!("Latency:");
    println!("  Samples:    {}", stats.latency.count);
    println!("  Average:    {}ms", stats.latency.avg_ms);
    println!("  P50:        {}ms", stats.latency.p50_ms);
    println!("  P95:        {}ms", stats.latency.p95_ms);
    println!("  P99:        {}ms", stats.latency.p99_ms);
    println!("  Min:        {}ms", stats.latency.min_ms);
    println!("  Max:        {}ms", stats.latency.max_ms);
    println!();
    println!("Throughput:");
    println!("  Published:  {} messages", stats.throughput.total_published);
    println!("  Received:   {} messages", stats.throughput.total_received);
    println!("  Expected:   {} messages", stats.throughput.expected_received);
    println!("  Delivery:   {}%", stats.throughput.delivery_rate);
    println!(
        "  Pub rate:   {} msg/s (all clients combined)",
        stats.throughput.publish_rate_actual
    );
    println!();
    println!("Duration:");
    println!("  Elapsed:    {}s", stats.duration.elapsed_seconds);
}

#[derive(Serialize)]
struct ResultsFile {
    timestamp: String,
    config: ResultsConfig,
    stats: StatsReport,
}

#[derive(Serialize)]
pub struct ResultsConfig {
    #[serde(rename = "relayUrls")]
    pub relay_urls: Vec<String>,
    #[serde(rename = "numClients")]
    pub num_clients: usize,
    #[serde(rename = "publishRateHz")]
    pub publish_rate_hz: u32,
    #[serde(rename = "payloadBytes")]
    pub payload_bytes: usize,
    #[serde(rename = "durationSeconds")]
    pub duration_seconds: u64,
    #[serde(rename = "staggerDelayMs")]
    pub stagger_delay_ms: u64,
    pub transport: String,
    #[serde(rename = "numWorkers")]
    pub num_workers: usize,
    #[serde(rename = "numRelays")]
    pub num_relays: usize,
}

pub fn save_results(
    dir: &str,
    config: ResultsConfig,
    stats: StatsReport,
) -> anyhow::Result<()> {
    let results = ResultsFile {
        timestamp: chrono::Utc::now().to_rfc3339(),
        config,
        stats,
    };
    let json = serde_json::to_string_pretty(&results)?;
    let path = format!("{}/results.json", dir);
    std::fs::write(&path, json)?;
    println!("\nResults saved to: {}", dir);
    Ok(())
}
