use anyhow::Context;
use clap::Parser;
use std::time::Duration;

mod client;
mod config;
mod ipc;
mod metrics;
mod relay;
mod report;
mod worker;
mod worker_pool;

use client::VirtualClient;
use config::Config;
use metrics::Metrics;
use relay::RelayManager;
use worker_pool::WorkerPool;

/// Resolve the results directory, defaulting to <project_root>/results/loadtest-quic/<timestamp>_<config>
fn resolve_results_dir(config_dir: &Option<String>, config: &Config, num_relays: usize) -> String {
    if let Some(dir) = config_dir {
        return dir.clone();
    }
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let num_workers = config.workers.resolve(config.num_clients);
    let name = format!(
        "{}_{}c-{}hz-{}r-{}w",
        timestamp, config.num_clients, config.publish_rate_hz, num_relays, num_workers
    );
    let rel = format!("results/loadtest-quic/{}", name);
    // Use project root (parent of moq-src) so results go to /root/moq/results/
    match relay::find_project_root() {
        Some(root) => root.join(&rel).to_string_lossy().into_owned(),
        None => rel,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    config.log.init();

    if config.worker_mode {
        // Child worker process: IPC via stdin/stdout
        let quic_client = config.client.clone().init().context("failed to init QUIC client")?;
        return worker::run_worker(quic_client).await;
    }

    let num_workers = config.workers.resolve(config.num_clients);

    if num_workers > 1 {
        run_multi_worker(config, num_workers).await
    } else {
        run_single_process(config).await
    }
}

/// Multi-worker mode: spawn N child processes, coordinate via IPC
async fn run_multi_worker(config: Config, num_workers: usize) -> anyhow::Result<()> {
    let relay_urls = config.client_relay_urls()?;
    let is_release = config.is_release();

    println!("=== MoQ Native QUIC Load Test (Multi-Worker) ===");
    println!("  Relay URLs:    {}", relay_urls.iter().map(|u| u.as_str()).collect::<Vec<_>>().join(", "));
    println!("  Clients:       {}", config.num_clients);
    println!("  Workers:       {}", num_workers);
    println!("  Publish rate:  {} Hz", config.publish_rate_hz);
    println!("  Payload:       {} bytes", config.payload_bytes);
    println!("  Duration:      {}s", config.duration_seconds);
    println!("  Stagger:       {}ms", config.stagger_delay_ms);
    println!("  Build:         {}", if is_release { "release" } else { "debug" });
    println!("  Transport:     QUIC (native)");
    if !config.all_start_relays().is_empty() {
        let configs: Vec<_> = config.all_start_relays().iter().map(|r| r.config.as_str()).collect();
        println!("  Managed relays: {}", configs.join(", "));
    }
    let results_dir = resolve_results_dir(&config.results_dir, &config, relay_urls.len());
    println!("  Results:       {}", results_dir);
    println!();

    tokio::fs::create_dir_all(&results_dir).await?;

    // Phase 0: Start managed relays
    let mut relay_manager = RelayManager::new(&results_dir, is_release);
    let all_relay_specs = config.all_start_relays();
    if !all_relay_specs.is_empty() {
        println!("[Phase 0] Starting relays...");
        relay_manager.kill_existing().await;
        relay_manager.build().await?;
        for spec in &all_relay_specs {
            relay_manager.start(&spec.config, &spec.url).await?;
        }
        if all_relay_specs.len() > 1 {
            println!("  Waiting for cluster connections...");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        println!();
    }
    drop(all_relay_specs);

    // Distribute clients across workers (round-robin)
    let client_ids: Vec<String> = (0..config.num_clients)
        .map(|i| format!("client-{}", i))
        .collect();
    let mut client_ids_per_worker: Vec<Vec<String>> = vec![Vec::new(); num_workers];
    for (i, id) in client_ids.iter().enumerate() {
        client_ids_per_worker[i % num_workers].push(id.clone());
    }

    // Phase 1: Spawn workers and connect clients
    println!("[Phase 1] Spawning {} workers and connecting clients...", num_workers);
    let mut handles = WorkerPool::spawn(num_workers, config.tls_disable_verify()).await?;

    let all_connected = WorkerPool::connect_all(
        &mut handles,
        &relay_urls,
        &client_ids_per_worker,
        config.publish_rate_hz,
        config.payload_bytes,
        config.stagger_delay_ms,
    )
    .await?;

    let num_connected = all_connected.len();
    let metrics = Metrics::new();
    metrics.start(num_connected);
    println!("  {}/{} clients connected across {} workers\n", num_connected, config.num_clients, num_workers);

    if num_connected < 2 {
        eprintln!("Need at least 2 connected clients. Aborting.");
        relay_manager.stop_all().await;
        std::process::exit(1);
    }

    // Phase 2: Subscribe (all workers get all peer IDs)
    println!("[Phase 2] Starting subscribers...");
    WorkerPool::subscribe_all(&mut handles, &all_connected).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    println!("  Subscribers waiting for broadcasts\n");

    // Phase 3: Publish
    println!("[Phase 3] Starting publishers...");
    WorkerPool::publish_all(&mut handles).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("  Publishers started\n");

    // Phase 4: Run for duration
    println!(
        "[Phase 4] Running for {}s... (Ctrl+C to stop early)",
        config.duration_seconds
    );
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(config.duration_seconds)) => {}
        _ = tokio::signal::ctrl_c() => {
            println!("\n  Received Ctrl+C, stopping...");
        }
    }

    // Phase 5: Stop, collect metrics, and report
    println!("\n[Phase 5] Stopping workers and collecting metrics...");
    let snapshots = WorkerPool::stop_and_collect(&mut handles).await?;

    // Merge all worker snapshots into the parent metrics
    for snapshot in snapshots {
        metrics.merge(snapshot);
    }

    let stats = report::compute_stats(&metrics);
    report::print_report(&stats);

    let results_config = report::ResultsConfig {
        relay_urls: relay_urls.iter().map(|u| u.to_string()).collect(),
        num_clients: config.num_clients,
        publish_rate_hz: config.publish_rate_hz,
        payload_bytes: config.payload_bytes,
        duration_seconds: config.duration_seconds,
        stagger_delay_ms: config.stagger_delay_ms,
        transport: "QUIC".to_string(),
        num_workers: num_workers,
        num_relays: config.all_start_relays().len().max(relay_urls.len()),
    };
    report::save_results(&results_dir, results_config, stats)?;

    relay_manager.stop_all().await;

    Ok(())
}

/// Original single-process mode (unchanged behavior)
async fn run_single_process(config: Config) -> anyhow::Result<()> {
    let relay_urls = config.client_relay_urls()?;
    let is_release = config.is_release();

    println!("=== MoQ Native QUIC Load Test ===");
    println!("  Relay URLs:    {}", relay_urls.iter().map(|u| u.as_str()).collect::<Vec<_>>().join(", "));
    println!("  Clients:       {}", config.num_clients);
    println!("  Publish rate:  {} Hz", config.publish_rate_hz);
    println!("  Payload:       {} bytes", config.payload_bytes);
    println!("  Duration:      {}s", config.duration_seconds);
    println!("  Stagger:       {}ms", config.stagger_delay_ms);
    println!("  Build:         {}", if is_release { "release" } else { "debug" });
    println!("  Transport:     QUIC (native)");
    if !config.all_start_relays().is_empty() {
        let configs: Vec<_> = config.all_start_relays().iter().map(|r| r.config.as_str()).collect();
        println!("  Managed relays: {}", configs.join(", "));
    }
    let results_dir = resolve_results_dir(&config.results_dir, &config, relay_urls.len());
    println!("  Results:       {}", results_dir);
    println!();

    // Create results directory
    tokio::fs::create_dir_all(&results_dir).await?;

    // Phase 0: Start managed relays
    let mut relay_manager = RelayManager::new(&results_dir, is_release);

    let all_relay_specs = config.all_start_relays();
    if !all_relay_specs.is_empty() {
        println!("[Phase 0] Starting relays...");
        relay_manager.kill_existing().await;
        relay_manager.build().await?;
        for spec in &all_relay_specs {
            relay_manager.start(&spec.config, &spec.url).await?;
        }
        if all_relay_specs.len() > 1 {
            println!("  Waiting for cluster connections...");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        println!();
    }
    // Drop the borrow on config before moving fields out
    drop(all_relay_specs);

    // Initialize QUIC client (single endpoint for all connections)
    let quic_client = config.client.clone().init().context("failed to init QUIC client")?;

    let client_ids: Vec<String> = (0..config.num_clients)
        .map(|i| format!("client-{}", i))
        .collect();

    let metrics = Metrics::new();

    // Phase 1: Connect all clients with stagger
    println!("[Phase 1] Connecting clients...");
    let mut clients: Vec<VirtualClient> = Vec::new();

    for (i, client_id) in client_ids.iter().enumerate() {
        let relay_url = relay_urls[i % relay_urls.len()].clone();
        let mut vc = VirtualClient::new(
            client_id.clone(),
            metrics.clone(),
            config.publish_rate_hz,
            config.payload_bytes,
        );

        match vc.connect(relay_url.clone(), &quic_client).await {
            Ok(()) => {
                println!("  Connected {} -> {}", client_id, relay_url);
                clients.push(vc);
            }
            Err(e) => {
                tracing::error!(client = %client_id, "failed to connect: {:?}", e);
                eprintln!("  Failed to connect {}: {}", client_id, e);
            }
        }

        if i < config.num_clients - 1 && config.stagger_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(config.stagger_delay_ms)).await;
        }
    }

    let connected_ids: Vec<String> = clients.iter().map(|c| c.id.clone()).collect();
    metrics.start(clients.len());
    println!("  {}/{} clients connected\n", clients.len(), config.num_clients);

    if clients.len() < 2 {
        eprintln!("Need at least 2 connected clients. Aborting.");
        relay_manager.stop_all().await;
        std::process::exit(1);
    }

    // Phase 2: Start subscribers first (they'll wait on announced())
    // This must happen BEFORE publishers so announce events aren't missed
    let skip_subscribers = std::env::var("NO_SUBSCRIBERS").is_ok();
    if skip_subscribers {
        println!("[Phase 2] Subscribers SKIPPED (NO_SUBSCRIBERS=1)");
    } else {
        println!("[Phase 2] Starting subscribers...");
        for client in &clients {
            let peer_ids: Vec<String> = connected_ids
                .iter()
                .filter(|id| *id != &client.id)
                .cloned()
                .collect();
            client.start_subscribers(&peer_ids);
        }
        // Brief pause to let subscriber tasks start their announced() listeners
        tokio::time::sleep(Duration::from_millis(100)).await;
        println!("  Subscribers waiting for broadcasts\n");
    }

    // Phase 3: Start publishers (triggers ANNOUNCE to relay → subscribers discover broadcasts)
    println!("[Phase 3] Starting publishers...");
    for client in &mut clients {
        client.start_publisher();
    }
    // Wait for subscribers to discover and subscribe to tracks
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("  Publishers started\n");

    // Phase 4: Run for duration
    println!(
        "[Phase 4] Running for {}s... (Ctrl+C to stop early)",
        config.duration_seconds
    );
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(config.duration_seconds)) => {}
        _ = tokio::signal::ctrl_c() => {
            println!("\n  Received Ctrl+C, stopping...");
        }
    }

    // Phase 5: Stop everything and report
    println!("\n[Phase 5] Stopping...");
    for client in &clients {
        client.stop();
    }
    // Brief wait for tasks to wind down
    tokio::time::sleep(Duration::from_millis(200)).await;

    let stats = report::compute_stats(&metrics);
    report::print_report(&stats);

    let results_config = report::ResultsConfig {
        relay_urls: relay_urls.iter().map(|u| u.to_string()).collect(),
        num_clients: config.num_clients,
        publish_rate_hz: config.publish_rate_hz,
        payload_bytes: config.payload_bytes,
        duration_seconds: config.duration_seconds,
        stagger_delay_ms: config.stagger_delay_ms,
        transport: "QUIC".to_string(),
        num_workers: 1,
        num_relays: config.all_start_relays().len().max(relay_urls.len()),
    };
    report::save_results(&results_dir, results_config, stats)?;

    relay_manager.stop_all().await;

    Ok(())
}
